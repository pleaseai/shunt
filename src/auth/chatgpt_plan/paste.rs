//! Hidden-input paste of the callback URL.

use tokio::sync::oneshot;

/// What the paste prompt delivers: the typed line, or why it could not be read.
pub type PasteReceiver = oneshot::Receiver<anyhow::Result<String>>;

/// Read one line on a plain OS thread and hand it over a oneshot.
///
/// Deliberately not `spawn_blocking` or tokio's stdin: those keep the runtime
/// from shutting down while the read is blocked, so the process would hang
/// after the loopback listener already won. A detached thread dies with the
/// process.
pub fn spawn_stdin_reader() -> PasteReceiver {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("chatgpt-plan-paste".into())
        .spawn({
            let sender = sender;
            move || {
                let line = rpassword::prompt_password(
                    "Or paste the full callback URL here (input hidden): ",
                )
                .map_err(anyhow::Error::from);
                let _ = sender.send(line);
            }
        });
    if let Err(error) = spawned {
        // The closure (and its sender) was dropped: the receiver reports a
        // closed channel, which the caller treats as "no paste available".
        tracing::debug!("could not start the paste reader: {error}");
    }
    receiver
}

/// The controlling terminal's settings, captured before the paste prompt can
/// change them and reapplied on drop.
///
/// `rpassword` turns echo off and restores it only when its own guard drops.
/// When the loopback callback wins, that reader thread stays blocked until the
/// process exits, so its guard never drops and the terminal would be left
/// silent. Dropping this once the race is decided puts the settings back. It
/// is a no-op where there is no controlling terminal and on non-unix builds.
pub struct TerminalGuard {
    #[cfg(unix)]
    saved: Option<libc::termios>,
}

impl TerminalGuard {
    #[cfg(unix)]
    pub fn capture() -> Self {
        Self {
            saved: with_tty(|fd| {
                // SAFETY: `termios` is plain data and `fd` is an open tty.
                let mut termios: libc::termios = unsafe { std::mem::zeroed() };
                (unsafe { libc::tcgetattr(fd, &mut termios) } == 0).then_some(termios)
            })
            .flatten(),
        }
    }

    #[cfg(not(unix))]
    pub fn capture() -> Self {
        Self {}
    }
}

#[cfg(unix)]
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(termios) = self.saved {
            // SAFETY: `fd` is an open tty and `termios` came from `tcgetattr`.
            with_tty(|fd| unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) });
        }
    }
}

/// Run `f` with `/dev/tty` open, the device `rpassword` prompts on.
#[cfg(unix)]
fn with_tty<T>(f: impl FnOnce(libc::c_int) -> T) -> Option<T> {
    // SAFETY: a NUL-terminated path; the fd is closed below.
    let fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return None;
    }
    let result = f(fd);
    // SAFETY: `fd` was opened above and is not used afterwards.
    unsafe { libc::close(fd) };
    Some(result)
}
