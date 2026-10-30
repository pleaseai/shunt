# Optional store — SQLite or PostgreSQL

shunt keeps pool quota, gateway refresh tokens, and spend limits plus their audit
trail in memory, with an optional JSON snapshot per subsystem. The optional
`[store]` table replaces those three snapshots with one transactional database.
Without `[store]` nothing changes: the three snapshots, their `state_path` keys,
and their restore paths run exactly as before.

The decision and its alternatives are in
[ADR-0006](../.please/docs/decisions/0006-optional-pluggable-store.md); the
evaluation that led there is in [`storage.md`](storage.md). This page is the
operator-facing behavior of what shipped in the single-instance phase. The
user-facing key reference is
[`[store]` in the configuration reference](https://shunt.sh/reference/configuration/#store-optional).
The step-by-step move from JSON snapshots to a store is the
[Migrating to the Store](https://shunt.sh/guides/store-migration/) guide.

## What the store holds

| Subsystem | Tables | Store-backed when |
| :-- | :-- | :-- |
| Pool quota (`AccountPool` per-account quota state) | `pool_quota` | `[store]` is set. `[server.pool] state_path`, if any, is only an import source. |
| Gateway refresh tokens (`[server.gateway]` login sessions) | `refresh_token` | `[server.gateway]` is configured |
| Spend limits and audit trail (`[server.spend]`) | `spend_limit`, `spend_audit`, `spend_meta` | `[server.spend]` is configured |

The schema also has `import_record` (what was imported, once) and `instance_hold`
(the single-instance hold). Both engines use identical table and column names;
migrations are embedded in the binary (`migrations/sqlite/`,
`migrations/postgres/`) and applied at boot.

Not in the store: provider credential files (`~/.claude/.credentials.json`-shaped
files and the other provider stores), admin browser sessions, pending provisioning
logins, gateway device grants, and OIDC state. Cooldowns are not persisted, as
before. The request path (`src/proxy*`, `src/adapters/**`) never touches the
store; a source-scan test fails if either ever references `crate::store`.

## Backends

- **SQLite** is always compiled in. `url` is a file path or a `sqlite:` /
  `sqlite://` URL. A plain file path is used as written; the path of a
  `sqlite:` / `sqlite://` URL is percent-decoded, so
  `sqlite:///data/my%20store.db` opens `/data/my store.db`. SQLite `file:` URIs,
  URL query options (`?...`), and an in-memory database are rejected at config
  load. shunt opens exactly one connection in WAL mode with a 5 s busy timeout. A
  missing file is created with mode `0600` before SQLite sees it (so the `-wal`
  and `-shm` sidecars inherit it); an existing file keeps whatever mode the
  operator gave it.
- **PostgreSQL** is compiled in only with the `store-postgres` Cargo feature.
  Prebuilt release binaries are built with it (`--features ui,store-postgres`);
  `cargo build` and `cargo install` are not, so build with
  `--features store-postgres` from source. A build without the feature rejects
  `backend = "postgres"` at config load, naming the feature. The pool defaults
  to 4 connections (`max_connections`) with a 10 s acquire timeout. Every round
  trip a store transaction makes (BEGIN, a statement, COMMIT, ROLLBACK) gives up
  after 15 s, so a connection that goes half-open fails that write instead of
  stalling every write queued behind it; writes queued behind it wait for that
  bounded time. Applying a pending migration at startup is not bounded (the
  schema-version check before it is). A transaction that does not finish with a
  successful COMMIT or ROLLBACK closes its connection instead of returning it to
  the pool, so a dead connection does not keep its pool slot, and the write does
  not wait on a ROLLBACK. The client giving up does not end the server side: a
  backend whose client is gone keeps its transaction's row locks until the
  server notices and reaps it, and until then later writes to those rows keep
  failing at their own bound. A COMMIT that gives up this way has an unknown outcome
  (`CommitUncertain`). The gateway token and spend writers read the store back
  to settle it, waiting at most 5 s (a transaction-local `lock_timeout`) for a
  write still in flight; the pool flush rewrites its state on the next tick
  either way. The bounds are fixed, not configurable.

## Durability classes

| Subsystem | When a change is durable | If the write fails |
| :-- | :-- | :-- |
| Pool quota | Write-behind: a background task flushes changed quota every 15 s, and once more at graceful shutdown | The state stays in memory and is re-flushed on the next tick with the then-current value; the pool keeps serving. Counted in `shunt.store.write_failures{subsystem="pool"}` |
| Spend limits and audit | Synchronous: the admin API call returns only after the write commits | The call returns the usual Anthropic-shaped `500` (`api_error`) and the in-memory state is unchanged. Counted with `subsystem="spend"` |
| Gateway refresh tokens | Synchronous: a token is committed before the response that hands it to the client | See [token endpoint failures](#token-endpoint-failures). Counted with `subsystem="gateway"` |

A hard crash can lose up to the last 15 s of pool quota. Quota is re-derived from
upstream responses, so the cost is a short cold start for the affected accounts.
Spend and gateway tokens do not have that window.

## Boot and shutdown order

`shunt run` opens the store after the router is built and before it serves:

1. Connect and check the schema version. A database migrated by a newer binary
   refuses to boot (`schema version N, newer than the M this binary supports;
   upgrade shunt`) and is not modified.
2. Acquire the [instance hold](#instance-hold).
3. Apply pending migrations.
4. Start the hold heartbeat, so a slow import or restore cannot let the hold go
   stale.
5. [Import](#import-once) the JSON snapshots that have not been imported yet.
6. Restore pool quota, gateway tokens, and spend from the store.
7. Renew the hold once more, refusing to boot if another process took it, and
   start the pool flush task.

Any failure before serving exits non-zero with the reason and releases the hold,
so a corrected restart does not wait out the stale window. On graceful shutdown,
after the HTTP drain, shunt stops its store tasks, writes pending pool quota and
pending gateway revocations once, releases the hold, and closes the database, all
within 5 s. Past that bound the remaining steps are abandoned and the hold
expires on its own. A second signal exits immediately, with no flush.

## Import once

The first boot with `[store]` copies each store-backed subsystem's JSON snapshot
into the store, in one transaction per subsystem together with an `import_record`
row. The snapshots are `[server.pool] state_path`, `[server.gateway] state_path`
(default `~/.shunt/gateway-sessions.json`), and `[server.spend] state_path`
(default `~/.shunt/gateway-spend.json`). The operator procedure, including how to
recover from a first boot that missed a snapshot, is in
[Migrating to the Store](https://shunt.sh/guides/store-migration/).

- The decision is keyed on the `import_record`, not on whether the tables are
  empty. A subsystem that was imported never imports again, even if you later
  delete every row, and a snapshot that appears after a boot with no snapshot is
  ignored.
- A boot with no snapshot (the path is unset or the file is absent) still writes a
  record, with no source path.
- shunt only reads the snapshots. With a store attached it never creates or
  modifies them. After a successful import it writes a small sidecar,
  `<state_path>.imported`, recording the backend, the time, and the snapshot's
  SHA-256.
- A malformed or version-mismatched snapshot fails the boot, naming the subsystem
  and the file. Nothing is imported for that subsystem and no record is written;
  fix or remove the file and restart. Subsystems imported before the failing one
  keep their rows and records.
- A version 2 pool snapshot is normalized to version 3 on the way in, by the same
  code the store-less restore uses.

### Going back to no store

Import is a one-time copy, not a sync. If you later remove `[store]`, shunt starts
from the JSON snapshot as it was **at import time**, which can resurrect sessions
revoked, and undo spend-limit changes made, in the store since then. To make that
visible, a store-less boot that loads a snapshot whose SHA-256 still equals the one
in its `.imported` sidecar logs a warning:

> this snapshot was imported into a store and is unchanged since; running without
> [store] rolls back to that import-time state, which may include sessions revoked
> and spend limits changed in the store since then

Once a store-less run rewrites the snapshot, the hashes differ and the warning
stops, so an operator who accepted the rollback is not told again. shunt does not
export store state back to JSON.

## Instance hold

One store is for one shunt process. A single `instance_hold` row names the
holder; acquiring it is one conditional `UPDATE`, identical on both engines.
Before that `UPDATE`, the opener takes the row's lock: SQLite starts the
transaction with `BEGIN IMMEDIATE`, and PostgreSQL reads the row `FOR UPDATE`,
waiting at most 5 s (`lock_timeout`). The stale check and the takeover warning
then use the row as it is once the lock is held, so a hold that goes stale while
the opener waits for a holder's write is taken over rather than refused.

- The holder label is `{HOSTNAME}:{pid}`: the `HOSTNAME` environment variable,
  else the operating-system hostname, else `unknown-host`.
- A second `shunt run` against a live hold exits non-zero with `store <target> is
  held by another shunt instance (<label>), last renewed Ns ago; only one shunt
  process can use a store at a time`. It is refused before it migrates anything,
  so a newer binary in a rolling deploy cannot change a live holder's schema.
  On PostgreSQL, an opener that cannot get the row lock within 5 s (a holder's
  transaction left open, typically by a backend whose client is gone) fails with
  a database error naming the lock timeout instead.
- The holder renews the hold every `hold_stale_seconds / 3` seconds (at least 1).
  A hold that has not been renewed for more than `hold_stale_seconds` (default
  `30`, minimum `3`) is stale; the next opener takes it over and logs a warning
  naming the previous holder. After a crash, a restart is therefore refused for up
  to `hold_stale_seconds`. The stale check uses the opening process's local clock
  and its own `hold_stale_seconds`, so processes with skewed clocks or differing
  `hold_stale_seconds` values can disagree about when a hold is stale.
- If a heartbeat finds the hold taken, the process logs an error and refuses every
  later durable write, but keeps serving proxied requests. Every durable write
  transaction also re-checks that it still holds the row, so a holder that stalled
  past the window and was taken over cannot write before its next heartbeat.
- Graceful shutdown releases the hold.

This is a guard against two processes sharing a store, not multi-instance
coordination. Running several instances against one store is not supported.

## Token endpoint failures

With a store, the gateway token endpoint (`/oauth/token`) commits the token diff to
the store before it answers. When the write fails:

| Request | Response | In-memory state |
| :-- | :-- | :-- |
| Device-grant token issue | `503` `{"error":"temporarily_unavailable"}` with `Cache-Control: no-store` | Rolled back |
| Refresh-token rotation | `503` `{"error":"temporarily_unavailable"}` with `Cache-Control: no-store` | Rolled back, so the presented refresh token still rotates when the client retries |
| Replay of an already-rotated token | The usual `401` `invalid_grant` | The family stays revoked |

Rolling back an issue or rotation is what keeps the client's retry from looking
like a replay, which would revoke the family it belongs to. A device-grant issue
that fails after the grant was consumed costs that user one re-login: the retried
poll gets `expired_token`, and the login restarts from the device-authorization
step.

A revocation is never rolled back, because undoing it exactly when the store is
degraded would un-revoke a family. The pending revocation is written by the next
successful durable write or by the next 15 s flush-task tick. If the process
crashes before either, the revocation is lost and the family's later tokens are
valid again after restart. This residual is accepted.

Without a store, the token endpoint saves to `[server.gateway] state_path` after
building the response, as before.

## `shunt check`

With `[store]`, `shunt check` prints `config ok`, then opens the store read-only
and prints one line. It never migrates, never modifies the database, and never
creates a missing SQLite file. SQLite may still create or remove the `-wal` and
`-shm` sidecar files while it inspects a WAL database. The inspection is bounded
at 15 s. It reaches the network for a PostgreSQL store, unlike the rest of
`check`.

```text
config ok
store ok: sqlite sqlite:/var/lib/shunt/shunt.db, schema v1 (binary supports v1)
```

| Situation | Output after `config ok` | Exit |
| :-- | :-- | :-- |
| Up to date | `store ok: <backend> <target>, schema vN (binary supports vM)` | 0 |
| Older schema | the same line plus `; 1 pending migration applies at boot` (`; K pending migrations apply at boot` for K > 1) | 0 |
| SQLite file absent | `store ok: sqlite sqlite:<path>, file does not exist yet; it is created with mode 0600 at boot` | 0 |
| Unreachable | stderr `store check failed`, cause `store <target> is unreachable: <reason>` | 1 |
| Schema too new | stderr `store check failed`, cause `store <target> is at schema version N, newer than the M this binary supports; upgrade shunt` | 1 |

`<target>` is `sqlite:<path>` or `postgres://host:port/db`; credentials are never
printed. A `[store]` that fails config validation (for example the TLS rule below)
fails with `config check failed` before the store is contacted.

## PostgreSQL connection rules

- **TLS.** A host that is not `localhost`, a loopback IP, or a unix socket must use
  `sslmode=verify-full`, or config load fails. The rule judges what sqlx parses from
  the URL, so `?host=` and `?hostaddr=` cannot route around it. Setting
  `allow_insecure_remote_tls = true` opts out and logs a warning. sqlx verifies
  against the bundled Mozilla root set, not the operating-system store; a server
  with a private CA needs `sslrootcert=/path/ca.pem` in the URL.
- **Password.** `password_env` names an environment variable holding the password.
  It is read only when the URL carries no password (in the userinfo or as
  `?password=`). If it is needed and the variable is unset or empty, startup fails
  rather than attempting a passwordless connection.
- **Secrecy.** `url` is a redacting secret: logs, errors, and diagnostics show only
  `postgres://host:port/db`.

## Security

> The store holds refresh-token hashes — restrict database access and backups.

The `refresh_token` table holds the SHA-256 of each gateway refresh token plus the
identity it was issued to; raw tokens are not stored. A copy of the database still
reveals who has logged in and which tokens are live, and the spend tables carry the
audit trail's actors. Give the SQLite file the same protection as the snapshot it
replaces (shunt creates it `0600`), restrict PostgreSQL roles to the shunt schema,
and treat backups of either as sensitive.

## Observability

`shunt.store.write_failures` (counter, label `subsystem` = `pool` | `spend` |
`gateway`) counts failed store writes. It is emitted to OpenTelemetry and, with
`[sentry] metrics = true`, to Sentry. A failed write is also logged. See
[`dashboard-metrics.md`](dashboard-metrics.md).

Changing `[store]` on a config reload logs a warning that a restart is required;
the store is opened once at startup.

## Not covered

Request history, Turso/libSQL, multi-instance coordination, moving credential files
into the store, and exporting store state back to JSON are outside this phase.
