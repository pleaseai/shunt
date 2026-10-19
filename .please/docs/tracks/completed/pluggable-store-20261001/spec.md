---
product_spec_domain: storage
---

# Optional pluggable store, Phase 1a (single instance)

> Track: pluggable-store-20261001

## Overview

shunt keeps its durable state in three separate JSON snapshot files. This track
adds an **optional** store that an operator turns on with a new `[store]` table,
backed by either a local SQLite file or a PostgreSQL server. With a store
configured, pool quota state, gateway refresh tokens, and spend limits with their
audit trail are kept in one transactional store instead of three last-writer-wins
files.

Without `[store]`, shunt behaves exactly as it does today, apart from the one-time rollback warning in AC-031 for a snapshot that was previously imported into a store.

This is Phase 1 of
[ADR-0006](../../../decisions/0006-optional-pluggable-store.md): one shunt process
per store, enforced at boot. Request history on the store is the other half of
Phase 1 and follows as its own track
([requirements](../../../brainstorms/2026-10-01-request-history-requirements.md)).
Sharing a store across several processes (Phase 2) is out of scope and becomes its
own track.

## Durability classes

Each kind of state has one durability class. The acceptance criteria below refer
to these classes.

| State | Class | Meaning |
| :-- | :-- | :-- |
| Spend limits and audit | **Synchronous** | The admin mutation reports success only after the store has the write. |
| Gateway refresh tokens (grant, rotation, revocation) | **Synchronous** | The token endpoint responds only after the store has the write. This matches today's JSON path, which awaits its save on that endpoint. The one exception is a replay-triggered revocation whose store write fails: it stays in memory and is retried later (see AC-023). |
| Pool quota | **Write-behind** | Written in the background, and flushed on graceful shutdown. A crash can lose the writes since the last flush. |

None of these classes adds a wait to the proxy path. Gateway token endpoints and
admin endpoints are not on the proxy path.

## User Stories

### User Story 1 — Durable state in one store (Priority: P1)

An operator running one shunt process configures `[store]` with a SQLite file.
After a graceful restart, pool quota state, gateway sessions (refresh tokens), and
spend limits with their audit trail are all exactly as they were, held in a single
store instead of three hand-written files. The store's schema is created and
upgraded by shunt itself, and shunt refuses to start when it cannot safely use
the configured store.

**Why this priority**: it is the foundation every other story builds on, and it
replaces three bespoke persistence formats with one.

**Independent Test**: configure a SQLite store, create state in each of the
three subsystems, restart gracefully, and observe the same state through the
existing admin and gateway endpoints.

**Acceptance Criteria**:

1. **AC-001** — When shunt restarts gracefully with a configured store, the
   system shall restore pool quota state, gateway refresh tokens,
   and spend limits and audit records to the values they held before the
   restart.
2. **AC-002** — While a store is configured, the system shall not write the
   pool, gateway, or spend JSON snapshot files.
3. **AC-003** — When the operator changes a spend limit while a store is
   configured, the system shall report success only after the change is
   durably stored.
4. **AC-023** — When the gateway token endpoint issues, rotates, or revokes a
   refresh token while a store is configured, the system shall respond only
   after the change is durably stored, except that a revocation caused by a
   replayed refresh token fails closed in memory when the store write fails: the
   caller still receives `401 invalid_grant`, the next durable write or store
   persister tick retries the write, and a crash before then loses it
   (`src/gateway/durable.rs`).
5. **AC-024** — When shunt shuts down gracefully, the system shall flush pending
   write-behind pool state to the store before exiting.
6. **AC-018** — When shunt boots with a configured store, the system shall
   apply any pending schema migrations before serving traffic.
7. **AC-019** — If the configured store is unreachable or a migration fails at
   boot, then the system shall refuse to start.
8. **AC-025** — If shunt refuses to start because of the store, then the system
   shall report the reason.
9. **AC-020** — If the store's schema version is newer than the running binary
   supports, then the system shall refuse to start without writing to the
   store.
10. **AC-026** — While another live shunt process holds the store, the system
    shall refuse to start and name the holder.
11. **AC-027** — When the holder recorded in the store has stopped renewing its
    hold for longer than the stale threshold, the system shall take over the
    store and log that it did.
12. **AC-028** — When the system creates a SQLite store file, it shall create it
    with owner-only permissions.

### User Story 2 — Upgrade without losing existing state (Priority: P1)

An operator who already uses the JSON `state_path` snapshots adds `[store]`.
Their existing state carries over on the first boot. Removing `[store]` later is
possible but returns them to the snapshot as it was at import time, not to their
current state, and shunt says so.

**Why this priority**: without it, adopting the store discards live gateway
sessions and spend limits, which forces users to log in again.

**Independent Test**: populate the three JSON snapshots, add `[store]`, boot,
and confirm the store holds the imported state. Restart and confirm no second
import happens, including after a subsystem's data has been cleared. Make one
snapshot malformed and confirm boot fails with nothing imported for it.

**Acceptance Criteria**:

1. **AC-004** — When shunt boots with a configured store that has no import
   record for a subsystem and that subsystem's JSON snapshot exists, the system
   shall import the snapshot and record the import in one transaction.
2. **AC-005** — When the store has an import record for a subsystem, the system
   shall not import that subsystem's JSON snapshot again, even if the
   subsystem's data is empty.
3. **AC-006** — The system shall not delete or modify a JSON snapshot file when
   importing it.
4. **AC-029** — If importing a subsystem's JSON snapshot fails, then the system
   shall leave no imported data and no import record for that subsystem.
5. **AC-030** — If importing a subsystem's JSON snapshot fails, then the system
   shall refuse to start and report the reason.
6. **AC-031** — When shunt boots without `[store]` and a JSON snapshot it loads
   was previously imported into a store, the system shall warn that the snapshot
   reflects import-time state, which may include sessions revoked and spend
   limits changed since then.

### User Story 4 — The proxy is never slowed by the store (Priority: P1)

A developer's requests keep flowing at full speed even when the store is slow or
briefly unreachable.

**Why this priority**: shunt is a proxy first, and its request path must not
take on a new failure mode.

**Independent Test**: make the store unavailable while traffic flows, and confirm
that every proxied request still succeeds and that the failure is reported as a
metric and a log line.

**Acceptance Criteria**:

1. **AC-013** — The system shall not wait on the store while proxying a request.
2. **AC-014** — If a write-behind write to the store fails, then the system shall
   log the failure.
3. **AC-034** — If a write-behind write to the store fails, then the system shall
   increment a store-write-failure counter.
4. **AC-035** — If a write-behind write to the store fails, then the system shall
   retry it with the latest state rather than the state that failed.
5. **AC-036** — While write-behind writes are failing, the system shall keep
   serving from its in-memory state.
6. **AC-016** — If the store is unavailable when an admin changes a spend limit,
   then the system shall reject the change with a gateway-owned error in the
   Anthropic error shape.

### User Story 5 — Choose SQLite or PostgreSQL, and know it works before starting (Priority: P2)

An operator chooses between a local SQLite file and a PostgreSQL server, and
validates the configuration and connectivity before starting the gateway.

**Why this priority**: PostgreSQL in Phase 1 keeps the abstraction honest
for Phase 2. Validating up front turns misconfiguration into a clear message
instead of a boot loop.

**Independent Test**: every acceptance criterion in User Stories 1, 2, and 4 is
verified once with the `sqlite` backend and once with the `postgres` backend.
Then run `shunt check` against a reachable and an unreachable store of each
backend.

**Acceptance Criteria**:

1. **AC-021** — When the operator runs `shunt check` with a configured store,
   the system shall connect to it and report its schema version, without
   applying migrations or changing data.
2. **AC-038** — When the PostgreSQL password is supplied through the
   environment and absent from the inline config, the system shall connect
   using the environment value.
3. **AC-039** — If a `postgres` store points at a non-loopback host without TLS
   and server-certificate verification, then the system shall refuse to start
   unless the operator has explicitly opted out of that requirement.
4. **AC-022** — Where no `[store]` table is configured, the system shall behave
   as it does today, including reading and writing the existing JSON
   snapshots.

## Functional Requirements

- **FR-001**: System MUST accept an optional `[store]` configuration selecting
  the `sqlite` or `postgres` backend and its connection settings; secrets (a
  PostgreSQL password) MUST be loadable from the environment rather than only
  inline.
- **FR-002**: System MUST keep pool quota state, gateway refresh
  tokens, and spend limits plus audit in the store when one is configured, with
  the durability class given for each in "Durability classes", and in today's
  memory-plus-JSON form when not.
- **FR-003**: System MUST import each subsystem's existing JSON snapshot at most
  once, atomically with a durable per-subsystem import record, without altering
  the file.
- **FR-007**: System MUST manage the store's schema through versioned migrations
  applied at boot, and refuse a schema newer than it supports.
- **FR-008**: `shunt check` MUST verify store connectivity and report the schema
  version when a store is configured.
- **FR-009**: Existing configuration keys, including both `state_path` keys,
  MUST keep their current meaning.
- **FR-010**: User-facing documentation (`README.md` and its translations,
  `docs/`, and the `site/` reference and guides with their `ko`/`ja`/`zh-cn`
  copies) MUST describe `[store]`, both backends, the durability classes,
  migration and import behavior, the import-time nature of a rollback to JSON,
  the single-instance limit and its boot-time enforcement, and that
  the store holds refresh tokens and must be access-restricted accordingly.
- **FR-011**: System MUST hold an exclusive, renewed instance hold on the store
  while running, so a second process on the same store refuses to start.

## Non-functional Requirements

- **NFR-001**: The proxy request path MUST NOT wait on the store. A store being
  slow or unavailable MUST NOT change request success or add store-induced
  latency to proxied requests.
- **NFR-002**: The store file (SQLite) MUST be created with owner-only
  permissions, matching today's snapshot files, since it holds refresh tokens.
- **NFR-003**: A `postgres` store on a non-loopback host MUST use TLS with
  server-certificate verification unless the operator explicitly opts out.

## Success Criteria

- **SC-001**: After a graceful restart with a store configured, 100% of pool,
  gateway session, and spend state is present, with no user needing to log in
  again. After a crash, gateway session and spend state is still 100% present,
  and only pool writes since the last flush may be missing.
- **SC-002**: An operator upgrading from JSON snapshots to a store keeps all
  existing gateway sessions and spend limits on the first boot.
- **SC-004**: With the store unreachable, 100% of proxied requests that would
  succeed without a store still succeed.
- **SC-005**: A misconfigured store is reported by `shunt check` before the
  operator starts the gateway.

## Out of Scope

- **Phase 2 (multi-instance)**: refresh-token leases, shared device grants, OIDC
  state, admin sessions and pending logins, a fleet-wide pool view, and the
  credential-file question. These belong to a separate track. The instance hold
  in FR-011 only enforces the Phase 1 limit and is not a Phase 2 lease.
- **Turso / libSQL** as a backend (deferred by ADR-0006).
- **Request history** (recording, retention, label policy, admin read API):
  the follow-up track seeded by
  `.please/docs/brainstorms/2026-10-01-request-history-requirements.md`.
  Dashboard charts and the in-memory ring come after that.
- **Moving credential material** (`~/.claude/.credentials.json`-shaped files)
  into the store.
- **Exporting store state back to JSON** for a lossless rollback.
- **Porting the Cursor app-state reader** off its current SQLite library.
- **Removing or deprecating** the JSON `state_path` snapshots.

## Assumptions

- Cooldowns are monotonic in-process instants and are not persisted today; the
  store keeps the same quota-only scope as the pool snapshot.
- Exact `[store]` key names and the instance-hold stale threshold are settled in
  the plan.
- How the backends are verified in CI (service containers, local skip) and how
  NFR-001 is measured are settled in the plan.
