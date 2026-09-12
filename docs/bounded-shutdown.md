# Bounded shutdown

Shunt treats shutdown as a process-lifecycle boundary. The first `SIGTERM` or
`SIGINT` permanently stops Axum listener admission and begins graceful drain for
active HTTP responses, SSE streams, and inbound WebSocket sessions. The drain
deadline is `[server] shutdown_timeout_seconds`, which defaults to 30 seconds
and must be between 1 and 3600 inclusive.

The timeout starts when the first signal is observed—not when the process
starts. It is one absolute budget shared by every connection; individual
streams do not receive fresh extensions. Requests that finish within the budget
complete normally, including response-body cleanup and admission-lease release.

If work remains at the deadline, Shunt drops the owned Axum server future and
returns from `run`. The Tokio runtime then cancels remaining connection,
WebSocket, and process-lifetime background tasks; Rust RAII guards release
account-admission slots, response bodies, and subprocess handles. This is a
normal return rather than `process::exit`, so telemetry exporters receive their
usual drop/flush opportunity. Periodic pool-state persistence remains
best-effort; this lifecycle does not promise a new final state transaction.

Cancellation reaches async tasks only. Work already running on a blocking thread
(`tokio::task::spawn_blocking`) cannot be aborted — see `offload::spawn_bounded` —
and simply *dropping* the runtime would wait for it with no deadline at all,
which would put the process back past `shutdown_timeout_seconds` and past the
second-signal escape hatch, whose watcher is itself cancelled once teardown
begins. So teardown is bounded explicitly: already-started blocking work gets a
fixed five-second grace (`BLOCKING_SHUTDOWN_GRACE` in `src/main.rs`), after
which its threads are leaked and the process exits regardless.

The second-signal escape hatch does not cover that final grace. Its watcher is
itself a Tokio task, so it is gone once teardown starts, and Tokio leaves its
signal handler installed after the listener drops — a signal arriving in that
window is captured and discarded rather than falling through to the default
disposition. This is deliberate rather than overlooked: the hatch exists to
escape an *unbounded* wait, and a wait that is already bounded at five seconds
is served by the bound itself. It is also why the grace is a small constant
instead of a configurable value that an operator could raise to a length where
an uninterruptible process would matter.

The two budgets cover different work classes and are deliberately not the same
number. A configured `shutdown_timeout_seconds = 30` means "up to 30 seconds of
draining, then up to 5 seconds for blocking work" — a worst case of 35 seconds,
not a silent 60. The blocking grace is fixed rather than configurable because
this crate's blocking tasks are short, bounded CPU jobs (compression in
`offload`, token counting in `proxy::failover`), not open-ended waits.

On Unix, isolated Antigravity process groups are terminated as soon as the first
signal arrives, because they do not inherit gateway signals and must not pin the
drain. A second `SIGTERM` or `SIGINT` remains the explicit emergency escape
hatch: it exits immediately with status 143 or 130 respectively, terminates the
process groups again, and skips normal telemetry flushing.

`shutdown_timeout_seconds` is captured at boot. A hot reload accepts a changed
value into the configuration snapshot but logs that a restart is required for
the new deadline to take effect.
