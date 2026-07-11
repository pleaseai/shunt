# Config hot-reload (SIGHUP + file watch)

## 0. Problem

A shared/enterprise gateway should pick up config edits — a new route, a rotated
API key env, an added client token — without dropping in-flight requests or a
maintenance window. shunt reloads its config file while running, fail-safe: a bad
edit never takes the process down and never leaves it running open.

## 1. Triggers

Two triggers reload the config; both call the same fail-safe path.

- **SIGHUP**: `kill -HUP <pid>` (Unix only). Handy after editing the file in
  place or from a config-management tool.
- **File change (automatic)**: shunt watches the config file's **parent
  directory**, not the file inode, so atomic-rename saves (editors) and
  **Kubernetes ConfigMap** symlink swaps — which replace the file rather than
  write in place — are detected. Events are debounced (400 ms quiet period) so a
  burst of writes coalesces into a single reload. Auto-reload is active only when
  the effective config path is known (a `--config` path or a discovered file);
  running on built-in defaults has nothing to watch.

If the file watcher fails to initialize, shunt logs a warning and keeps running
with SIGHUP-only reload rather than aborting.

## 2. Fail-safe behavior

A reload loads and fully **validates** the candidate config before doing
anything. Only on complete success is the new config swapped in atomically. On
**any** error (missing file, TOML parse error, unknown provider, unresolvable
`[server.auth]` tokens, …) the reload returns the error, logs it, and leaves the
**currently-running config live and unchanged**. There is no window where the
gateway runs a half-applied or open config.

## 3. In-flight request consistency

The live config is held behind an atomic pointer (`arc-swap`). Each request
snapshots the live config on entry and uses that one snapshot for its whole
lifetime, so a reload that lands mid-request never changes config underneath it.
Requests that arrive after the swap see the new config. No locks, no added
per-request latency.

## 4. Fields that require a restart

Most settings apply on reload (routes, providers, models, `[server.auth]`,
`sse_keepalive_seconds`). Two cannot be hot-applied; changing them is accepted
into the reloaded config but only takes effect after a restart, and shunt logs a
`warn!` so the change is not mistaken for live:

- **`server.bind`** — the listener is already bound at the old address.
- **`[sentry]`** — the Sentry client is initialized once before the runtime
  starts and cannot be hot-swapped.

## 5. What reload does not do

- No signal handling on non-Unix platforms (SIGHUP does not exist there); the
  file watcher still works.
- No partial/atomic multi-file config — shunt reloads the single effective file.
