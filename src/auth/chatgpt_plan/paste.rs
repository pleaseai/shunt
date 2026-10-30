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
