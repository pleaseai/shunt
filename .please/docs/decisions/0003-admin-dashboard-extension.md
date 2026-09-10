# ADR-0003: Admin dashboard extension — UI platform first, optional SQLite history, no cost estimates

## Status

Accepted

## Date

2026-09-10

## Context

Two self-hosted CLIProxyAPI (CPA) panels — [CPA Manager Plus][cpamp] and
[CPA Usage Keeper][keeper] — were proposed as references for growing shunt's
admin surface. Both show a class of thing shunt's does not: persistent request
history, cost analytics with synced model prices, usage trends and heatmaps,
scheduled credential health inspection, and config management from the browser.
A desktop build is also anticipated, which makes the delivery mechanism — not
just the feature list — a decision to make now rather than discover later.

[cpamp]: https://github.com/seakee/CPA-Manager-Plus
[keeper]: https://github.com/Willxup/cpa-usage-keeper

### What shunt has today

`src/admin/` is 8,094 lines across seven files. `admin_router`
(`src/admin/mod.rs:221-257`) registers 16 paths / 18 method+path pairs — a
server-rendered login flow, four JSON reads, and the Claude/Codex provisioning
mutations; the per-path inventory lives in
[`admin-ui-delivery.md`](../../../docs/admin-ui-delivery.md). `/admin/observed`
already builds per-credential usage rows for six providers. The UI itself is
1,525 lines of HTML/CSS/JS held in Rust string literals (`src/admin/html.rs` 879,
`src/admin/script.rs` 646).

### The gap is a store, not a UI

A survey of every observability and accounting module — `metrics.rs`,
`observability.rs`, `usage.rs`, `usage_poll.rs`, `stream_metrics.rs`,
`telemetry.rs`, `state_persist.rs`, `oauth_usage.rs`, `status_poll.rs`,
`upstream_status.rs`, and the `gateway/` and `gateway/spend/` trees — establishes
three facts:

1. **There is no per-request event history anywhere.** shunt retains live
   aggregates (Sentry/OTLP counters and histograms), per-stream diagnostics that
   are discarded when the stream finishes (`src/stream_metrics.rs:142-199`), and
   current per-account quota snapshots. `[server.pool] state_path` persists only
   the current `QuotaState` (`src/state_persist.rs:52-64`).
2. **Nothing computes monetary cost.** No pricing table, no currency field, on
   any path.
3. **`[server.spend]` meters nothing.** `SpendStore` holds limit *policy* and an
   append-only mutation audit (`src/gateway/spend/store.rs:49-103`); it records
   no tokens, requests, or consumption. Ingested OTLP telemetry is relayed
   verbatim or discarded (`src/gateway/telemetry_ingest.rs:169-214`) and is never
   read back.

The majority of what the reference panels display therefore rests on a store
shunt does not have. Their own architecture makes this explicit: each runs as a
second service beside CPA with its own SQLite database, draining CPA's usage
queue.

### Constraints already fixed

[`admin-ui-delivery.md`](../../../docs/admin-ui-delivery.md),
[`storage.md`](../../../docs/storage.md), and
[`desktop-app.md`](../../../docs/desktop-app.md) are binding here, not reopened.
Three of their findings constrain the decision below; the rest is one link away:

- **A second serving process is rejected** (Decision 1) — the admin handlers read
  live in-process state (reload-aware `SharedState`, `AccountPool`,
  `StatusStore`). This is where shunt and the reference panels diverge
  irreconcilably: their deployment model is not portable here, so any analytics
  must live inside the gateway.
- **Plain SQLite is already the positioned engine** for history and audit, kept
  separate from the multi-instance question PostgreSQL would answer, and optional
  so the in-memory single-instance path stays the default.
- **The frontend work and the store decision are independent** — neither blocks
  the other, which is what makes the tracks below separable rather than a queue.

### Why cost is not simply portable

CPAMP's cost analytics assume per-token billing. shunt's primary case is consumer
subscriptions (Claude Max, ChatGPT Plus/Pro), where — as issue #309 puts it —
"there is no per-token bill, so a money figure would be fiction." Cost is
meaningful only for API-key upstreams, so importing the feature wholesale would
put a fabricated number on the surface an operator trusts most.

## Decision

Extend the admin surface in three separable tracks, and start with the first.

### 1. The UI platform lands first

Implement `admin-ui-delivery.md` Decisions 3 and 4 before any new data feature:
move the admin JSON and mutations to `/admin/api/*` as one breaking change, and
replace the Rust string literals with a React + Vite SPA embedded behind
`--features ui`.

This is first because it is the only prerequisite **shared** by both of the other
tracks and by the desktop app — Decision 4 notes the Tauri shell loads the same
bundle, so the desktop build reuses this frontend instead of growing a second
one. It also stops `html.rs`/`script.rs` from growing further; they are already
1,525 lines against the ~770 the design record measured when it argued the
literals had run out of room.

Sequencing within the track:

1. The `build_router` smoke test with every optional surface enabled at once,
   plus the path-inventory assertion. `admin-ui-delivery.md` names this as worth
   landing before any UI route, independent of the rest of the design, and it is
   the safety net for every route change that follows.
2. `feat!` — the `/admin/api/*` move, with the path-migration table in
   `endpoints.md` and a `BREAKING CHANGE:` commit footer pointing at it.
3. The SPA scaffold: its own package and lockfile, the `--features ui` gate,
   asset embedding, and an SPA fallback confined to the `/admin` mount.
4. Port the existing views and delete the dashboard string literals.

`[server.admin].bind` (Decision 2) is separable and may land at any point in or
after this track; it carries its own fail-open access-control constraint and
boot diagnostic.

### 2. History is backed by optional SQLite

When history lands, it is plain SQLite, opt-in, with the current in-memory path
remaining the default so the single-binary local mode survives — `storage.md`'s
own position, adopted here rather than left open. This is what makes the
reference panels' analytics — per-request search, trends, retention — possible at
all.

It is explicitly **not** coupled to the multi-instance question. Sharing pool
state, sessions, and refresh coordination across replicas is a different problem
with a different engine, and `storage.md`'s hazard analysis (five refreshable
stores, all single-flighting in-process only) is its audit scope, not this
track's.

The bounded in-memory ring proposed in issue #309 is not adopted as the
destination. It remains available as a cheaper first increment if the store slips,
but the target shape is durable.

### 3. Monetary cost estimates are deferred

No currency figure ships on the admin surface for now. The decision is revisited
once the history store exists and the shape of the recorded data is known;
attribution work in the meantime is expressed in tokens and quota-window fill,
which is both measurable today and the question subscription operators actually
have.

## Corrections to the design record made alongside this ADR

Three drifts were found while verifying the above and fixed in
[`admin-ui-delivery.md`](../../../docs/admin-ui-delivery.md) before this ADR was
accepted. The first is consequential, because Resolution 6 designates Decision
3's blocked-path table as *the* migration inventory:

- The table omitted `POST /admin/accounts/claude/{name}/refresh`
  (`src/admin/mod.rs:237-240`), listing 12 of the 13 paths that must move. A
  migration driven from the table as written would have left that route behind.
  It is Claude-only — Codex has no refresh route — so it fell outside the paired
  claude/codex shape every neighbouring row has.
- The "Current surface" table credited `admin_router` with 15 paths / 17
  method+path pairs. Those are M9's endpoint-table counts, not the router's: the
  router has 16 / 18, and M9 documents 15 of them (all but `GET /admin/status`).
  As written the row was self-contradictory — it claimed a total *and* an
  omission from that same total.
- The table cited `admin_router` at `src/admin/mod.rs:115-146`; it is now at
  `src/admin/mod.rs:221-257`.

## Consequences

### Positive

- One frontend serves the web dashboard and the Tauri desktop shell, so the
  desktop build is an additional consumer rather than a second UI to maintain.
- The `/admin/api/*` split lets the SPA claim clean `/admin/*` deep links
  immediately, with no aliases to retire and no hash routing to live with.
- `--features ui` keeps `cargo build` free of a Node toolchain; release CI
  enables the feature, so release binaries carry the dashboard.
- Deferring cost keeps a fabricated number off the surface while leaving the
  door open once real data exists.
- The two tracks are independent, so history work can start before the SPA is
  finished, or after, without either blocking.

### Negative

- Track 1 ships **no new data**. The visible payoff is deferred to the port and
  to track 2, which is a real cost in perceived progress.
- The `/admin/api/*` move breaks every scripted caller documented in
  `m9-admin-surface.md`. It must land in one release, and the release notes are
  release-please prose built from commit footers, so the `BREAKING CHANGE:`
  footer — not the PR body — is what carries it (issue #270).
- An optional store is more work than either extreme: a feature that only works
  when a store is configured is a second product surface to test.
- A from-source build without `--features ui` has no dashboard.
- The reference panels' deployment model, their SQLite schemas, and their
  usage-queue drain cannot be reused; only their feature vocabulary transfers.

### Neutral

- `[server.spend]` keeps its current meaning — limit policy and audit, not
  metering. A dashboard can show configured limits and their mutation history
  today; it cannot show spend.
- The dashboard remains one process's view. `storage.md` and
  `admin-ui-delivery.md`'s topology table own the multi-instance question, and
  whatever the UI shows must be labeled as this instance's view.
- Existing admin issues (#207, #214, #309, #369, #375, #427, #440, #441) keep
  their independent value; this ADR sequences the platform work around them
  rather than superseding them.
- Cost remains available as a later addition scoped to API-key upstreams, which
  is the narrower shape the caveat above implies.

## Alternatives Considered

- **Data/history first, rendered in the current UI.** Rejected as the *starting*
  slice: it would either strain 1,525 lines of string literals further or
  produce views that are rewritten once the SPA lands. The track itself is
  adopted, just not first.
- **Breadth on the current UI** (more provider provisioning, config editing, a
  live activity view). Rejected as the starting slice — cheapest, and it closes
  visible gaps against CPAMP's credential management, but every screen added
  this way is a screen ported twice.
- **A separate service beside shunt, as CPAMP and Keeper do.** Rejected by
  `admin-ui-delivery.md` Decision 1 and reaffirmed here: shunt's admin handlers
  read live in-process state a second process does not have, so it would become
  a proxy in front of a proxy with a duplicated auth surface.
- **The #309 bounded in-memory ring as the destination for history.** Rejected
  as the destination, retained as a possible increment. It answers "what is
  consuming this subscription" with no store and no retention or PII question,
  but it is lost on restart and covers streaming responses only — so the
  Antigravity adapter, which returns non-streaming JSON when `stream:false`,
  would read near zero.
- **PostgreSQL for history.** Rejected. It answers the multi-instance question,
  not the history one, and a gateway that requires PostgreSQL to boot is a
  different product from one that runs from a single binary on a laptop.
- **Full cost analytics with synced model prices, as CPAMP ships.** Rejected for
  now; see the caveat above.
- **Hash routing (`/admin/#/pool`) or a separate `/admin/ui/*` prefix.**
  Rejected by Resolution 6 as permanent URL costs that only defer the collision.
- **Committing `dist/`, or a network-fetching `build.rs`.** Rejected by
  Resolution 1 — drift risk and a supply-chain surface respectively.
