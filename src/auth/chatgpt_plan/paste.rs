//! Visible-input paste of the callback URL.
//!
//! The terminal mode is never changed: the pasted URL carries a single-use code
//! that is useless without the PKCE verifier, which never leaves the process,
//! and leaving the mode alone keeps Ctrl-C working normally.

use std::io::Write;

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
                eprint!("Or paste the full callback URL here (it will be visible): ");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let line = match std::io::stdin().read_line(&mut line) {
                    // EOF is an empty line, which the caller treats as no paste.
                    Ok(_) => Ok(line),
                    Err(error) => Err(anyhow::Error::from(error)),
                };
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
