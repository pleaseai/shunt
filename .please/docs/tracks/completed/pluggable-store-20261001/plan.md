# Plan: Optional pluggable store, Phase 1a (single instance)

> Track: pluggable-store-20261001
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: pluggable-store-20261001
- **Issue**: #773
- **Created**: 2026-10-01
- **Approach**: one concrete `Store` (sqlx; `sqlite` always compiled, `postgres` behind `store-postgres`) attached as an optional handle at the three existing persistence choke points; no `[store]` = today's code path untouched
- **Plan Depth**: deep
- **Planned At**: fafcd4a

## Purpose

Give one shunt process a transactional, optional store for pool quota, gateway
refresh tokens, and spend limits/audit, so the three last-writer-wins JSON
snapshots stop being the only durability path — without changing anything for a
deployment that does not configure `[store]`. Request history is the follow-up
track (`.please/docs/brainstorms/2026-10-01-request-history-requirements.md`).

## Context

### Problem

Pool quota, gateway refresh tokens, and spend limits/audit persist through three
separate whole-file JSON snapshots, each last-writer-wins, with no shared schema,
no transactions, and — for gateway tokens — a save whose failure is swallowed
after the token was already handed out.

### Requirements Summary

Spec user stories 1, 2, 4, 5 (AC-001…AC-006, AC-013/014/016, AC-018…AC-031,
AC-034…AC-036, AC-038/039; FR-001…FR-003, FR-007…FR-011; NFR-001…NFR-003):
optional `[store]` on SQLite or PostgreSQL; synchronous durability for spend and
gateway tokens, write-behind for pool; import-once from JSON; boot-time
migrations, schema guard, and single-instance hold; `shunt check` inspection;
no change without `[store]`.

### Non-Goals

Request history; Turso/libSQL; multi-instance coordination (Phase 2); moving
credential files into the store; exporting store state back to JSON.

### Facts

Decided upstream: [ADR-0006](../../../decisions/0006-optional-pluggable-store.md)
(sqlx for `sqlite` + `postgres`, Turso deferred, optional `[store]`, enforced single
instance). Facts the design rests on, verified in exploration:

- **Pool** (`src/state_persist.rs`): `PersistedPool` v3 (v2 migrates via
  `AccountPool::import_quotas_legacy`); quota only, cooldowns are not persisted;
  `restore()` swallows every error to a cold start; 15 s interval persister on
  `AccountPool::take_dirty/mark_dirty`; **no shutdown flush today**.
- **Gateway tokens** (`src/gateway/{refresh,persist,oauth}.rs`): tokens mutate in
  memory first; `save_if_dirty` runs **after** the response is built
  (`oauth.rs:175`) and swallows failure, so an unpersisted token can be handed out.
  `persist_gate` keeps export+write ordered so a revoked token cannot be resurrected.
- **Spend** (`src/gateway/spend/{store,persist,api}.rs`): already copy-on-write —
  `mutation_gate` → export → pure upsert/delete → `persist::save` → `replace`; a
  failed save returns the Anthropic-shaped 500 from `api.rs::error()`. Opaque
  records round-trip in position (`OpaqueRecord{typed_before,value}`);
  `MAX_AUDIT_RECORDS = 10_000`; `restore` is fail-closed.
- **Boot/shutdown** (`src/main.rs::serve`): restores at :737-746, then
  `axum::serve(..).with_graceful_shutdown`, then `shutdown::await_bounded_drain`; a
  second signal `process::exit`s with no flush. `AppState` growth is visible to
  CodSpeed (required check).
- **`shunt check`** (`main.rs::check`) is sync, offline, outside any runtime;
  `tests/check_cli.rs` pins it.
- **CI**: clippy and tests run with `--all-features`; no `services:` containers
  exist; actions are SHA-pinned; `release.yml` builds `--features ui` for darwin
  and linux-musl.
- `libsqlite3-sys` is 0.30.1 today (via `rusqlite` 0.32, Cursor reader only);
  `sqlx-sqlite` 0.9 accepts `>=0.30.1, <0.38.0`; sqlx 0.9 MSRV 1.94, CI is stable.

### STOP Conditions

- `cargo tree -i libsqlite3-sys` shows two versions, or the musl release targets
  fail to link the bundled SQLite twice → stop and report; the dependency choice in
  ADR-0006 is then wrong.
- Any change would alter `src/atomic_file.rs` semantics or credential-file
  writeback (`src/auth/**`) → stop and ask (AGENTS.md boundary).

## Architecture Decision

**One concrete store, no per-subsystem traits** (ADR-0006 Decision 1 as amended).
The in-memory pool/refresh/spend code is entangled with its JSON snapshot and dirty
flags; a trait would refactor all three for one real second implementation. The
store instead attaches at three choke points — the pool persister, the gateway
token-endpoint save, `spend::persist::save` — and each choke point keeps its
current body when no store is attached.

**Where the handle lives.** `GatewayStores` gains `store: OnceLock<Arc<Store>>`
(`attach_store()` / `store()`); the pool persister receives `Arc<Store>` by
argument. `AppState`, `from_shared`, and `refreshed` do not change — no struct
growth on the CodSpeed-sensitive path.

**Backend dispatch.** `enum Db { Sqlite(SqlitePool), #[cfg(feature =
"store-postgres")] Postgres(PgPool) }`, not `AnyPool`: `AnyConnectOptions` is
URL-only and loses `create_if_missing(false)`, `read_only(true)`, WAL/busy-timeout
tuning, and the pre-created 0600 file. All SQL is written once with `$N`
placeholders in a portable subset (TEXT/BIGINT, `ON CONFLICT … DO UPDATE`, no
RETURNING, no JSONB); the SQLite arm rewrites `$N` → `?N`. SQLite uses exactly one
connection (WAL, `busy_timeout` 5 s), which removes read-then-write upgrade
`SQLITE_BUSY` races. u64 values convert with `i64::try_from` (overflow is an
error, never a wrap).

**Migrations.** `migrations/sqlite/` and `migrations/postgres/`, embedded by two
`sqlx::migrate!` statics (needs sqlx `macros`; add a root `build.rs` with
`cargo:rerun-if-changed=migrations`). `preflight()` reads
`MAX(version) FROM _sqlx_migrations` read-only (missing table = 0) and returns
`SchemaTooNew` before `run()` — `run()` itself writes, so it cannot be the AC-020
check. `Inspect` mode (`shunt check`) uses the same read and never runs.
`Mode::Serve` opens in the order `preflight` → `ensure_hold_table` (`CREATE TABLE
IF NOT EXISTS instance_hold` + seed row via `INSERT … ON CONFLICT DO NOTHING`,
the same DDL 0001 uses with `IF NOT EXISTS`) → acquire hold → `run()`. A process
that is refused the hold, such as a newer binary in a rolling deploy, therefore
never migrates a live holder's schema.

**Schema** (identical DDL in both sets):

| Table | Holds |
| :-- | :-- |
| `import_record(subsystem PK, imported_at, source_path NULL, source_sha256, row_count)` | AC-004/005/029; a no-snapshot boot writes a row with `source_path NULL` |
| `instance_hold(id=1, holder_id, holder_label, started_at, heartbeat_at)` | seeded row; AC-026/027 |
| `pool_quota(account_key PK, quota_json, updated_at)` | `AccountKey` and `QuotaState` as serde JSON text |
| `refresh_token(token_sha256 PK, family, identity_json, inactive_since NULL, issued_at)` + index on `family` | tombstones = `inactive_since` set |
| `spend_limit(ord PK, id, opaque, body)` / `spend_audit(ord PK, audit_id, opaque, body)` / `spend_meta(id=1, next_audit_id)` | TEXT bodies so opaque records round-trip byte-for-byte; `typed_before` recomputed from `ord` on load |

**Instance hold.** Acquire = `UPDATE instance_hold SET holder_id=$1, … WHERE id=1
AND (holder_id IS NULL OR holder_id=$1 OR heartbeat_at < $now_minus_stale)`; one
row = held, zero = `HoldHeld{label, age}`. Heartbeat every `stale/3`; zero rows
on heartbeat sets a `fenced` flag that makes every durable write return `HoldLost`
and stops pool flushes, while the proxy keeps serving. The heartbeat alone leaves
a window: a holder stalled past the stale threshold and taken over would keep
writing until its next tick. So every durable write transaction (spend, gateway
tokens, pool flush, import) starts with `UPDATE instance_hold SET
heartbeat_at=$now WHERE id=1 AND holder_id=$me`; zero rows aborts the transaction
with `HoldLost` and sets `fenced`. Released on graceful
shutdown. Label `{HOSTNAME or "unknown-host"}:{pid}`. Default
`hold_stale_seconds = 30`; a crash blocks a restart for up to that long. The stale check is evaluated by each process against its own clock and its own `hold_stale_seconds`, so processes with clock skew or differing values can take over a live holder early (Phase 1a single-instance limitation).

**Gateway durable path** (new `src/gateway/durable.rs`). With a store: take a
store-owned `tokio::Mutex` (replaces `persist_gate` for this path) → `before =
export()` → mutate → `after = export()` → apply `diff(committed, after)` in one
transaction, where `committed` is the last export the store accepted. On success
`committed = after`. On error the outcome depends on what the mutation did:

- **Adds an active token** (device-grant issue, rotation):
  `refresh_tokens.import(before)` rolls memory back and the endpoint returns
  `oauth_error(503, "temporarily_unavailable")`. Rollback is what keeps a client's
  retry from becoming a replay that revokes its own family. `before` already holds
  any earlier pending revocation, so rollback never undoes one.
- **Only revokes** (a replayed tombstone revokes its family): memory keeps the
  revocation and the endpoint returns the usual 401 `invalid_grant`. Revocation
  fails closed and a store outage never undoes it. Because `committed` did not
  advance, the next durable write, or the 15 s store persister tick, writes the
  pending revocation. A crash before that write loses it; this residual is
  documented in `docs/store.md`.

Without
a store the existing `save_if_dirty` path runs unchanged. Residual: a device code
consumed by `poll` before a failed write costs that user one re-login.

**Import.** Each JSON module splits `load` into `parse(bytes) -> Result<_, LoadIssue>`;
the lenient store-less `load` keeps mapping issues to warn + `None`, a new
`load_strict` returns `Err`. Per subsystem, one transaction writes rows + the
`import_record`; any error rolls back both and boot fails with the subsystem and
path. Pool import goes through a scratch `AccountPool` (`import_quotas[_legacy]` →
`export_quotas`) so v2→v3 normalization is reused, not reimplemented. After commit,
a sidecar `<state_path>.imported` (`{backend, imported_at, source_sha256}`, written
with `atomic_file::write_private_atomic`) records the import without touching the
snapshot; a store-less `restore` warns (AC-031) while the file's sha256 still
matches.

**Which subsystems go to the store.** Pool: always when `[store]` is set (its
`state_path`, if any, is only an import source). Gateway and spend: when
`[server.gateway]` / `[server.spend]` exist — they have nothing to persist
otherwise.

**Config.** `Option<StoreConfig>` next to `sentry`/`otel` with
`#[serde(default, skip_serializing_if = "Option::is_none")]`; `url` is a `Secret`.
Keys: `backend`, `url`, `max_connections` (postgres only), `password_env`,
`allow_insecure_remote_tls` (default false), `hold_stale_seconds` (default 30).
`tls_violation(&PgConnectOptions)` is pure and runs in `validate` and again at
open. It judges the options sqlx parses from the URL (effective host, `hostaddr`,
socket, `ssl_mode`), never the raw string, so `?host=` / `?hostaddr=` cannot
bypass it. A host that is not `localhost`, a loopback IP, or a unix socket
requires `sslmode=verify-full` unless the opt-out is set. The same parsed options
decide whether a password is inline (`?password=` counts) for `password_env`, and
build the sanitized target that `check` and errors print. `reload.rs` warns that `[store]`
changes need a restart.

**Rejected:** `AnyPool` (above); code generic over `sqlx::Database` (trait-bound
maze for ~15 statements); per-subsystem repository traits (above); an always-warn
AC-031 marker (nags forever after the operator accepts the rollback); a same-host
dead-pid fast path for the hold (YAGNI).

## Architecture Diagram

```text
 main.rs::serve ──► store::boot::start ──► open (validate·TLS·0600) ─► preflight ─► ensure hold table
        │                     │            acquire hold ─► migrate ─► import ×3 (txn + record + sidecar)
        │                     │            attach to GatewayStores ─► restore ×3 from store
        │                     └──────────► spawn heartbeat + pool store persister
        │   (no [store]) ──► today's three JSON restores, unchanged
        ▼
  axum::serve ── proxy path (never touches Store)
        │        token endpoint ─► gateway::durable ─► Store (sync)   ─► 503 + rollback on Err (revocations kept)
        │        spend API ─────► spend::persist::save ─► Store (sync) ─► Anthropic 500 on Err
        │        pool persister (15 s) ─► Store (write-behind, retry latest)
        ▼
  await_bounded_drain ─► StoreRuntime::shutdown: stop tasks ─► final pool flush ─► release hold ─► close
```

## Tasks

- [x] T001 Add sqlx dependency, `store-postgres` feature, root `build.rs`, empty `src/store/` (file: Cargo.toml) [US-5]
  Validation: `cargo tree -i libsqlite3-sys` prints exactly one version; `cargo build` and `cargo build --features store-postgres` both succeed; the Cursor reader tests (`auth::observation`) still pass; `release.yml` builds `--features ui,store-postgres`; which root-CA store sqlx's aws-lc TLS uses is recorded in Decision Log. Evidence: command output pasted in Progress
  Method: `cargo tree -i libsqlite3-sys`, `cargo build`, `cargo build --features store-postgres`, `cargo test auth::observation`
  STOP: two `libsqlite3-sys` versions resolve, or enabling `tls-rustls-aws-lc-rs` breaks rustls provider selection (`ensure_crypto_provider`)
- [x] T002 `[store]` config, validation, TLS rule, restart-only reload warning (file: src/store/config.rs) (depends on T001) [US-5, FR-001, FR-009, AC-038, AC-039, NFR-003]
  Validation: `StoreConfig` deserializes the keys listed in Architecture Decision with `url: Secret`; `validate` rejects `:memory:`, `max_connections` on sqlite, `backend = "postgres"` without the `store-postgres` feature (message names the feature), and a non-loopback postgres URL without `sslmode=verify-full` unless `allow_insecure_remote_tls = true` (AC-039); `password_env` is used only when the URL has no password (AC-038); `Debug` and serialization never print the URL; `Config::default()` round-trip unchanged; changing `[store]` on reload logs the restart warning. Evidence: test output
  Method: unit tests in `src/store/config.rs` and `src/reload.rs` — loopback IPv4/IPv6/`localhost`/unix-socket accepted, remote `prefer`/`require`/`verify-ca`/missing rejected, a loopback URL whose `?host=` or `?hostaddr=` points remote rejected, opt-out accepted with warning, env password used vs ignored (including `?password=` counted as inline), Secret redaction
- [x] T003 `Db` layer, open (SQLite pre-create 0600, one connection, WAL), preflight + migrations, `StoreError` (file: src/store/db.rs) (depends on T002) [US-1, FR-007, AC-018, AC-019, AC-020, AC-025, AC-028, NFR-002]
  Validation: `Store::open(cfg, Mode::Serve)` creates a missing SQLite file with mode 0600 and the `-wal`/`-shm` sidecars are 0600 after a write (AC-028); pending migrations apply before return (AC-018); an unreachable store or failing migration returns `StoreError::{Unreachable, Migration}` whose `Display` names the reason and never the password (AC-019/025); a DB whose `_sqlx_migrations` max version exceeds the binary's returns `SchemaTooNew` and the file is byte-identical afterwards (AC-020); `rewrite_placeholders` maps `$1..$N` → `?1..?N` and a test asserts no statement contains a literal `$` elsewhere. Evidence: test output
  Method: `cargo test store::` (tempdir SQLite) — new file, existing file, unwritable dir, injected bad migration, too-new schema, placeholder rewrite
- [x] T004 Postgres in CI and the cross-engine contract harness (file: .github/workflows/ci.yml) (depends on T003) [US-5]
  Validation: `test` and `coverage` jobs run a SHA/tag-pinned `postgres:16-alpine` service with a `pg_isready` health check and set `SHUNT_TEST_POSTGRES_URL` + `SHUNT_TEST_POSTGRES_REQUIRED=1`; the in-crate harness (`src/store/contract.rs`, tests in `src/store/contract/tests.rs`) runs one body per engine, each postgres test in its own schema; with the URL unset locally the postgres arm prints "skipped" and passes, with `REQUIRED=1` and no URL it fails; a parity test proves both migration sets yield identical table/column names. Evidence: CI log showing postgres tests executed (not skipped)
  Method: `cargo test --lib store::contract` locally with and without a postgres URL; push and read the CI log
- [x] T005 Instance hold: acquire, heartbeat, fencing, release (file: src/store/hold.rs) (depends on T003, T004) [FR-011, AC-026, AC-027]
  Validation: `Store::open(Mode::Serve)` runs preflight → `ensure_hold_table` → acquire → migrate, so a second `Store::open` against a live hold returns `HoldHeld` naming the holder label and age (AC-026) and leaves `_sqlx_migrations` unchanged even when the binary carries a newer migration; a hold whose `heartbeat_at` is older than `hold_stale_seconds` is taken over and the previous holder is logged (AC-027); a heartbeat that finds the row taken sets `fenced`, after which durable writes return `HoldLost`; a durable write whose in-transaction `holder_id = $me` check matches zero rows aborts with `HoldLost` and writes nothing, even before the next heartbeat; graceful release clears `holder_id`; the acquire statement is a single conditional UPDATE identical on both engines. Evidence: contract test output on both engines
  Method: contract tests — live holder refused, refused opener with an extra test migration leaves `_sqlx_migrations` unchanged, stale holder taken over, stolen hold fences, row stolen between heartbeats then next write refused, release then re-acquire; plus one binary-level test spawning `CARGO_BIN_EXE_shunt` twice against one SQLite file (second exits non-zero naming the first)
- [x] T006 Pool repo, store persister with retry-latest, shutdown flush, write-failure counter (file: src/state_persist.rs) (depends on T003, T004) [US-4, FR-002, AC-014, AC-024, AC-034, AC-035, AC-036]
  Validation: `save_pool` upserts the exported quotas and deletes absent keys in one transaction and `load_pool` round-trips them; the store persister reuses the 15 s `take_dirty` protocol, and on failure logs (AC-014), increments `shunt.store.write_failures{subsystem="pool"}` (AC-034), re-marks dirty and retries with the then-current export (AC-035) while the pool keeps serving (AC-036); `PoolPersister::flush_now()` writes pending state (AC-024); with no store attached `spawn_state_persister` behaves exactly as today (existing `state_persist::tests` unchanged and green). Evidence: test output
  Method: contract tests with an engine-level failure trigger (SQLite `RAISE(ABORT)` / plpgsql) installed then dropped — fail, mutate again, recover, assert stored state equals the latest export, counter = 1
- [x] T007 Spend repo and the store branch in `spend::persist::save`/`restore` (file: src/gateway/spend/persist.rs) (depends on T003, T004) [AC-003, AC-016]
  Validation: with a store attached, `save` replaces limits, appends only audit records with `id >= before.next_audit_id`, trims to `MAX_AUDIT_RECORDS` (typed + opaque) in the same transaction, and updates `spend_meta`; `restore` reproduces `SpendState` including opaque records at their original positions; a failing write makes `create`/`delete` return the existing Anthropic-shaped 500 and leaves the in-memory state unchanged (AC-003/016); `api.rs` is not modified; existing spend tests unchanged and green. Evidence: test output
  Method: contract tests — opaque-in-position round trip, 10 001st audit record trims the oldest, injected failure on create and delete, no-op upsert issues no write
- [x] T008 Gateway durable token path with rollback, sessions repo, `oauth.rs` wiring (file: src/gateway/durable.rs) (depends on T003, T004) [AC-023]
  Validation: with a store attached, device-grant issue and refresh rotation commit the token diff before the response (AC-023); on a failed write the endpoint returns `503 {"error":"temporarily_unavailable"}` with `Cache-Control: no-store`, the in-memory store is rolled back so the presented refresh token still rotates on retry; a replayed tombstone while the store is failing returns 401 `invalid_grant`, keeps its family revoked in memory (no rollback), and the revocation is in the store after the next successful durable write or persister tick; concurrent rotations cannot resurrect a revoked token; without a store, `oauth.rs` behaves exactly as today (`src/gateway/tests.rs` `state_path_keeps_refresh_sessions_across_a_restart` green). Evidence: contract test output on both engines
  Method: store-backed cases run once per engine through the T004 harness (postgres arm required in CI) — rotate ok, rotate with injected failure then retry succeeds, replay during failure returns 401 and stays revoked after recovery and restart, 32 concurrent rotations of one token yield exactly one 200 and 31 `invalid_grant`, after which the family has no active token in memory or the store (replay detection revokes the winner; see Decision Log 2026-10-17); the store-less regression stays in `src/gateway/tests.rs`
- [x] T009 `store::boot` start/shutdown, `main.rs::serve` wiring, AC-002/022/013 regression guards (file: src/store/boot.rs) (depends on T005, T006, T007, T008, T010) [US-1, AC-001, AC-002, AC-013, AC-022, NFR-001, SC-001, SC-004]
  Validation: `serve` calls `store::boot::start` after `build_router` and runs today's three JSON restores only when it returns `None`; boot order is open (preflight → hold → migrate) → imports (T010) → attach → restore ×3 → spawn heartbeat + persister; any `StoreError` exits non-zero with the reason (AC-019/025); `StoreRuntime::shutdown(5s)` runs after `await_bounded_drain` on both drained and timed-out paths, in order stop tasks → `flush_now` → release hold → close (AC-024); with a store, no JSON snapshot is created or modified (AC-002); with no `[store]`, the full existing suite is green unchanged (AC-022); a source-scan test asserts nothing under `src/proxy*` or `src/adapters/**` references `crate::store` (AC-013/NFR-001); with the store closed mid-run, proxied requests still succeed (SC-004); with a test-only delay that holds every store statement for 3 s while the pool persister is flushing, each of 50 proxied requests completes in under 1 s (a request that awaited the store could not), which measures NFR-001. Evidence: test output on both engines
  Method: integration tests driving the binary, run once per engine (postgres arm required in CI) — restart keeps pool/sessions/spend (AC-001, SC-001), three `state_path`s set + store → files byte-identical, store closed then 50 proxied requests all 200, 3 s injected store delay then 50 proxied requests each under 1 s, unreachable postgres URL → exit code ≠ 0 and stderr names the host
- [x] T010 Strict loaders, import-once transactions, import records, `.imported` sidecar and store-less warning (file: src/store/import.rs) (depends on T006, T007, T008) [US-2, FR-003, AC-004, AC-005, AC-006, AC-029, AC-030, AC-031, SC-002]
  Validation: an absent `import_record` + present snapshot imports rows and record in one transaction (AC-004); a present record never re-imports even when the subsystem's tables are empty (AC-005); the snapshot is byte-identical after import (AC-006); a malformed or version-mismatched snapshot leaves no rows and no record (AC-029) and boot exits naming the subsystem and path (AC-030); a no-snapshot boot writes a `source_path NULL` record; a v2 pool snapshot imports with v3 normalization; after import, a store-less boot that loads the unchanged snapshot logs the AC-031 warning and stays silent once the snapshot's sha256 changes; the lenient store-less loaders behave as today. Evidence: contract test output on both engines
  Method: store-side cases run once per engine through the T004 harness — v2 and v3 pool fixtures, gateway and spend fixtures, malformed JSON per subsystem, cleared-data-no-reimport, late-appearing snapshot ignored, sidecar warn/no-warn
- [x] T011 `shunt check` store inspection (file: src/main.rs) (depends on T003) [FR-008, AC-021, SC-005]
  Validation: with `[store]`, `check` builds a runtime, opens in `Mode::Inspect`, prints one line `store ok: <backend> <sanitized target>, schema vN (binary supports vM)` plus "K pending migrations apply at boot" when older, and exits non-zero when unreachable or schema-too-new (AC-021/SC-005); it never runs migrations, never modifies the database contents (SQLite is opened read-only, though SQLite may create or remove the `-wal`/`-shm` sidecar files while it inspects a WAL database), and never creates a missing SQLite file (prints that it will be created with 0600 at boot); the output never contains the password; with no `[store]`, `tests/check_cli.rs` is unchanged and green. Evidence: test output
  Method: `tests/check_cli.rs` additions — SQLite present/absent/too-new, postgres reachable/unreachable (contract-gated), assert file absence and byte-identity after `check`
- [x] T012 Documentation across README, docs, site (4 locales), example config (file: site/src/content/docs/reference/configuration.md) (depends on T009, T010, T011) [FR-010]
  Validation: `[store]` (keys, defaults, both backends, `store-postgres` feature, TLS rule and opt-out, `password_env`), the durability classes, import-once and the import-time nature of a rollback, the instance hold and stale takeover, the token-endpoint failure behavior (503 for issue and rotation, 401 with a pending store write for a replay revocation, lost if the process crashes first), `shunt check` output, and "the store holds refresh-token hashes — restrict database access and backups" appear in `README.md` + `README.{ko,ja,zh-CN}.md`, `site/src/content/docs/{,ko/,ja/,zh-cn/}reference/configuration.md`, a new `docs/store.md`, the topology table in `docs/admin-ui-delivery.md`, and `docs/storage.md`; `shunt.toml.example` gains a commented `[store]`; every claim is grep-checked against the code; locale pages link only anchors that exist in that locale's built output. Evidence: per-surface checklist in the PR body and `site` build output
  Method: `cd site && bun run build`, grep each documented key/default against `src/store/config.rs`, check locale anchors in `site/dist/<locale>/`

## Dependencies

T001 → T002 → T003 → {T004, T011} ; T004 → {T005, T006, T007, T008} ; {T006–T008} → T010 ; {T005–T008, T010} → T009 ; {T009, T010, T011} → T012. T005–T008 depend on T004 so their contract tests exist in one harness and run on both engines in CI.

## Key Files

- **Create**: `src/store/{mod,config,error,db,open,migrate,hold,pool_repo,sessions_repo,spend_repo,import,marker,boot}.rs`, `src/gateway/durable.rs`, `migrations/{sqlite,postgres}/0001_init.sql`, `build.rs`, `src/store/contract.rs`, `src/store/contract/tests.rs`, `docs/store.md`
- **Modify**: `Cargo.toml`, `src/config.rs` (`Config.store`), `src/reload.rs`, `src/main.rs` (`serve`, `check`), `src/state_persist.rs`, `src/gateway/store.rs` (`GatewayStores.store`), `src/gateway/oauth.rs`, `src/gateway/persist.rs` (strict parse), `src/gateway/spend/persist.rs`, `src/metrics.rs` (`record_store_write_failure`), `.github/workflows/{ci,release}.yml`, `tests/check_cli.rs`, docs surfaces in T012
- **Reuse, do not modify**: `src/atomic_file.rs::write_private_atomic`, `AccountPool::{import_quotas, import_quotas_legacy, export_quotas, take_dirty, mark_dirty}`, `RefreshTokenStore::{export, import}`, `SpendStore::{export, mutation_gate, replace}`, `spend::persist::interleave_records`, `main.rs::{runtime, block_on_bounded}`, `shutdown::await_bounded_drain`, `oauth.rs::{oauth_error, no_store}`

## Verification

### Automated Tests

- [x] `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features --workspace` green locally (SQLite) and in CI (SQLite + PostgreSQL, postgres arm required not skipped).
  Evidence (2026-10-17, head c5180e46): CI run `CI` success; local `cargo test --features store-postgres,bench --workspace --no-fail-fast` with `SHUNT_TEST_POSTGRES_REQUIRED=1` -> 3952 passed, 2 failed (the known `tests/responses_chain_stream.rs` local failures, identical on pristine HEAD), 2 ignored; fmt and `clippy --all-targets --features store-postgres,bench -D warnings` clean.
- [x] `cargo test --test router_surface --test prefill_router` (default features) green.
  Evidence: CI step `Test default build (no ui feature)` green on c5180e46; `RUSTFLAGS="-D warnings" cargo check --lib` clean locally.
- [x] NFR-001: with every store statement delayed 3 s, each proxied request completes in under 1 s (T009), on both engines.
  Evidence: T009 `store_boot` -> slowest of 50 proxied requests 2.2 ms (sqlite) / 2.4 ms (postgres) while the pool write was held >= 3 s.
- [x] Coverage job includes the postgres service; Sonar new-code coverage ≥ 80 %.
  Evidence: SonarCloud quality gate OK for PR #774, `new_coverage` 93.6, `new_duplicated_lines_density` 0.2.
- [x] CodSpeed: no `AppState` growth; any delta on `main` comparison is checked against the AGENTS.md CodSpeed procedure before being treated as a regression.
  Evidence: `CodSpeed Performance Analysis` pass on c5180e46 (run 39005568806) and on every earlier head of PR #774.

### Manual Testing

- Run `shunt run` with a SQLite `[store]` and an existing `~/.shunt/gateway-sessions.json`, log Claude Code in through the gateway, restart, confirm no re-login; start a second `shunt run` on the same file and confirm it refuses naming the first.

## Test Scenarios

### T008

- Happy: device grant approved → token issued → row present before response.
- Error: store write fails on rotate → 503 `temporarily_unavailable` → same refresh token rotates successfully once the store recovers (no family revocation).
- Error: replayed tombstone while the store is failing → 401 `invalid_grant`, family stays revoked in memory (revoke-only diffs are never rolled back); once the store recovers the next durable write persists the revocation, and after a restart the family's descendants are still denied.
- Integration: 32 concurrent rotations of one token → exactly one 200, 31 × 401 `invalid_grant`, the winner revoked by replay detection (no active token left), store matches memory.

### T010

- Edge: subsystem imported, then all spend limits deleted through the API, then restart with the old JSON still present → no re-import, limits stay empty.
- Edge: first boot with no snapshot, JSON file created later by hand → never imported.
- Error: truncated gateway snapshot → boot fails naming `gateway` and the path; store has no `refresh_token` rows and no `gateway` record.

## Progress

- [x] (2026-10-16 19:40 KST) T001 Add sqlx dependency, `store-postgres` feature, root `build.rs`, empty `src/store/`
  Evidence: `cargo tree -i libsqlite3-sys` → one version, `libsqlite3-sys v0.30.1`, reached from both `rusqlite v0.32.1` and `sqlx-sqlite v0.9.0`; `cargo build` and `cargo build --features store-postgres` → `Finished`; `cargo test --lib auth::observation` → 15 passed; `cargo fmt --all --check` → exit 0; `cargo clippy --all-targets --features bench -- -D warnings` → clean; `cargo test --features store-postgres` → lib 3017 passed, only `tests/responses_chain_stream.rs` fails (3 tests, identical on pristine HEAD)
- [x] (2026-10-16 21:00 KST) T002 `[store]` config, validation, TLS rule, restart-only reload warning
  Evidence: `cargo test --lib store::config` → 11 passed; `cargo test --lib --features store-postgres store::config` → 20 passed; `cargo test --lib reload::` → 26 passed (2 new `[store]` tests); `cargo test --lib --features store-postgres` → 3039 passed; `cargo fmt --all --check` → exit 0; `cargo clippy --all-targets --features store-postgres,bench -- -D warnings` → clean; default-build clippy fails only on the pre-existing `src/routing/stage/store.rs:50` unused import
- [x] (2026-10-16 21:40 KST) T003 `Db` layer, open (SQLite pre-create 0600, one connection, WAL), preflight + migrations, `StoreError`
  Evidence: `cargo test --lib store::` → 146 passed (17 new in store::db / store::open); `cargo test --lib --features store-postgres store::` → 155 passed; `cargo fmt --all --check` → exit 0; `cargo clippy --all-targets --features store-postgres,bench -- -D warnings` → clean
- [x] (2026-10-16 20:58 KST) T004 Postgres in CI and the cross-engine contract harness
  Evidence: `SHUNT_TEST_POSTGRES_URL=postgres://…@localhost:55432/postgres SHUNT_TEST_POSTGRES_REQUIRED=1 cargo test --lib --features store-postgres store::` → 165 passed (3 runs), 0 `contract_%` schemas left; URL unset → 10 passed, 5 × `skipped: SHUNT_TEST_POSTGRES_URL unset`; `REQUIRED=1` with no URL → 5 postgres tests FAILED (`SHUNT_TEST_POSTGRES_REQUIRED=1 but SHUNT_TEST_POSTGRES_URL is unset`); default features → 5 sqlite tests pass; CI run 38896134846 → `store::contract::tests::*::postgres ... ok` and the parity test ok in both `fmt · clippy · test` and `coverage` jobs, 0 skipped lines. The run is red only on `Test default build (no ui feature)`: `constant POSTGRES_ACQUIRE_TIMEOUT / DEFAULT_POSTGRES_CONNECTIONS is never used` in `src/store/open.rs` (from T003, 691d009c; not fixed here)
- [x] (2026-10-16 22:30 KST) T005 Instance hold: acquire, heartbeat, fencing, release
  Evidence: `SHUNT_TEST_POSTGRES_URL=postgres://…@localhost:55432/shunt_test SHUNT_TEST_POSTGRES_REQUIRED=1 cargo test --lib --features store-postgres store::` → 184 passed (9 hold contract tests on each engine + `inspect_never_touches_the_hold`); `cargo test --lib store::` (default features) → 161 passed; `cargo fmt --all --check` → exit 0; `cargo clippy --all-targets --features store-postgres,bench -- -D warnings` → clean; `RUSTFLAGS="-D warnings" cargo check --lib` → clean
  Deferred: binary-level two-process test (spawn `CARGO_BIN_EXE_shunt` twice against one SQLite file; second exits non-zero naming the first) — needs `serve` wired to the store, so it moves to T009
- [x] (2026-10-16 23:30 KST) T006 Pool repo, store persister with retry-latest, shutdown flush, write-failure counter
  Evidence: `SHUNT_TEST_POSTGRES_URL=postgres://…@localhost:55432/shunt_test SHUNT_TEST_POSTGRES_REQUIRED=1 cargo test --lib --features store-postgres -- state_persist:: store:: metrics::` → 275 passed (7 persister + 4 pool_repo contract tests on each engine, existing `state_persist::tests` unchanged); `cargo test --lib -- state_persist:: store:: metrics::` (default features) → 242 passed; `cargo fmt --all --check` → exit 0; `cargo clippy --all-targets --features store-postgres,bench -- -D warnings` → clean; `RUSTFLAGS="-D warnings" cargo check --lib` → clean
- [x] (2026-10-16 23:55 KST) T007 Spend repo and the store branch in `spend::persist::save`/`restore`
  Evidence: `cargo test --lib --features store-postgres -- gateway:: store:: state_persist::` (Postgres 55432) -> 452 passed; default build `cargo test --lib -- gateway:: store::` -> 388 passed; fmt, clippy (store-postgres,bench) and `RUSTFLAGS=-D warnings cargo check --lib` clean
- [x] (2026-10-17 00:40 KST) T008 Gateway durable token path with rollback, sessions repo, `oauth.rs` wiring
  Evidence: `cargo test --lib --features store-postgres -- gateway:: store:: state_persist::` (Postgres 55432, REQUIRED=1) -> 496 passed (12 `gateway::tests::store_backed` + 7 `sessions_repo` + 2 persister session cases, each on both engines; `state_path_keeps_refresh_sessions_across_a_restart` unchanged and green); default build -> 431 passed; fmt, clippy (store-postgres,bench) and `RUSTFLAGS=-D warnings cargo check --lib` clean; mutation (rollback disabled) -> 4 store_backed tests FAILED, restored
- [x] (2026-10-17 02:10 KST) T010 Strict loaders, import-once transactions, import records, `.imported` sidecar and store-less warning
  Evidence: `SHUNT_TEST_POSTGRES_URL=postgres://shunt:...@localhost:55432/shunt_test SHUNT_TEST_POSTGRES_REQUIRED=1 cargo test --lib --features store-postgres -- gateway:: store:: state_persist::` -> 530 passed (12 `store::import::tests` contract cases on each engine + `sources_follow_the_config_rules` + 6 `store::marker` + 3 store-less warn/no-warn cases; existing state_persist/gateway/spend tests unchanged); default build `cargo test --lib -- gateway:: store:: state_persist::` -> 453 passed; fmt, `clippy --all-targets --features store-postgres,bench -D warnings` and `RUSTFLAGS="-D warnings" cargo check --lib` clean
- [x] (2026-10-17 01:15 KST) T009 `store::boot` start/shutdown, `main.rs::serve` wiring, AC-002/022/013 regression guards
  Evidence: `SHUNT_TEST_POSTGRES_URL=postgres://shunt:...@localhost:55432/shunt_test SHUNT_TEST_POSTGRES_REQUIRED=1 cargo test --features store-postgres --test store_boot` -> 13 passed (6 cases x {sqlite, postgres} + unreachable-postgres; NFR-001 slowest of 50 proxied requests 2.2 ms / 2.4 ms while the pool write was held >= 3 s); `cargo test --lib --features store-postgres store::boot` -> 5 passed (source scan over 53 files, matcher positive/negative twins, failed start releases the hold, shutdown releases the hold); AC-022 `cargo test --workspace --features store-postgres,bench --no-fail-fast` (no `[store]` anywhere but store_boot) -> 3928 passed, 3 failed, 2 ignored; the 3 are the known `tests/responses_chain_stream.rs` local failures; mutation: dropping `store_runtime.shutdown` from `serve` fails `restart_keeps_pool_sessions_and_spend::sqlite` (pool_quota 0 rows); fmt, `clippy --all-targets --features store-postgres,bench -D warnings`, `RUSTFLAGS="-D warnings" cargo check --lib` and default-feature `cargo test --no-run --test router_surface --test prefill_router --test store_boot` clean
- [x] (2026-10-17 02:40 KST) T011 `shunt check` store inspection
  Evidence: `SHUNT_TEST_POSTGRES_URL=postgres://shunt:...@localhost:55432/shunt_test SHUNT_TEST_POSTGRES_REQUIRED=1 cargo test --features store-postgres --test check_cli` -> 11 passed (5 existing unchanged + 4 sqlite: current / unmigrated / absent / too-new, each asserting file bytes+mtime unchanged or file still absent; 2 postgres: reachable, unreachable, neither stream contains the password); default build `cargo test --test check_cli` -> 9 passed; `cargo test --lib --features store-postgres store::` -> 262 passed; fmt, `clippy --all-targets --features store-postgres,bench -D warnings`, `RUSTFLAGS="-D warnings" cargo check --lib` and `cargo test --no-run --test check_cli` (default features) clean
- [x] (2026-10-17 03:30 KST) T012 Documentation across README, docs, site (4 locales), example config
  Evidence: `cd site && bun run build` -> exit 0, `209 page(s) built`, Pagefind `Indexed 205 pages`; built-dist anchors present: `id="store-optional"` (en), `store-선택` (ko), `storeオプション` (ja), `store可选` (zh-cn), `shunt-check` in all four cli pages, every cross-link to them resolves; live `target/debug/shunt check` (store-postgres build) printed the documented absent-file, current-schema, unreachable-postgres and remote-TLS-rejected lines

## Decision Log

- 2026-10-01: Split request history into its own track (task count > 12; history recording spans six call sites with the account mostly absent). Seed: `.please/docs/brainstorms/2026-10-01-request-history-requirements.md`.
- 2026-10-01: No per-subsystem repository traits; one concrete store at three choke points. ADR-0006 Decision 1 amended accordingly.
- 2026-10-01: Import keyed on `import_record`, not table emptiness; no-snapshot boot still writes a record. ADR-0006 Decision 3 amended.
- 2026-10-01: AC-031 interpreted as "warn while the loaded snapshot is still byte-identical to what was imported" (sidecar + sha256), not an unconditional warning.
- 2026-10-01: AC-023 includes rolling back the in-memory mutation on a failed write; otherwise a client's retry is a replay that revokes its own family.
- 2026-10-01 (plan review): Rollback applies only to mutations that add an active token. A revoke-only mutation (replay) is kept in memory, answered with 401 `invalid_grant`, and written by the next durable write, because rolling it back would un-revoke a family exactly when the store is degraded.
- 2026-10-01 (plan review): Every durable write checks `holder_id = $me` inside its own transaction; heartbeat-only fencing left a stale-writer window after a takeover.
- 2026-10-01 (plan review): The hold is acquired before migrations run, through an idempotent `ensure_hold_table`, so a refused opener never changes a live holder's schema.
- 2026-10-01 (plan review): The PostgreSQL TLS and password rules judge sqlx's parsed `PgConnectOptions`, not the raw URL.
- 2026-10-01: Gateway and spend are store-backed only when `[server.gateway]` / `[server.spend]` exist; the pool is store-backed whenever `[store]` is set.

- 2026-10-16: sqlx's rustls TLS (`tls-rustls-aws-lc-rs` -> `_tls-rustls-aws-lc-rs` = `_tls-rustls` + `rustls/aws-lc-rs` + `webpki-roots`) trusts the bundled Mozilla `webpki-roots` store, not the OS store: `import_root_certs` in sqlx-core 0.9.0 `src/net/tls/tls_rustls.rs` builds `RootCertStore` from `webpki_roots::TLS_SERVER_ROOTS` unless `rustls-native-certs` is on, which only `_tls-rustls-ring-native-roots` enables. It also builds its own `aws_lc_rs::default_provider()` via `builder_with_provider`, so it never reads the process-wide default that `ensure_crypto_provider` installs. A private-CA Postgres server therefore needs `sslrootcert` (not the OS trust store).
  Date/Author: 2026-10-16 / sol-implementer

- 2026-10-16: sqlx-postgres 0.9.0 has no password getter and seeds `password` from `PGPASSWORD`/`.pgpass` (`options/mod.rs` `new_without_pgpass`, `apply_pgpass`), so "inline password" is decided by parsing the URL userinfo and `?password=` with the `url` crate (`store::config::url_has_inline_password`). The TLS rule uses `get_host()`/`get_socket()`/`get_ssl_mode()`: `?host=` and `?hostaddr=` both overwrite `host` via `.host()` (`options/parse.rs:69-86`), `/`-prefixed hosts set `socket`, which `fetch_socket` prefers (`options/mod.rs:457`). The opt-out warning is logged from `StoreConfig::validate`, which re-runs on reload (the sentry/otel precedent avoids that); the repeat is accepted because it only fires while the operator keeps the opt-out set.
  Date/Author: 2026-10-16 / claude-worker

- 2026-10-16: `Store::open(.., Mode::Inspect)` does not run the preflight and returns `Ok` for a too-new schema; `shunt check` calls `Store::check_schema()` (returns `SchemaTooNew`) so it can still print "schema vN (binary supports vM)". `Mode::Serve` runs preflight inside `open`. The "byte-identical" guarantee for Inspect holds for the database file; SQLite may create or remove `-shm`/`-wal` bookkeeping for a WAL database even read-only.
  Date/Author: 2026-10-16 / claude-worker

- 2026-10-16: The T004 harness is in-crate (`src/store/contract.rs`, tests in `src/store/contract/tests.rs`, run with `cargo test --lib store::contract`), not `tests/store_contract.rs`. T005-T008 test crate-private repo code (`state_persist`, `gateway::spend::persist`, `gateway::durable`) that an integration test cannot reach. Gate: the `postgres` arm is `#[cfg(feature = "store-postgres")]`; the sqlite arm always runs. Contract tests are written with `store_contract_test!(name, |store| { ... })`, which expands to `mod name { sqlite, postgres }`.
  Date/Author: 2026-10-16 / claude-worker
- 2026-10-16: Postgres isolation is one `contract_<uuid>` schema per test, selected by appending `options=-c search_path=<schema>` to the URL, so `Store::open` migrations and `_sqlx_migrations` land inside it; the schema is dropped (best effort) by `Harness::finish`. A panicking body leaves its schema behind. CI pins `postgres:16-alpine@sha256:721873c3...` (the multi-arch index digest).
  Date/Author: 2026-10-16 / claude-worker
- 2026-10-16: `test_support::TempDir` gained an atomic counter in its directory name: macOS clock ticks are microseconds, so two parallel contract tests got the same directory (`table import_record already exists`).
  Date/Author: 2026-10-16 / claude-worker
- 2026-10-16: The `CARGO_BIN_EXE_shunt` two-process hold test moves from T005 to T009: `main.rs::serve` is not wired to the store until T009, so there is no binary-level entry point to refuse the second process. T005 covers the same behavior in-process with a second `Store::open` against the same database.
  Date/Author: 2026-10-16 / claude-worker
- 2026-10-16: `spawn_state_persister(state, store: Option<Arc<Store>>) -> PoolPersister`; the file path is unchanged when `store` is `None`. `PoolPersister` is not abort-on-drop (the default deployment drops it in `main`); `stop(self).await` aborts and joins. The store sink serializes flushes with a `tokio::sync::Mutex`, so an older export cannot land after a newer one when the timer and `flush_now` overlap.
  Date/Author: 2026-10-16 / claude-worker
- 2026-10-16: `HoldLost` halts pool flushing: it is counted on `shunt.store.write_failures{subsystem="pool"}`, logged once at ERROR, not re-marked dirty, and later `flush_now` calls return `HoldLost` without touching the database. Every other failure is counted, logged at WARN and re-marked dirty.
  Date/Author: 2026-10-16 / claude-worker
- 2026-10-16: The write-failure counter test hook is a per-thread `thread_local` (`metrics::store_write_failures_for_tests`), not a process-wide map: `#[tokio::test]` runs on the test thread, so the sqlite and postgres arms of one test never share a count.
  Date/Author: 2026-10-16 / claude-worker
- Decision: `GatewayStores` carries the store handle as `OnceLock<Arc<Store>>` (`attach_store`/`store`); `AppState`, `from_shared` and `refreshed` are unchanged.
  Rationale: `refreshed()` clones the `Arc<GatewayStores>`, so the attached store survives every refresh with no `AppState` growth (CodSpeed). `GatewayStores::new` is only called at `AppState::new` and `build_router`.
  Date/Author: 2026-10-16 / claude-worker
- Decision: for `spend_audit`, a record is "new" when it is typed and `id >= before.next_audit_id`; opaque audit records are never appended after the first save (they come only from a restore, mutations only append typed ones). On the first save (no `spend_meta` row) every audit record, opaque included, is written. A trim removes the oldest rows by `ord`, typed and opaque alike, down to `MAX_AUDIT_RECORDS`, which equals the in-memory `trim_audit_records` result because both drop from the front of the combined sequence (verified by the 10 001st-record and opaque-trim contract tests).
  Rationale: the plan's `id >= before.next_audit_id` rule cannot classify opaque rows; ids inside them are untrusted.
  Date/Author: 2026-10-16 / claude-worker
- Decision: `save_spend` takes `before` from `SpendStore::export()` inside `persist::save`; `api.rs` holds `mutation_gate` and calls `replace` only after `save` returns, so the in-memory state is exactly `before`.
  Rationale: keeps `api.rs` unmodified while giving the repo a transition rather than a snapshot.
  Date/Author: 2026-10-16 / claude-worker

- 2026-10-17: Gateway durable path state lives in `GatewayStores.durable_sessions` (`gateway::durable::DurableSessions`: a `tokio::sync::Mutex<HashMap<token_sha256, RefreshRecord>>` whose guarded value is `committed`), not in AppState. It starts empty (correct for an empty store) and `durable::restore_from_store` sets it to the rows *loaded* (before the import sweep), so the first `flush_pending` also deletes rows that expired while down. The lock spans `export -> mutate -> export -> write`; "adds an active token" is `adds_active(before, after)` (an active hash in `after` that `before` lacks). An empty diff issues no statement (no `begin_durable`).
  Date/Author: 2026-10-17 / sol-implementer
- 2026-10-17: The store persister (`PoolPersister`) also calls `gateway::durable::flush_pending` on every tick and in `flush_now`, using the gateway-attached store (no-op without one). Alternative considered: a second gateway-only task; rejected because it would duplicate the interval/stop/shutdown plumbing T009 already wires for the pool persister. Pool behavior and tests are unchanged; a `flush_now` error is the pool error, else the sessions error.
  Date/Author: 2026-10-17 / sol-implementer
- 2026-10-17: The plan's "32 concurrent rotations of one token leave exactly one active descendant" cannot hold with the unchanged `RefreshTokenStore`: the first rotation succeeds, and each of the other 31 is a replay of a tombstone, which revokes the whole family including the new token. The test asserts the achievable form: exactly one 200, 31 x 401, store == memory, and zero active tokens afterwards (the descendant is revoked, never resurrected). Making a concurrent loser not revoke the winner would change store-less behavior, which the task forbids.
  Date/Author: 2026-10-17 / sol-implementer
- 2026-10-17: A device-grant issue that fails the store write returns 503 after `DeviceGrantStore::poll` already consumed the approved grant, so that login must restart from `/oauth/device_authorization` (the retried poll gets `expired_token`). Restoring the grant would need a new un-consume API on the device-grant store; not built (the plan specifies rollback of the refresh store only).
  Date/Author: 2026-10-17 / sol-implementer
- Decision: the strict path is `parse(bytes)` per module plus a typed accessor the import calls (`state_persist::normalized_quotas`, `gateway::persist::parse_records`, `gateway::spend::persist::parse_state`), not a `load_strict(path)`. `LoadIssue::{NotJson, Version}` lives in `store::import`; the lenient store-less `load`s map it to the same warn texts and `None` (spend keeps its fail-closed `io::Error` messages).
  Rationale: the import needs the exact bytes for the sha256 and the path for its error, so a path-taking `load_strict` would read the file twice.
  Date/Author: 2026-10-17 / sol-implementer
- Decision: a sidecar write failure after the commit is a WARN, not a boot failure.
  Rationale: the data and `import_record` are already durable; the sidecar only drives the AC-031 warning, and failing boot would strand an operator whose store is correct.
  Date/Author: 2026-10-17 / sol-implementer
- Decision: `store::import` is a `pub` module (`import_all`, `ImportSources`, `Source`, `Outcome`, `ImportReport`, `Subsystem`) because `main.rs` is a separate crate from the lib; `ImportSources::from_config(&Config)` encodes "pool always, gateway/spend when configured". Every failure, including a store error mid-transaction, is wrapped in `StoreError::Import{subsystem, path, reason}` (path `(no snapshot)` for a NULL-source record).
  Date/Author: 2026-10-17 / sol-implementer
- Decision: the repos' write bodies became `pool_repo::write_pool`, `sessions_repo::apply_diff_in`, `spend_repo::write_state` (take `&mut Tx`); public behavior is unchanged. Import never calls a repo fn that opens its own transaction (SQLite has one connection). The AC-031 check runs inside each lenient store-less `load` right after the file read, so it fires only on store-less boots.
  Date/Author: 2026-10-17 / sol-implementer
- Decision: `PoolPersister::stop_and_flush(self)` (stop the timer task, then `flush_now`) is what `StoreRuntime::shutdown` calls; `stop(self)` consumes the handle, so the plan's "stop tasks -> flush_now" order needed one method that does both. `shutdown` wraps heartbeat stop -> `stop_and_flush` -> release hold -> close in one `tokio::time::timeout(5 s)`; past it the rest is abandoned (WARN) and the hold goes stale on its own. A failed final flush is a WARN and the hold is still released.
  Rationale: keeps `stop`/`flush_now` and their tests unchanged.
  Date/Author: 2026-10-17 / implement-executor
- Decision: `store::boot::start` attaches the store whenever `[store]` is set but restores gateway sessions / spend only when `ImportSources::from_config` marks them `On` (pool always). The spend restore's `io::Error` becomes `StoreError::Database { reason: "restoring gateway spend-limit state: ..." }`. `serve` reports a `start` error as `failed to start the store` with the `StoreError` as its cause (anyhow chain on stderr, exit 1), like its other fatal boot errors.
  Rationale: mirrors the import rule; `StoreError` has no spend-specific variant and adding one is a public type change for a message.
  Date/Author: 2026-10-17 / implement-executor
- Decision: binary-level cases live in `tests/store_boot.rs` (+ `tests/store_boot/{harness,faults}.rs`), each a `per_engine!` pair; the child runs with `env_clear()` and `HOME` = a scratch dir. Case 6 (SIGTERM flushes and releases the hold) is folded into `restart_keeps_pool_sessions_and_spend`: the restart boots only if the hold was released, and pool quota survives only via the shutdown flush (the test asserts SIGTERM is sent >= 1 s before the persister's first 15 s tick). The second-instance HoldHeld case runs on both engines, not only SQLite. A pooled route behind `[server.gateway]` needs a gateway JWT, so the harness mints one with `shunt::gateway::jwt::mint`.
  Date/Author: 2026-10-17 / implement-executor
- Decision: fault injection, no hooks in the binary. SC-004: SQLite — the test process holds `BEGIN EXCLUSIVE` on the store file; PostgreSQL — `pg_terminate_backend` on the binary's connections (tagged `application_name=<schema>`) then `DROP SCHEMA ... CASCADE`; non-vacuity: the following shutdown logs the failed final pool write (or the shutdown timeout). NFR-001: SQLite — `BEGIN IMMEDIATE` held from 2 s before the persister's first tick until 3 s after it (inside the binary's 5 s busy timeout); PostgreSQL — statement-level `BEFORE` triggers calling `pg_sleep(3)` on `pool_quota` and `instance_hold`. The tick time is read from the timestamp of the binary's `pool state persistence enabled (store)` log line, not from spawn time. Non-vacuity: `pool_quota` is still empty after the 50 requests, then gains its row after the delay lifts, >= 3 s after the tick and before the next one.
  Date/Author: 2026-10-17 / implement-executor
- Decision: the AC-013 source scan matches `crate::store` / `shunt::store` as whole path segments, one-line and multi-line `use crate::{ store::... }` groups, and `super::store` in root-level `src/proxy.rs`; it asserts it scanned `src/proxy.rs` and > 20 files. A nested group (`gateway::{\n store::X`) is a false positive by design (fails loudly).
  Date/Author: 2026-10-17 / implement-executor
- Decision: `shunt check` prints `config ok`, then (with `[store]`) one line from `store::inspect::inspect`: `store ok: <backend> <sanitized target>, schema vN (binary supports vM)`, plus `; K pending migrations apply at boot` on the same line when N < M (`; 1 pending migration applies at boot` for K = 1). A missing SQLite file is not an error: `store ok: sqlite sqlite:<path>, file does not exist yet; it is created with mode 0600 at boot`, exit 0. Failures (`Unreachable`, `SchemaTooNew`, ...) exit non-zero with the `StoreError` Display under `store check failed` on stderr (`config ok` has already printed, since the config itself was valid). The inspection is bounded at 15 s (`INSPECT_TIMEOUT`, above the 10 s postgres acquire timeout).
  Rationale: the plan left punctuation open; one line keeps `check` output greppable and the docs in T012 quote these strings.
  Date/Author: 2026-10-17 / implement-executor

- Decision: T012 also touched surfaces beyond the validation list: `docs/config-reload.md` (`[store]` joins the restart-only list), `docs/dashboard-metrics.md` (new `shunt.store.write_failures` row plus a note that its counts predate it), the four `guides/opentelemetry.mdx` metric tables, the four `reference/cli.mdx` `## shunt check` sections (the "stays offline" sentence now reads "This guard stays offline", scoped to the Antigravity guard), and two rows in each `reference/troubleshooting.md` (store startup errors; `/oauth/token` 503). The `state_path` rows for pool, gateway, and spend in the configuration reference each note the file is only an import source once `[store]` is set.
  Rationale: AGENTS.md requires every surface that states the changed behavior to move in the same change; troubleshooting is a symptom/cause table, so the boot errors and the 503 fit its structure.
  Date/Author: 2026-10-17 / sol-implementer
- Decision: docs say prebuilt release binaries include PostgreSQL support and `cargo build`/`cargo install` do not; they do not claim the Homebrew formula has it (only `release.yml` was verified). ADR-0006 was `Proposed` when this task ran; it was flipped to `Accepted` afterwards (ADR and index), so the ADR file is the single source of truth for its status.
  Date/Author: 2026-10-17 / sol-implementer

## Surprises & Discoveries

- `[server.gateway].state_path` changes on reload redirect `save_if_dirty` silently (it reads `state.refreshed().config`), with no restart-only warning — pre-existing, out of scope, follow-up issue.
- Pool `restore()` treats a malformed or unknown-version snapshot as a cold start, while spend fails closed — the import path needs strict loaders for all three.
- Observation: `cargo clippy --all-targets --features store-postgres -- -D warnings` without `bench` fails on `src/routing/stage/store.rs:50` (unused import `MAX_TRACKED_CHILD_PINS`, used only by `bench_support`); clean with `--features bench`. Pre-existing, unrelated to sqlx.
  Evidence: lib-test target error; `--features store-postgres,bench` finishes clean.
- Observation: the default-feature build (`cargo test --test router_surface --test prefill_router` under `RUSTFLAGS=-D warnings`) fails on `src/store/open.rs` `POSTGRES_ACQUIRE_TIMEOUT` and `DEFAULT_POSTGRES_CONNECTIONS` being dead without `store-postgres`; introduced in T003, first seen in CI on the T004 push.
  Evidence: CI run 38896134846, step `Test default build (no ui feature)`, exit 101.
- Observation: `tests/responses_chain_stream.rs` fails 3 tests on this machine (`refused_port_is_deterministically_refused` TimedOut, two `a_body_error_*`), identically with Cargo.toml, Cargo.lock and src/lib.rs reverted to HEAD.
  Evidence: pristine run `13 passed; 3 failed`.
- Observation: on this machine a freshly spawned debug `shunt` binary takes ~3 s from `spawn()` to its first log line (the in-process boot with a SQLite store takes ~35 ms), so any timing derived from spawn time is off by seconds.
  Evidence: `/health` refused for ~3.0 s after spawn in every run; the child's log shows config load -> store opened in 33 ms.
- Observation: `docs/dashboard-metrics.md` states 18 series and "sixteen of eighteen are write-only", but `src/metrics.rs` already has more counters than the doc's table lists (for example `shunt.router.decisions`), so the counts were stale before this track. T012 added the store row and a note rather than recounting.
  Evidence: `grep -c shunt.router.decisions src/metrics.rs` -> 2; the table has no such row.

## Outcomes & Retrospective

- **Delivered**: an opt-in `[store]` (SQLite always compiled, PostgreSQL behind `store-postgres`) backing the three existing snapshots — pool quota, gateway refresh tokens, and spend limits with their audit — plus a single-instance hold with heartbeat and fencing, import-once from the legacy JSON files, `shunt check` store inspection, and docs across README, `docs/`, and the site in four locales. No `[store]` keeps today's code path (AC-022 regression guard in `store::boot`). ADR-0006 is Accepted; the request-history rollups from its Phase 1 move to a follow-up track.
- **Review loop (3 iterations, ensemble: gpt, ocr, cubic)**: each round found real concurrency defects in the durable paths, all fixed with mutation-checked tests — a shutdown flush lost to an aborted tick, a failed migration stranding the hold, a dropped request stranding a rotation (f581409e); detached commits left unbounded by the first fix, unknown commit outcomes, spend mutations split from memory (ff12dc04); unreadable unknown commits leaving a stale baseline (c5180e46). The last fix commit was not re-reviewed (loop cap).
- **Deferred**:
  - Manual testing — a real Claude Code login through the gateway across a restart, and a second `shunt run` on the same SQLite file — was not run in this track.
  - Validated-but-plausible review items, left unfixed: `shunt check` creates SQLite `-wal`/`-shm` sidecars, contradicting `docs/store.md`'s "never writes" claim. A missing parent directory on inspect. The hold uses the local clock and judges staleness by the acquirer's config. A SQLite hold read-then-write race surfaces `Database` rather than `HoldHeld`.
  - TD-003 and TD-004 are in the tech-debt tracker.
- **Lessons**:
  - A fix that moves work onto a spawned task must also move the lock that bounded it, or the fix creates a new unbounded queue.
  - Treat "commit failed" as "outcome unknown" on any network engine, and reconcile against the store rather than memory.
  - When running the full suite locally, pass `--no-fail-fast`: without it, the known `responses_chain_stream` failure stops every later test binary from running, so the run quietly covers less.
