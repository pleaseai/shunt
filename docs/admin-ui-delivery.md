# Admin UI delivery — listener, namespace, and asset pipeline

How shunt should deliver an operator dashboard: which process serves it, which
socket it listens on, which paths it may claim, and how its assets are built.

This is a design record, not an implementation plan. It fixes four decisions —
**in-process serving**, an **optional second listener**, a **three-way path
split**, and an **embedded SPA bundle** — reserves one namespace
(`/v1/organizations/*`) that a later milestone will need, records the
**single-instance deployment constraint** the dashboard would otherwise obscure,
and evaluates the **shared store** that constraint implies without adopting one.

It builds on [`m9-admin-surface.md`](m9-admin-surface.md), which already ships an
admin-authenticated web surface. M9 answered *what* the admin surface does; this
document answers *where it lives* as that surface grows past what
server-rendered string literals can carry.

## Motivation

The question "how do we offer a UI from the CLI" reads as a binary — a `shunt ui`
subcommand, or serve it from `shunt run` — but shunt already serves an admin web
surface from `shunt run`, gated by `[server.admin]`. So the real decisions are
elsewhere, and three of them are load-bearing:

- **Exposure.** `server.bind` is a single value, and the admin routes merge into
  the same router as the proxy (`server::build_router`). A gateway bound to
  `0.0.0.0` exposes `/admin/login` to the same network as `/v1/messages` — and,
  when `[server.gateway]` is on, alongside deliberately unauthenticated OAuth
  routes.
- **Namespace.** `/admin` already mixes HTML pages and JSON API on the same
  prefix. An SPA that wants `/admin/pool` as a deep link collides with the
  `GET /admin/pool` JSON endpoint that exists today.
- **Assets.** The current UI is ~770 lines of HTML/CSS/JS inside Rust string
  literals (`src/admin/html.rs`, `src/admin/script.rs`). There is no build step,
  no type checking, and no component model. `src/AGENTS.md` asks for files under
  500 lines; two of these are already near that on presentation code alone.

## Current surface

Every path shunt can register, and what gates it. This table is the conflict
baseline for any new route.

| Gate | Method | Path |
| :-- | :-- | :-- |
| always | `GET` | `/` — plain-text landing; **also answers `HEAD` for liveness probes**, unauthenticated |
| always | `GET` | `/health` — unauthenticated, exempt from the concurrency gate |
| always | `GET` | `/protocol`, `/v1/models`, `/routes` |
| always | `POST` | `/v1/messages`, `/v1/messages/count_tokens` |
| `[server.admin]` | — | 15 routes under `/admin` — see [M9](m9-admin-surface.md#endpoints-registered-only-when-serveradmin-is-set) |
| `[server.gateway]` | `GET` | `/.well-known/oauth-authorization-server`, `/device`, `/device/callback`, `/managed/settings` |
| `[server.gateway]` | `POST` | `/oauth/device_authorization`, `/oauth/token`, `/device`, `/device/authorize` |
| `[server.gateway]` | `POST` | `/v1/metrics`, `/v1/logs`, `/v1/traces` (inbound OTLP ingest) |
| `[server.codex_endpoint]` | `POST` | `/backend-api/codex/responses`, `/responses`, `/v1/responses` |
| `[server.codex_endpoint]` | `POST` | `/backend-api/codex/analytics-events/events`, `/codex/analytics-events/events` |
| `[server.usage]` | `GET` | `/usage` |
| `[server.oauth_usage]` | `GET` | `/api/oauth/usage` |

Two properties of this table matter downstream:

- **The router has no wildcard or fallback route anywhere.** An unmatched path
  404s in the gateway's own error shape. That is a contract clients depend on,
  not an accident.
- **The Codex endpoint paths are fixed constants**, not configuration.
  `CodexEndpointConfig` carries only `provider`; the paths live in
  `codex_endpoint::PATHS` and `codex_analytics::PATHS`, which
  `concurrency::is_codex_path` also classifies against so registration and error
  shape cannot drift apart.

## Deployment topology — single instance today

Everything below assumes one shunt process. That assumption is currently
load-bearing, and it is a property of the gateway, not of the UI. Recording it
here because a dashboard is the surface where an operator first notices it.

| State | Backing | Behavior across N instances |
| :-- | :-- | :-- |
| Admin browser session (`SessionStore`) | in-process `Mutex<HashMap>` | Sign in on A, request lands on B ⇒ login screen. Requires sticky sessions. |
| Pending login (`PendingStore`) | in-process, single-use, TTL-bound | Provisioning **breaks**: start on A, complete on B ⇒ expired/absent. |
| OIDC state (`OidcStateStore`) | in-process | Authorize on A, callback on B ⇒ state mismatch. |
| Admin rate limiters | in-process, documented as process-global | Effective limit relaxes to N× the configured value. |
| `AccountPool` quota/cooldown | memory + optional `[server.pool] state_path` | Per-instance view. `/admin/pool` reports one replica, not the fleet. |
| Gateway login sessions | optional `[server.gateway] state_path` | Per-instance view. |
| Either `state_path` | `atomic_file::write_private_atomic` | Atomic per write, but **last-writer-wins across processes**. No lock, no merge — a shared path is silently clobbered. |
| Credential refresh | `static REFRESH_LOCK` | In-process single-flight only. See below. |
| `max_concurrent_requests` | per process | Fleet ceiling is N× the configured value. |

**The blocking constraint is refresh serialization.** `src/auth/claude/auth.rs`
holds a process-global `tokio::sync::Mutex`, documented as "In-process
single-flight for Claude OAuth refreshes"; `src/auth/codex/auth.rs` mirrors it.
Two processes sharing an account store have no shared lock, so both can refresh
the same account concurrently. Refresh tokens rotate, so the loser's stored token
is invalidated — the exact condition the code already warns about ("stored
refresh token is now stale until re-login"). Recovery is a manual re-login.

Sharing one account pool across replicas requires sharing those very files, so
this is not a tuning problem. Scaling out safely needs a shared store for pool
state, sessions, and refresh coordination — which is why the upstream gateway
puts its spend, audit, and identity tables in Postgres.

Supported today: **one instance**. Running several means giving each its own
`state_path` and its own account-store directory — separate gateways that happen
to share a config shape, not one horizontally-scaled gateway. Reaching the admin
surface then means addressing a replica directly rather than going through a load
balancer, which Decision 2 makes natural.

## Storage — what a shared store fixes, and what it does not

The topology section says scaling out needs a shared store. This section records
what was evaluated and why nothing is adopted here, so the same research is not
repeated. It is deliberately not a fifth decision: a store is a gateway-wide
concern that outgrew this document the moment it came up.

### A store is justified independently of replicas

Three of these have nothing to do with running more than one process:

- **Dashboard history.** Every observable today is a point-in-time value in
  memory. "Per-account usage over the last seven days" — the thing that makes a
  dashboard worth building rather than a status page — is impossible without
  durable storage. This is the strongest reason to want a store at all.
- **Spend limits and audit.** The reserved `/v1/organizations/*` namespace needs
  durable counters and an append-only mutation trail. `/audit` is a table.
- **Replacing two ad-hoc JSON files.** `[server.pool] state_path` and
  `[server.gateway] state_path` are separate hand-rolled snapshots.
- **Cross-process clobbering.** Both files are written whole via
  `atomic_file::write_private_atomic`. Atomic per write, last-writer-wins between
  processes. A transactional read-modify-write ends that.

### What a store does not fix

`max_concurrent_requests` is per-process by nature, and a distributed semaphore
on the request hot path is not wanted. The admin rate limiters could be stored,
but a write per login attempt on the hot path is a poor trade for a
defense-in-depth control.

### Two hazards a store introduces

- **Refresh coordination is a lease, not a transaction.** Replacing the
  process-global `REFRESH_LOCK` means claiming a lease in one short transaction,
  performing the OAuth round trip, then writing the token and releasing in a
  second one. A write transaction must not be held across the network call —
  SQLite has a single writer, so that stalls everything. The hazard moves from
  "no lock" to "lease expiry versus a slow refresh", which is an improvement but
  still a distributed-lock design with the usual failure modes.
- **Persisting sessions weakens a documented security property.** M9 specifies
  that the pending-login store is in-memory, single-use and TTL-bound, and that
  emergency token rotation works because a restart "drops every session the old
  token minted". Session rows in a database survive a restart, so that recovery
  path has to be rebuilt rather than inherited.

Moving credential material itself into a store is a separate question again: it
would break interop with the `~/.claude/.credentials.json`-shaped files other
tools read, and `src/AGENTS.md` requires sign-off before changing credential
refresh or writeback semantics.

### Candidates

**SQLite** is the low-cost option. `rusqlite` 0.32 (`bundled`) is already a
dependency, used today only to read Cursor's app-state database read-only
(`src/auth/observation.rs`); shunt owns no database of its own. It covers
everything in "justified independently" above. Its ceiling is that it is a
single-host embedded engine: locking over a network filesystem is documented as
unreliable, so replicas on separate nodes sharing one file is a corruption path,
not a deployment. SQLite turns "one process" into "one **host**, several
processes" — not into horizontal scale.

**Turso** was evaluated because it is Rust-native and markets the multi-node
story. It does not fit today, for three independent reasons:

- Its Rust rewrite is pre-1.0, and the one capability that would matter here —
  multi-process WAL coordination — is listed as experimental.
- Turso Sync and embedded replicas are eventually consistent by design. shunt's
  multi-instance problems are write *coordination* (the refresh lease, pool
  read-modify-write), not read scaling. An eventually consistent lease is not a
  lease: two replicas can each acquire it locally and both refresh, reproducing
  the token-rotation race with extra steps.
- Its Postgres wire compatibility, which would otherwise be attractive given the
  upstream gateway's Postgres schema, currently has `FOR UPDATE` perform no
  locking and no advisory locks at all — the two primitives a lease needs, one
  silently broken and one absent. The server also "trusts every connection" and
  offers no TLS. Its own compatibility matrix warns of "wrong results or lost
  information without any error".

Revisit Turso if multi-process WAL coordination leaves experimental status, or if
Postgres compatibility gains real locking, authentication, and TLS.

**Postgres** is the answer if the requirement ever becomes genuine multi-node,
and the upstream gateway's configuration reference is the strongest evidence for
that. There, `store` is not an optional scaling upgrade — it is one of five
**required** sections, and `postgres_url` is mandatory for a stated reason worth
quoting: "the device-grant rendezvous, where the browser callback writes and the
polling CLI reads, needs cross-replica state." Spend limits are not the
justification; the login flow is. `max_connections` is documented as a per-replica
pool with the guidance to keep "replicas × this" under the database's own limit,
so several replicas are the assumed topology rather than an advanced case, and
the gateway runs its own schema migrations at boot.

That matters to shunt beyond this document. shunt's own gateway surface has the
same rendezvous — `/oauth/device_authorization` writes, `/device` completes,
`/oauth/token` polls — and it is in-memory, exactly like the admin pending-login
store. The constraint the upstream gateway resolved by making Postgres mandatory
is structural and already present here; it is not created by adding a dashboard.
Adopting the reserved namespace later would land on the same schema, so it would
port rather than be reinvented.

### Position

Adopt nothing here. If a store lands, plain SQLite for the history and audit work
that stands on its own merits, kept deliberately separate from the multi-instance
question — which is Open question 5, and which Postgres, not SQLite, would answer.

The upstream comparison raises the stakes on that question rather than settling
it. A gateway that requires Postgres to boot is a different product from one that
runs from a single binary on a laptop, and shunt's single-binary local mode is a
deliberate feature. The realistic shape is therefore a store that is **optional**,
with the single-instance in-memory path remaining the default — which is more
work than either extreme and should be decided on purpose.

## Decision 1 — the UI is served in-process; `shunt ui` is a launcher

A standalone `shunt ui` **server** process is rejected.

The admin handlers read live in-process state: the reload-aware `SharedState`
snapshot (re-read per request, so admin pages reflect a reloaded config rather
than the boot snapshot), `AccountPool` quota and cooldown, and `StatusStore`. A
second process has none of it. It would have to call the running gateway's admin
API over HTTP, holding an admin token, to render anything — a proxy in front of a
proxy, with a second copy of the auth surface to keep correct.

The corollary, from the topology section: the dashboard renders **one process's**
view. Under several instances `/admin/pool` describes that replica's pool, not
the fleet's. A fleet-wide dashboard is not a UI feature — it presupposes a shared
state store that does not exist yet.

`shunt ui` still earns its place as a **launcher**: resolve the admin URL and
token file from config, open the browser, exit. That composes with
`shunt dashboard setup` (`src/dashboard.rs`), which already writes the token
file, adds the config blocks, and prints the URL.

For reference, `ccr` splits its gateway and UI across two ports because its UI is
largely a config-file editor with shallow runtime-state dependencies. shunt's is
not. Coder, whose dashboard does depend on live server state, serves the UI and
the API from one `coderd` listener — the model shunt already follows.

## Decision 2 — optional second listener via `[server.admin].bind`

```toml
[server]
bind = "0.0.0.0:3001"      # gateway reachable on the network

[server.admin]
bind = "127.0.0.1:3002"    # new, optional; admin surface loopback-only
```

Absent ⇒ today's behavior exactly: the admin tree merges into the main router on
`server.bind`. This keeps the change backward compatible and avoids redefining a
documented config key (`AGENTS.md` boundary: ask before changing public config
keys).

Present ⇒ the admin routes are **removed** from the main router and served on
their own `TcpListener` in the same process, sharing the same `AppState` — so the
live-state argument from Decision 1 is preserved.

What this buys, beyond tidiness:

- The admin surface gets an exposure independent of the proxy's. Today one
  `bind` governs both, so hardening the gateway's reachability and hardening the
  admin surface are the same knob.
- Admin requests stop competing with inference for `max_concurrent_requests`
  permits, and stop inheriting the proxy's `http_tuning` body limits.
- With `[server.gateway]` enabled, `/admin/login` no longer shares a socket with
  intentionally unauthenticated routes (`/oauth/token`, `/device`,
  `/.well-known/oauth-authorization-server`), which simplifies any fronting
  proxy's allowlist.
- Under several instances it sidesteps the session and pending-login problems
  structurally rather than papering over them: an admin listener on a management
  interface is addressed per replica, so nothing depends on a load balancer
  honoring session affinity.

Whether the route tree is registered stays a **boot-time** decision, as M9
already specifies for `[server.admin]`; a reload that adds or changes `bind` logs
that it needs a restart.

**Implementation hazard.** `shutdown::shutdown_signal()` must not be called
twice. Its documentation explains why: tokio delivers signals through a
process-wide channel with no queue, and the current implementation deliberately
keeps one listener continuously live to close a delivery gap between waits. A
second listener must therefore be driven by a fan-out (`tokio::sync::watch`) from
a single `shutdown_signal()` call, not by a second call.

## Decision 3 — three-way path split; never a catch-all at `/`

| Namespace | Contents | Contract owner |
| :-- | :-- | :-- |
| `/admin/*` | operator UI: HTML shell, SPA client routes, `/admin/assets/*` | shunt |
| `/admin/api/*` | shunt-specific JSON: pool, status, accounts, observed, provisioning | shunt |
| `/v1/organizations/*` | **reserved** — see below | Anthropic |

### Why the UI and the JSON API must split

`GET /admin/pool`, `/admin/status`, `/admin/accounts`, and `/admin/observed`
return JSON today. Those are exactly the paths an SPA wants as browsable deep
links. Serving both meanings from one path requires content negotiation on
`Accept`, which breaks bookmarking and is fragile under any intermediary that
rewrites headers.

The JSON endpoints move to `/admin/api/*`. The current paths remain as aliases,
since M9 documents them as a curl-able surface and scripted callers exist.

### Why not the root

Mounting the UI at `/` with an SPA fallback — the shape a naive reading of
Coder's layout suggests — is closed by three separate facts:

1. `/` is already a handler that also answers `HEAD` for liveness probes.
   Replacing it changes behavior for any deployment probing it.
2. A catch-all fallback would make unmatched paths return HTML `200` instead of a
   404 in the gateway error shape. That breaks client error handling and, when
   Anthropic adds an endpoint shunt has not implemented yet, converts a clean
   "not implemented" into a parse failure. [`gateway-protocol.md`](gateway-protocol.md)
   relies on `404` being a legible answer.
3. `/v1/` already has three owners (base, gateway OTLP ingest, Codex endpoint)
   and the reserved namespace makes four. A fallback shadows precisely this kind
   of externally-specified path set.

`src/gateway/mod.rs` already carries an inline comment reasoning about
non-collision when it registers the OTLP paths. Path-collision avoidance is an
existing maintained invariant here; the UI joins it rather than introducing it.

## Reserved namespace — `/v1/organizations/*`

The Claude apps gateway serves its admin API at `/v1/organizations/spend_limits`
(list, create/replace, fetch, delete, plus `/effective` and `/audit`), with the
stated goal that a client written against Anthropic's public Admin API can
retarget it by changing base URL alone. Its conventions — `type` on every object,
`spl_`-prefixed IDs, cents-as-string amounts, the
`{type, error:{type,message}, request_id}` envelope, a `request-id` header on
every response — are fixed by that contract.

shunt does not implement this today, and this document does not put it in scope.
Spend limits are out of scope for epic #186. The namespace is recorded here so
that:

- No shunt-owned route ever claims `/v1/organizations/*`.
- The operator UI's own JSON stays at `/admin/api/*`, where shunt owns the shape,
  rather than being retrofitted into an Anthropic-shaped path later.

Two constraints to carry forward if it is ever implemented:

- **It presupposes a shared store.** Those endpoints are backed by Postgres
  upstream — spend counters, caps, an audit trail, and last-seen identity —
  because the gateway they belong to is a multi-replica design. Adopting the API
  is therefore not a routing change; it is the same shared-state work the
  topology section says scaling out needs. See
  [Storage](#storage--what-a-shared-store-fixes-and-what-it-does-not); the two
  should be planned together, not separately.
- **Authentication collides with an existing credential slot.** That API
  authenticates with `x-api-key` (against admin read/write key sets) or a gateway
  bearer whose `groups` claim grants admin. `x-api-key` is a slot shunt already
  consumes or strips on proxy paths, and four recent fixes (#355, #361, #362,
  #364) all came from adding a credential kind without updating every enumerating
  consumer of its slot. Adopting it requires a slot-consumer audit as an explicit
  step — feature tests passing is not evidence. Accepting only the
  gateway-bearer path first avoids the slot entirely.
- **`/protocol` grows.** The upstream protocol reference advertises usage-limit
  response headers and the `429` body. shunt serves its own `/protocol`
  (`src/protocol.rs`, six endpoints today), so conformance would include
  advertising them.

## Decision 4 — build the UI as an embedded bundle

The dashboard moves from Rust string literals to a real frontend build whose
output is embedded in the binary (the Rust equivalent of what Coder does with Go
`embed` — `rust-embed` or `include_dir`), served from `/admin` and
`/admin/assets/*`.

Constraints that survive the move:

- **Same-origin only, no external requests.** M9's guarantee that the admin pages
  make no outbound requests and never use `innerHTML` for upstream-derived
  strings must hold for the bundle too.
- **One binary.** No separate static host, no runtime asset directory.

The open trade-off is **whether `cargo build` may require a Node toolchain**.
Two options, neither yet chosen:

| | Feature-gated (`--features ui`) | Commit `dist/` |
| :-- | :-- | :-- |
| `cargo build` without Node | works, no UI | works, UI included |
| Source-of-truth risk | none | built output can drift from source |
| CI | builds assets for release | must verify the committed build reproduces |
| Default build has a dashboard | no | yes |

crates.io packaging is not a constraint: the crate is already `publish = false`
(issue #292).

## Desktop

[`desktop-app.md`](desktop-app.md) already fixed the desktop framework decision:
**Tauri**, with Deno Desktop evaluated and rejected — decisively because it has
no secure-storage API, which is disqualifying for a process whose job is holding
OAuth tokens and API keys, and secondarily on maturity and installer coverage.

Decision 4 compounds with that: a real SPA bundle is loadable by the Tauri shell
directly, so the desktop app reuses the dashboard frontend instead of growing a
second one. This is an additional argument for the asset-pipeline change, not an
independent decision.

## Risks

- **Duplicate-route panics are untested.** `axum::Router::merge` panics at boot
  on a duplicate path+method — a real safety net for new UI routes, but it only
  fires when both trees are registered. No test builds a router with
  `[server.admin]`, `[server.gateway]`, and `[server.codex_endpoint]` enabled
  together, so a collision would surface on an operator's machine rather than in
  CI. This is a gap today, independent of the UI work.
- **Alias drift.** Keeping `/admin/pool` and friends as aliases of
  `/admin/api/*` doubles the surface. They should share one handler, not two.
- **Second listener, second auth path.** The M9 session cookie is scoped
  `Path=/admin`, and its `Secure` flag keys off request-host loopback-ness. A
  separate admin listener changes the host a browser sees; the cookie scoping
  needs re-verification under that topology, not assumption.
- **The dashboard makes single-instance-ness visible.** Today an operator can run
  several shunt processes and not notice the per-replica pool view, because the
  pool is observable mainly through a response header and logs. A dashboard that
  presents pool health as *the* answer invites the misreading. Whatever the UI
  shows must be labeled as this instance's view.

## Open questions

1. Feature-gate vs commit `dist/` (Decision 4 table).
2. Frontend framework and whether `site/`'s toolchain is reused or kept separate.
3. Does `[server.admin].bind` also need its own TLS, or is a fronting proxy
   always assumed?
4. Should `shunt ui` open the browser only, or also run `dashboard setup` when
   the surface is not yet configured?
5. Is single-instance a documented *limitation* or a documented *decision*? The
   answer changes whether a shared-state store is a roadmap item or a non-goal —
   and the reserved namespace's fate follows it. See
   [Storage](#storage--what-a-shared-store-fixes-and-what-it-does-not) for what
   was evaluated; the upstream gateway requires Postgres to boot, while shunt's
   single-binary local mode is deliberate, so "optional store, in-memory default"
   is the likely shape and is more work than either extreme.

## Testing

- A `build_router` smoke test with **every optional surface enabled at once**,
  asserting no panic and that each namespace resolves. This closes the gap under
  Risks and is worth landing before any UI route, independent of the outcome
  above.
- Path-inventory assertion: the set of registered paths matches this document's
  table, so a new route cannot be added without the conflict review.
- `/` still answers `HEAD` after any UI work.
- An unmatched path still returns the gateway error shape, not HTML.
- With `[server.admin].bind` set, admin paths 404 on `server.bind` and serve on
  the admin listener — and the reverse when it is unset.
- Graceful shutdown drains both listeners from a single signal.

## Documentation impact

- `site/src/content/docs/reference/configuration.md` — `[server.admin].bind`.
- `site/src/content/docs/reference/endpoints.md` — the namespace split and the
  `/admin/api/*` aliases.
- `site/src/content/docs/reference/cli.mdx` — `shunt ui`, next to
  `shunt dashboard setup`.
- [`m9-admin-surface.md`](m9-admin-surface.md) — its endpoint table becomes the
  pre-split record; add a pointer here rather than rewriting it.
- `docs/running.md` — the single-instance topology statement belongs in the
  operational guide, not only in this design record.
- `README.md` — only if the dashboard becomes a headline capability.
