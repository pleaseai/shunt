# Persistent storage — what a shared store would fix

shunt keeps almost all of its state in process memory. This document records what
a durable store would fix, what it would not, what it would break, and which
engine fits — so the evaluation is not repeated every time the question comes up.

**Nothing is adopted here.** This is a decision record for a decision not yet
made. The deployment constraint that motivates it is documented in
[`admin-ui-delivery.md`](admin-ui-delivery.md#deployment-topology--single-instance-today),
whose state table is the single source of truth for what lives where today.

## Motivation

The question arrives from two directions that are easy to conflate:

- **"Can we run several gateways?"** — a scaling question. Admin sessions,
  pending logins, and OIDC state are per-process; both `state_path` snapshots are
  last-writer-wins between processes; and credential refresh is serialized by a
  process-global lock that a second process does not share.
- **"Can the dashboard show history?"** — a product question, and one that has
  nothing to do with replicas.

They want different answers. Treating them as one question produces either an
unnecessary database or a dashboard that cannot draw a chart.

## A store is justified independently of replicas

Three of these are true with exactly one process running:

- **Dashboard history.** Every observable today is a point-in-time value in
  memory. "Per-account usage over the last seven days" — the thing that makes a
  dashboard worth building rather than a status page — is impossible without
  durable storage. This is the strongest reason to want a store at all.
- **Spend limits and audit.** The `/v1/organizations/*` namespace reserved in
  [`admin-ui-delivery.md`](admin-ui-delivery.md#reserved-namespace--v1organizations)
  needs durable counters and an append-only mutation trail. `/audit` is a table.
- **Replacing two ad-hoc JSON files.** `[server.pool] state_path` and
  `[server.gateway] state_path` are separate hand-rolled snapshot formats with
  their own restore paths.
- **Cross-process clobbering.** Both are written whole via
  `atomic_file::write_private_atomic` — atomic per write, last-writer-wins
  between processes. A transactional read-modify-write ends that.

## What a store does not fix

`max_concurrent_requests` is per-process by nature, and a distributed semaphore on
the request hot path is not wanted. The admin rate limiters could be stored, but a
write per login attempt on the hot path is a poor trade for a defense-in-depth
control.

## Hazards a store introduces

- **Refresh coordination becomes a lease, not a transaction.** Replacing the
  in-process single-flight locks (`REFRESH_LOCK` in `src/auth/claude/auth.rs`,
  process-global; `REFRESH_LOCKS` in `src/auth/codex/auth.rs`, keyed per
  credential path) means claiming a lease in one short transaction,
  performing the OAuth round trip, then writing the token and releasing in a
  second one. A write transaction must not be held across the network call —
  SQLite has a single writer, so that stalls every other writer for the duration
  of an HTTP request. The hazard moves from "no lock" to "lease expiry versus a
  slow refresh": an improvement, but still a distributed-lock design with the
  usual failure modes, and the failure it guards against is an invalidated
  refresh token requiring manual re-login.
- **Persisting sessions weakens a documented security property.**
  [M9](m9-admin-surface.md#authentication-and-hardening) specifies that the
  pending-login store is in-memory, single-use, and TTL-bound, and that emergency
  token rotation works because a restart "drops every session the old token
  minted". Session rows in a database survive a restart, so that recovery path
  must be rebuilt rather than inherited.

Moving credential material itself into a store is a separate question again. It
would break interop with the `~/.claude/.credentials.json`-shaped files other
tools read and write, and `src/AGENTS.md` requires sign-off before changing
credential refresh or writeback semantics.

## Candidates

### SQLite

The low-cost option. `rusqlite` 0.32 (`bundled`) is already a dependency, used
today only to read Cursor's app-state database read-only
(`src/auth/observation.rs`); shunt owns no database of its own. It covers
everything under "justified independently of replicas" with no new dependency and
no new build requirement.

Its ceiling is that it is a single-host embedded engine. Locking over a network
filesystem is documented as unreliable, so replicas on separate nodes sharing one
file is a corruption path, not a deployment. SQLite turns "one process" into "one
**host**, several processes" — not into horizontal scale.

### Turso

Evaluated because it is Rust-native and markets the multi-node story. It does not
fit today, for three independent reasons:

- Its Rust rewrite is pre-1.0 (beta status was lifted at v0.7.0), and the one
  capability that would matter here — multi-process WAL coordination — is listed
  as experimental.
- Turso Sync and embedded replicas are eventually consistent by design. shunt's
  multi-instance problems are write *coordination* — the refresh lease, the pool
  read-modify-write — not read scaling. An eventually consistent lease is not a
  lease: two replicas can each acquire it locally and both refresh, reproducing
  the token-rotation race with extra steps.
- Its PostgreSQL wire compatibility, otherwise attractive given the upstream
  gateway's Postgres schema, currently has `FOR UPDATE` perform no locking and
  offers no advisory locks at all — the two primitives a lease needs, one
  silently broken and one absent. The server "trusts every connection" and
  reports that SSL is unavailable, so connections are unauthenticated and
  plaintext. Its own compatibility matrix warns that some clauses parse and run
  while their semantics are dropped, producing "wrong results or lost information
  without any error".

**Revisit if** multi-process WAL coordination leaves experimental status, or if
its Postgres compatibility gains real locking, authentication, and TLS.

### PostgreSQL

The answer if the requirement ever becomes genuine multi-node, and the upstream
Claude apps gateway's configuration reference is the strongest evidence for that.
There, `store` is not an optional scaling upgrade — it is one of five **required**
sections, and `postgres_url` is mandatory for a stated reason worth quoting: "the
device-grant rendezvous, where the browser callback writes and the polling CLI
reads, needs cross-replica state." Spend limits are not the justification; the
login flow is. `max_connections` is documented as a per-replica pool with guidance
to keep "replicas × this" under the database's own limit, so several replicas are
the assumed topology rather than an advanced case, and that gateway runs its own
schema migrations at boot.

That constraint is already present in shunt. Its gateway surface has the same
rendezvous — `/oauth/device_authorization` writes, `/device` completes,
`/oauth/token` polls — and it is in-memory, exactly like the admin pending-login
store. Nothing about a dashboard creates this; a dashboard only makes it visible.
Adopting the reserved admin-API namespace later would land on the same schema, so
it would port rather than be reinvented.

## Position

Adopt nothing yet. If a store lands, plain SQLite for the history and audit work
that stands on its own merits, kept deliberately separate from the multi-instance
question — which PostgreSQL, not SQLite, would answer.

The upstream comparison raises the stakes on that question rather than settling
it. A gateway that requires PostgreSQL to boot is a different product from one
that runs from a single binary on a laptop, and shunt's single-binary local mode
is a deliberate feature, not an accident of youth. The realistic shape is
therefore a store that is **optional**, with the single-instance in-memory path
remaining the default — which is more work than either extreme and should be
chosen on purpose rather than arrived at.

## Open questions

1. Is single-instance a documented *limitation* or a documented *decision*? Every
   other question here follows from that one.
2. If a store is optional, which subsystems may depend on it, and which must keep
   an in-memory path? A feature that only works with a store configured is a
   second product surface to test.
3. Does history belong in the same store as coordination state, or is retention
   and access so different that they should not share a lifecycle?
4. Retention and PII. The upstream gateway separates identity retention from
   spend retention deliberately, so a deprovisioned identity ages out while
   anonymous counters remain. Any shunt store holding per-account history inherits
   that question.

## Sources

- Claude apps gateway configuration reference (`store`, `admin`, `enforcement`
  blocks) and spend-limits Admin API reference, on `code.claude.com/docs`.
- `tursodatabase/turso` release notes and `postgres/COMPAT.md`; Turso embedded
  replicas and Rust SDK documentation on `docs.turso.tech`.
