# ADR-0006: Optional pluggable store on sqlx — SQLite and PostgreSQL; single instance first

## Status

Accepted

**Note (2026-10-17):** the request-history part of Phase 1 (the rollups in §3
and the "Dashboard history survives a restart" consequence) ships in a
follow-up track; see
[`2026-10-01-request-history-requirements.md`](../brainstorms/2026-10-01-request-history-requirements.md).
Phase 1 as delivered covers the three snapshots: pool quota, gateway refresh
tokens, and spend limits with their audit. The decision text below is
unchanged.

## Date

2026-10-01

## Context

[`docs/storage.md`](../../../docs/storage.md) evaluated a durable store and
adopted nothing. It answered three things — SQLite for single-host history and
audit, PostgreSQL if multi-node is ever required, and an **optional** store with
the in-memory path as the default — and left four questions open, the first of
which it says every other one follows from: *is single-instance a documented
limitation or a documented decision?*

This ADR answers that question: **single instance is a limitation, and it is
lifted in two phases.** Phase 1 gives one process a durable store. Phase 2 lets
several processes share one.

### What persists today

Three hand-rolled JSON snapshots, each written whole through
`atomic_file::write_private_atomic` and each last-writer-wins across processes:

| Snapshot | Config | Code |
| :-- | :-- | :-- |
| Pool quota | `[server.pool] state_path` | `src/state_persist.rs` |
| Gateway refresh tokens | `[server.gateway] state_path` (default on) | `src/gateway/persist.rs` |
| Spend limits + mutation audit | `[server.spend]` | `src/gateway/spend/persist.rs` |

Nothing records request history. `src/metrics.rs` pushes each observation to
OTel and Sentry and keeps no in-process aggregate outside `#[cfg(test)]`; the
bounded ring that [`dashboard-metrics.md`](../../../docs/dashboard-metrics.md)
Decision 2 adopts is not yet built.

### Turso was re-evaluated and left out

`storage.md` rejected **Turso Database**, the ground-up Rust rewrite, and those
findings stand. This ADR also considered **libSQL**, Turso's production-ready
SQLite fork ([docs.turso.tech/libsql](https://docs.turso.tech/libsql)), reached
as Turso Cloud or a self-hosted `sqld`. It speaks its own protocol (Hrana), not
the SQLite file format and not the PostgreSQL wire protocol, so no SQLite or
PostgreSQL driver can reach it. It would be a third implementation, and neither
phase needs it: `sqlite` covers one instance and `postgres` covers several.
See Alternatives.

## Decision

### 1. The store is optional; absent means today's behavior, byte for byte

A new top-level `[store]` table selects a backend. When it is absent, every
subsystem keeps its current in-memory and JSON-snapshot path, unchanged, and the
single-binary local mode stays the default. This keeps ADR-0003 Decision 2's
condition ("opt-in, with the current in-memory path remaining the default") and
`storage.md`'s Position.

```toml
[store]
backend = "sqlite"              # "sqlite" | "postgres"
url = "sqlite:///var/lib/shunt/shunt.db"
# postgres: url = "postgres://…", max_connections = 8
```

Exact key names are settled in the Phase 1 spec. Fixed here: `[store]` is
additive, and no existing key (`state_path` in particular) is removed or changes
meaning in Phase 1.

There is one concrete store type, with no per-subsystem repository traits. A
trait would force the in-memory pool, refresh-token, and spend stores to be
refactored for a single real second implementation, because their memory code is
entangled with the JSON snapshots and dirty flags. The store instead attaches as
an optional handle at the three persistence choke points: the pool persister, the
gateway token-endpoint save, and the spend save. When no handle is attached, those
choke points run today's code unchanged. The backend difference (`sqlite` or
`postgres`) stays inside the store's database layer, so each subsystem's queries
are written once. As a result, "a feature that only works with a store
configured" (`storage.md` open question 2) exists only where Decision 5 says so.

### 2. Two backends on one driver: `sqlx` for SQLite and PostgreSQL

| Backend | Driver | Phase 1 | Phase 2 (multi-instance) |
| :-- | :-- | :-- | :-- |
| `sqlite` | `sqlx` (`sqlite`) | yes | **no**: single host only |
| `postgres` | `sqlx` (`postgres`) | yes | yes |

- **One async driver for both.** `sqlx` 0.9 provides connection pools, async
  queries, and embedded migrations for both engines. That avoids wrapping a
  synchronous driver in `spawn_blocking` and running a second driver for
  PostgreSQL.
- **It coexists with `rusqlite`.** `rusqlite` 0.32 stays, and only the Cursor
  app-state reader (`src/auth/observation.rs`) uses it. `sqlx-sqlite` 0.9
  accepts `libsqlite3-sys >=0.30.1, <0.38.0`, so Cargo resolves one shared
  `libsqlite3-sys` (0.30.1 today) and links one bundled SQLite. Porting that
  reader and removing `rusqlite` is possible later but is not part of this work.
- **Queries are built at runtime (`query()`), not with the compile-time `query!`
  macros.** The macros need a live database or committed `.sqlx` offline data at
  build time. The plan decides whether `sqlx`'s `Any` driver or code generic
  over `sqlx::Database` shares one repository implementation between the two
  engines. `sqlx` 0.9 still ships the `any` feature. Which column types it
  covers is a plan-time check.
- **Two SQL dialects, two migration sets.** Migrations are embedded in the
  binary and applied at boot. A database at a newer schema version than the
  binary knows fails the boot rather than being written to.
- `sqlite` uses WAL mode and a single writer. `postgres` sits behind a cargo
  feature (`store-postgres`) that the release build enables, so a minimal build
  can drop it.

### 3. Phase 1 scope — the three snapshots and request history

**Snapshots.** Pool state, gateway refresh tokens, and spend limits and audit
move onto the store. When `[store]` is configured and the store holds no import
record for a subsystem, boot imports that subsystem's existing JSON snapshot.
The data and an import record are written in one transaction. When there is no
snapshot, boot writes the record alone, so a JSON file that appears later is
never imported over live data. The decision depends on the record, not on
whether the tables are empty: a subsystem whose data was later cleared is never
re-imported from a stale file. After the import the store is authoritative and
the JSON file is no longer written. The file is left in place, not deleted, so
removing `[store]` returns to the snapshot *as of import time*. Sessions revoked
and limits changed since then are not reflected, and shunt warns about this when
it loads such a file.

**Request history** is new. It is written only when a store is configured
(Decision 5):

- **Rollups, not events.** Rows are 5-minute buckets keyed by
  `(bucket, provider, account, model, status)` with request count, error count,
  latency sum and max, and token counts where the response observer has them.
  This matches the ring's shape in `dashboard-metrics.md`, so the read API
  serves either, and it bounds row count by label cardinality rather than by
  traffic. Per-request search is deferred.
- **No content, no end-user identity.** Rows carry no prompt, no response, and
  no inbound user or session identifier. `account` is the operator-configured
  account label. `model` passes through the same bounded, sanitized label set
  that `dashboard-metrics.md` Decision 3 requires. Without that bound a client
  could write unbounded rows.
- **Retention** is configurable with a 7-day default and is enforced by a
  periodic delete. This settles `storage.md` open question 4 for Phase 1: the
  history holds no identity, so it has no identity retention to separate.
- **Off the hot path.** The request path increments an in-memory bucket. A
  background task flushes closed buckets. If the store is unavailable, buckets
  are kept up to a bound and then dropped and counted. History is not an audit
  trail, and a request is never delayed or failed because of it.

This amends **ADR-0003 Decision 2**: history is backed by *the configured
store*, not by "plain SQLite" specifically. Its rejection of "PostgreSQL for
history" was a rejection of PostgreSQL *required to boot*, and that still holds,
because no backend is required.

### 4. History and coordination share one store, not one lifecycle

`storage.md` open question 3 is answered *same store, separate tables, separate
retention*. A second store config for history alone would double the
operational surface for no Phase 1 benefit. Retention deletes touch only
history tables.

### 5. Failure semantics

- A configured store that is unreachable or fails migration at boot **fails the
  boot**. An explicitly configured store does not silently fall back to memory.
- At runtime the **proxy path never awaits the store**. Pool state and history
  are written behind the request path. A failed write is logged, counted, and
  retried, and the in-memory state remains the serving copy.
  Pending write-behind state is flushed on graceful shutdown.
- Writes that must be durable **await the store**: spend-limit mutations, and
  gateway refresh-token grants, rotations, and revocations, which already await
  their JSON save on the token endpoint today (`src/gateway/oauth.rs`). Neither
  path is the proxy path. When the store is unavailable, a spend mutation fails
  with a gateway-owned error in the Anthropic error shape rather than reporting
  success for a write that did not land.
- **Single instance is enforced, not only documented.** While running, a process
  holds an exclusive, renewed hold in the store. A second process on the same
  store refuses to start, and a hold that has stopped renewing past a stale
  threshold can be taken over. Offering `postgres` would otherwise invite
  several replicas onto one database, where they would race on refresh-token
  rotation. This hold is a boot guard, not the Phase 2 lease.
- Dashboard history is the only feature that exists only with a store. Without
  one, history is whatever the in-memory ring serves, once that ring is built.

### 6. Phase 2 — what multi-instance must add (direction, not design)

Phase 2 is a separate track. It is recorded here so that Phase 1 does not close
off any of these paths:

- **Eligible backend: `postgres`.** `sqlite` can serve a second process on the
  same host, but it is rejected for multi-host because file locking over network
  filesystems is unreliable (`storage.md`).
- **Refresh coordination as a lease** across all five refreshable stores
  (Claude, Cursor, Google, xAI, Codex), never a subset (`admin-ui-delivery.md`
  audit table). The primitive is a conditional `UPDATE … WHERE key = ? AND
  (holder IS NULL OR expires_at < now)`, a single statement that needs no
  interactive transaction, advisory lock, or `FOR UPDATE`, so a backend added
  later can support it too.
  Claim, OAuth round trip, then write and release, with no transaction held
  across the network call.
- **Rendezvous state shared:** gateway device grants and OIDC state, admin
  pending logins, admin OIDC state, and admin sessions. Persisting sessions
  must rebuild M9's "restart drops every session" recovery. One way is a
  session epoch that the operator bumps, invalidating every row minted under
  the old one.
- **Stays per-process and is documented that way:** `max_concurrent_requests`
  and the admin and device-flow rate limiters (`storage.md` "What a store does
  not fix").
- **Open, and requires sign-off before Phase 2 design** (AGENTS.md: credential
  writeback): replicas on different hosts cannot share the account-store
  *files* that a lease coordinates. Phase 2 must either require a shared volume
  or move credential material into the store, and the second option breaks
  interop with `~/.claude/.credentials.json`-shaped files.

## Consequences

### Positive

- Three snapshot formats collapse into one schema with transactional writes.
- Dashboard history survives a restart, with a bounded retention and PII posture
  decided up front rather than inherited.
- Phase 2 builds on Phase 1's store type, migrations, and backend wiring instead of
  re-plumbing them.
- The local single-binary mode is untouched for anyone who does not opt in.

### Negative

- Every persisted subsystem has two implementations to test. That cost is
  intentional (`storage.md` called it "more work than either extreme"), and the
  test matrix must cover memory, `sqlite`, and `postgres`.
- Two SQL dialects mean two migration sets that can drift. CI must run the
  migration and repository tests against PostgreSQL, not only SQLite.
- New dependency: `sqlx`, with its `postgres` driver behind a cargo feature.
  `rusqlite` stays alongside it for the Cursor reader until someone ports that
  reader.

### Neutral

- JSON snapshots remain the store-less path and are not deprecated by this ADR.
- History rollups are a read model, like the ring. The OTel and Sentry sinks
  are unchanged (`dashboard-metrics.md` Decision 1).

## Alternatives considered

- **SQLite only, PostgreSQL later.** Rejected as the *plan*. Phase 2 needs a
  network backend, and introducing the second dialect only then would make
  Phase 1's store layer SQLite-shaped. Both backends land in Phase 1. Multi-instance
  *behavior* is still deferred.
- **`rusqlite` for SQLite plus a separate PostgreSQL driver.** Rejected. It
  means two drivers and two repository implementations, and the synchronous
  `rusqlite` API has to be wrapped in `spawn_blocking`. `sqlx` covers both
  engines behind one async API.
- **libSQL (Turso Cloud, `sqld`) as a third backend.** Deferred, not rejected.
  It needs the `libsql` crate, because no SQLite or PostgreSQL driver speaks
  its protocol. Its `remote` feature avoids a second bundled SQLite, but it pulls
  in a pre-1.0 crate and an older `hyper` 0.14 stack, and it would need a third
  repository implementation and a `sqld` CI service while serving neither
  phase. **Revisit if** a deployment needs a managed SQLite-dialect database.
  It would then be one more engine inside the store's database layer, either
  through `libsql`'s `remote` feature or through a Hrana HTTP client built on
  the existing `reqwest`.
- **libSQL as the local engine, replacing SQLite.** Rejected. Its embedded
  engine (`libsql-sys`) bundles a second SQLite whose `sqlite3_*` symbols
  collide with the `libsqlite3-sys` that `rusqlite` and `sqlx` share. It also
  puts a pre-1.0 crate under the default local store.
- **`turso` crate (Rust rewrite).** Rejected for the reasons in `storage.md`.
  Revisit on the triggers it lists.
- **PostgreSQL required.** Rejected. A gateway that needs PostgreSQL to boot is
  a different product (ADR-0003, `storage.md`).
- **A full ORM (sea-orm, diesel).** Rejected. The schema is a handful of
  tables, and `sqlx`'s runtime queries plus embedded migrations cover it
  without an entity layer.

## Related

- [`docs/storage.md`](../../../docs/storage.md): the evaluation this ADR resolves
- [`docs/admin-ui-delivery.md`](../../../docs/admin-ui-delivery.md#deployment-topology--single-instance-today): state table and refresh-lock audit scope
- [`docs/dashboard-metrics.md`](../../../docs/dashboard-metrics.md): ring shape, label bounding
- [ADR-0003](0003-admin-dashboard-extension.md): amended (Decision 2)
