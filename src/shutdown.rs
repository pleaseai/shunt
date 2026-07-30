//! Graceful-shutdown wiring: waits for SIGTERM/ctrl-c, drains in-flight
//! requests, and arms a second-signal escape hatch. Extracted from
//! `main.rs` to keep that file focused (see `src/AGENTS.md`).

use std::future::Future;

/// Waits for SIGTERM or ctrl-c (SIGINT) and logs which one fired. Passed to
/// `axum::serve(...).with_graceful_shutdown(...)` in `serve` (main.rs) so
/// `brew services stop shunt` — which launchd stops with SIGTERM — drains
/// in-flight requests instead of dropping them.
///
/// The drain has no deadline (an open SSE stream holds the process for as long
/// as its client keeps reading), so before returning this arms a watcher that
/// turns a second signal into an immediate exit, preserving the operator's
/// usual "press ctrl-c again" escape hatch. The [`SignalListener`]s are
/// created once, here, and reused for both waits by moving them into the
/// watcher: tokio delivers each signal through a process-wide channel with no
/// queue, so a signal landing between the first wait's receiver being dropped
/// and a fresh one subscribing for the second wait has no live receiver and
/// is silently discarded, not buffered for the next subscriber. Reusing one
/// listener keeps the subscription continuously live and consumes signals
/// sequentially, so this closes that gap without double-counting the signal
/// that ends the first wait.
pub(crate) async fn shutdown_signal() {
    let mut sigterm = SignalListener::sigterm();
    let mut ctrl_c = SignalListener::ctrl_c();

    let signal = select_shutdown_trigger(sigterm.recv(), ctrl_c.recv()).await;
    tracing::info!(
        signal,
        "shutdown signal received; draining in-flight requests"
    );
    arm_force_exit(
        async move { select_shutdown_trigger(sigterm.recv(), ctrl_c.recv()).await },
        |signal| force_exit(signal),
    );
}

/// Runs `on_signal` on a detached task once `next_signal` resolves. Split out
/// from [`shutdown_signal`] with the reaction injected so tests can prove the
/// watcher is armed and fires without the process actually exiting; only
/// [`force_exit`] itself — a single `std::process::exit` call — stays untested.
fn arm_force_exit<S, F>(next_signal: S, on_signal: F) -> tokio::task::JoinHandle<()>
where
    S: Future<Output = &'static str> + Send + 'static,
    F: FnOnce(&'static str) + Send + 'static,
{
    tokio::spawn(async move {
        let signal = next_signal.await;
        on_signal(signal);
    })
}

/// Ends the process without waiting for the drain, using the 128+SIGINT status
/// shells expect from an interrupted program. This skips the normal return
/// through `run` (main.rs), so the sentry/telemetry guards never flush: an
/// operator signalling twice is asking to stop now, and buffered diagnostics
/// are the cheaper thing to lose.
fn force_exit(signal: &'static str) -> ! {
    tracing::warn!(
        signal,
        "second shutdown signal received; exiting immediately without draining"
    );
    std::process::exit(130);
}

/// Resolves to a label naming whichever of `terminate`/`interrupt` completes
/// first. Split out from [`shutdown_signal`] so tests can drive the selection
/// with instantly-resolving mock futures instead of real OS signals.
async fn select_shutdown_trigger(
    terminate: impl Future<Output = ()>,
    interrupt: impl Future<Output = ()>,
) -> &'static str {
    tokio::select! {
        _ = terminate => "SIGTERM",
        _ = interrupt => "SIGINT",
    }
}

/// A signal listener whose `recv` can be awaited more than once, so
/// [`shutdown_signal`] can reuse a single subscription across its initial
/// wait and the force-exit watcher it arms afterward instead of recreating
/// one per wait. See that function's doc comment for why the reuse matters:
/// recreating the subscription opens a window where an arriving signal has
/// no live receiver and is dropped.
///
/// `tokio::signal::ctrl_c()` isn't used here for the ctrl-c case because it
/// drops its internal listener after one `recv`, making it one-shot; the
/// lower-level `signal`/`windows::ctrl_c` constructors return a listener that
/// keeps working across repeated `recv` calls.
enum SignalListener {
    #[cfg(unix)]
    Unix(tokio::signal::unix::Signal),
    #[cfg(windows)]
    Windows(tokio::signal::windows::CtrlC),
    /// Handler install failed, or (for SIGTERM) the target isn't Unix: pends
    /// forever, matching the previous per-wait fallback.
    Pending,
}

impl SignalListener {
    /// SIGTERM. Unix-only signal, so non-unix targets pend forever here and
    /// rely on [`Self::ctrl_c`] alone. A handler-install failure (rare; e.g.
    /// exhausted signal slots) is logged and then pends rather than aborting
    /// startup, mirroring the SIGHUP setup in `reload::spawn_sighup_task`;
    /// the signal keeps its default disposition, so it kills the process
    /// outright instead of draining.
    fn sigterm() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            match signal(SignalKind::terminate()) {
                Ok(signal) => return Self::Unix(signal),
                Err(error) => tracing::warn!(
                    %error,
                    "failed to install SIGTERM handler; SIGTERM will terminate the process immediately without draining"
                ),
            }
        }
        Self::Pending
    }

    /// ctrl-c (SIGINT on Unix, `CTRL_C_EVENT` on Windows). A handler-install
    /// failure degrades like [`Self::sigterm`]'s: log and pend, so the other
    /// trigger still works instead of refusing to start — but ctrl-c itself
    /// then terminates the process outright instead of draining.
    fn ctrl_c() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            match signal(SignalKind::interrupt()) {
                Ok(signal) => return Self::Unix(signal),
                Err(error) => tracing::warn!(
                    %error,
                    "failed to install ctrl-c handler; ctrl-c will terminate the process immediately without draining"
                ),
            }
        }
        #[cfg(windows)]
        {
            match tokio::signal::windows::ctrl_c() {
                Ok(ctrl_c) => return Self::Windows(ctrl_c),
                Err(error) => tracing::warn!(
                    %error,
                    "failed to install ctrl-c handler; ctrl-c will terminate the process immediately without draining"
                ),
            }
        }
        Self::Pending
    }

    async fn recv(&mut self) {
        match self {
            #[cfg(unix)]
            Self::Unix(signal) => {
                signal.recv().await;
            }
            #[cfg(windows)]
            Self::Windows(ctrl_c) => {
                ctrl_c.recv().await;
            }
            Self::Pending => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn select_shutdown_trigger_reports_sigterm_when_it_fires_first() {
        let label = select_shutdown_trigger(async {}, std::future::pending()).await;
        assert_eq!(label, "SIGTERM");
    }

    #[tokio::test]
    async fn select_shutdown_trigger_reports_sigint_when_it_fires_first() {
        let label = select_shutdown_trigger(std::future::pending(), async {}).await;
        assert_eq!(label, "SIGINT");
    }

    /// A second signal must bypass the drain: `shutdown_signal` arms this
    /// watcher after the first trigger, and firing it is what would call
    /// `std::process::exit`. The reaction is injected here so the assertion
    /// runs in-process.
    #[tokio::test]
    async fn arm_force_exit_reacts_to_the_next_signal() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        arm_force_exit(async { "SIGTERM" }, move |signal| {
            let _ = tx.send(signal);
        })
        .await
        .expect("force-exit watcher task join");

        assert_eq!(
            rx.await.expect("watcher reports the signal it saw"),
            "SIGTERM"
        );
    }

    /// The escape hatch must not short-circuit the drain the first signal
    /// started: with no further signal, the watcher stays parked.
    #[tokio::test]
    async fn arm_force_exit_stays_parked_until_a_second_signal_arrives() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let fired = Arc::new(AtomicBool::new(false));
        let watcher = arm_force_exit(std::future::pending::<&'static str>(), {
            let fired = fired.clone();
            move |_| fired.store(true, Ordering::SeqCst)
        });
        tokio::task::yield_now().await;

        assert!(
            !fired.load(Ordering::SeqCst),
            "watcher must wait for a second signal rather than exiting mid-drain"
        );
        watcher.abort();
    }

    /// Drives `shutdown_signal` against a real, process-delivered SIGTERM
    /// instead of the mock futures the tests above use, covering the actual
    /// `SignalListener::sigterm` registration + delivery path (the ~31
    /// previously-uncovered lines this test was added to close).
    ///
    /// SAFETY: exactly ONE signal is raised below, and this is the ONLY test
    /// in this binary allowed to raise SIGTERM or otherwise exercise a real
    /// `tokio::signal::unix::signal` listener for it. Tokio's signal registry
    /// (`tokio::signal::unix::registry::globals`) is a single process-wide
    /// `OnceLock`, shared by every `#[tokio::test]` runtime in this binary
    /// (each test gets its own `tokio::Runtime`, but they all register with
    /// the same global signal dispatcher) — a second SIGTERM-raising test
    /// running concurrently would race this one's watcher via that shared
    /// state. If you need another real-signal test, serialize it against this
    /// one rather than adding a second independent raise.
    ///
    /// Re SIGTERM firing this test's own second listener (the one
    /// `shutdown_signal` arms via `arm_force_exit` after the first trigger):
    /// it can't. `sigterm` is the SAME `Signal`/`watch::Receiver` object
    /// reused for both waits (see `shutdown_signal`'s doc comment), and per
    /// tokio's `watch::Receiver::changed()` semantics (vendored tokio 1.52.3,
    /// `src/signal/mod.rs:67-88`: `RxFuture::recv` awaits `rx.changed()`,
    /// then immediately re-arms `make_future(rx)` for the *next* change), a
    /// receiver only resolves once per sender version bump. The first
    /// `.recv()` inside `select_shutdown_trigger` consumes this raise and
    /// advances that receiver's cursor; the second `.recv()` inside the
    /// force-exit watcher can only resolve on a genuinely new signal, which
    /// this test never sends. This is the "reuse the same object" design
    /// that avoids the eager-independent-listener misfire a fresh second
    /// subscription would risk.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_signal_completes_on_a_real_sigterm() {
        use std::time::Duration;

        let waiter = tokio::spawn(shutdown_signal());
        // Let `shutdown_signal` run its synchronous prefix (registering both
        // `SignalListener`s) and reach its first await point before raising,
        // so the signal is guaranteed to land on a live, already-subscribed
        // receiver rather than racing the subscription.
        tokio::task::yield_now().await;

        unsafe {
            libc::raise(libc::SIGTERM);
        }

        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("shutdown_signal returns before the test deadline")
            .expect("shutdown_signal task join");
    }
}
