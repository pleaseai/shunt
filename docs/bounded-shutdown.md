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

On Unix, isolated Antigravity process groups are terminated as soon as the first
signal arrives, because they do not inherit gateway signals and must not pin the
drain. A second `SIGTERM` or `SIGINT` remains the explicit emergency escape
hatch: it exits immediately with status 143 or 130 respectively, terminates the
process groups again, and skips normal telemetry flushing.

`shutdown_timeout_seconds` is captured at boot. A hot reload accepts a changed
value into the configuration snapshot but logs that a restart is required for
the new deadline to take effect.
