# Admin UI delivery — listener, namespace, and asset pipeline

How shunt should deliver an operator dashboard: which process serves it, which
socket it listens on, which paths it may claim, and how its assets are built.

This is a design record, not an implementation plan. It fixes four decisions —
**in-process serving**, an **optional second listener**, a **three-way path
split**, and an **embedded SPA bundle** — reserves one namespace
(`/v1/organizations/*`) that a later milestone will need, and records the
**single-instance deployment constraint** the dashboard would otherwise obscure.
The storage question that constraint implies is evaluated separately, in
[`storage.md`](storage.md). The seven questions this document originally left
open are now settled in [Resolutions](#resolutions).

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
| `[server.admin]` | — | 15 paths under `/admin`, 17 method+path pairs — `admin_router` in `src/admin/mod.rs`. [M9's endpoint table](m9-admin-surface.md#endpoints-registered-only-when-serveradmin-is-set) documents all of them except `GET /admin/status` |
| `[server.gateway]` | `GET` | `/.well-known/oauth-authorization-server`, `/device`, `/device/callback`, `/managed/settings` |
| `[server.gateway]` | `POST` | `/oauth/device_authorization`, `/oauth/token`, `/device`, `/device/authorize` |
| `[server.gateway]` | `POST` | `/v1/metrics`, `/v1/logs`, `/v1/traces` (inbound OTLP ingest) |
| `[server.codex_endpoint]` | `POST` | `/backend-api/codex/responses`, `/responses`, `/v1/responses` |
| `[server.codex_endpoint]` | `POST` | `/backend-api/codex/analytics-events/events`, `/codex/analytics-events/events` |
| `[server.usage]` | `GET` | `/usage` |
| `[server.oauth_usage]` | `GET` | `/api/oauth/usage` |

Two properties of this table matter downstream:

- **The router has no wildcard or fallback route anywhere.** An unmatched path
  therefore gets axum's built-in `404` with an **empty body** — not a
  `ShuntError` envelope, since nothing constructs one for a path that reached no
  handler. The status is the part clients depend on, and it is what makes an
  unimplemented endpoint legible rather than a parse failure. Whether the empty
  body should be shaped is an open question this document does not settle:
  `AGENTS.md` asks that gateway-owned errors keep the Anthropic error shape, and
  it is arguable both ways whether a `404` no shunt code constructs is in that
  rule's scope. Recorded only so a UI fallback is not designed on the assumption
  that a shaped body already exists here.
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
| Admin OIDC state (`OidcStateStore`, `src/admin/session.rs:265`) | in-process | Authorize on A, callback on B ⇒ state mismatch. |
| Admin rate limiters | in-process, documented as process-global | Effective limit relaxes to N× the configured value. |
| `AccountPool` quota/cooldown | memory + optional `[server.pool] state_path` | Per-instance view. `/admin/pool` reports one replica, not the fleet. |
| Gateway refresh tokens (`GatewayStores.refresh_tokens`) | memory + optional `[server.gateway] state_path` | Per-instance view. The **only** thing that `state_path` persists — `PersistedSessions` carries `refresh_tokens` and nothing else (`src/gateway/persist.rs:35-37`). |
| Gateway device grants (`GatewayStores.device_grants`) | in-process, **no persistence path at all** | Device login **breaks**: `/oauth/device_authorization` on A, `/device` on B ⇒ unknown code. Survives no restart either. |
| Gateway OIDC state (`GatewayStores.oidc_states`) | in-process, no persistence | A separate store from the admin one above; same split-brain failure on `/device/callback`. |
| Gateway device-flow rate limiters (`device_authorization_rate`, `device_verify_rate`) | in-process per-IP | Effective limit relaxes to N× the configured value. |
| Either `state_path` | `atomic_file::write_private_atomic` | Atomic per write, but **last-writer-wins across processes**. No lock, no merge — a shared path is silently clobbered. |
| Credential refresh | a `REFRESH_LOCK` per provider (Claude, Cursor, Google, xAI) / `REFRESH_LOCKS` (Codex) | In-process single-flight only, in **all five** refreshable stores. See below. |
| `max_concurrent_requests` | per process | Fleet ceiling is N× the configured value. |

The gateway rows deserve emphasis because `[server.gateway] state_path` reads
like coverage and is not: it persists refresh tokens only, so the whole
`/oauth/device_authorization` → `/device` / `/device/callback` → `/oauth/token`
rendezvous is memory-only. That is the same rendezvous
[`storage.md`](storage.md#postgresql) identifies as the upstream gateway's stated
reason for requiring Postgres — so shared-store work that starts from this table
must treat device state as unaddressed, not as something session persistence
already covers.

**The blocking constraint is refresh serialization, and it covers every
refreshable credential store — not just the two most visible ones.** Five stores
refresh tokens, and all five single-flight **in-process only**:

| Store | Lock | Granularity |
| :-- | :-- | :-- |
| `src/auth/claude/auth.rs:90` | `static REFRESH_LOCK` | process-global |
| `src/auth/cursor/auth.rs:36` | `static REFRESH_LOCK` | process-global |
| `src/auth/google/auth.rs:32` | `static REFRESH_LOCK` | process-global |
| `src/auth/xai/auth.rs:53` | `static REFRESH_LOCK` | process-global |
| `src/auth/codex/auth.rs:43` | `REFRESH_LOCKS` registry | keyed per credential path |

Four serialize every account of that provider against every other; Codex's
path-keyed registry lets different accounts refresh in parallel while the same
file still serializes. The designs differ, the scope does not: two processes
sharing an account store have no shared lock in any of the five, so both can
refresh the same account concurrently.

**How badly that ends is provider-specific, and the difference is load-bearing.**
Where the refresh token rotates and is consumed, the loser replays a spent token
and the stored credential is invalidated — xAI states this outright
(`src/auth/xai/auth.rs:48-52`), and Claude and Codex carry matching warnings —
"stored refresh token is now stale until re-login" (`src/auth/claude/auth.rs:187`)
and "stored refresh token **may** now be stale until re-login"
(`src/auth/codex/auth.rs:216`). Recovery there is a
manual re-login. Cursor and Google do **not** assume rotation: both keep the
existing refresh token when the response omits a replacement
(`src/auth/cursor/auth.rs:84-91`, whose comment says Cursor "is not known to
rotate+consume its refresh tokens (unlike xAI)"; `src/auth/google/auth.rs:152-154`
has the same shape). For those two the concurrent-refresh hazard is a lost-update
race on the credential file, not guaranteed invalidation. Recording the weaker
case as the strong one would push a future store toward recovery semantics two of
the five stores do not need.

Enumerating all five matters because the list is the audit scope for any future
shared-store work: a lease design that covers only Claude and Codex would leave
the other three racy across replicas while looking complete. That same xAI
comment also states the single-process assumption in code rather than leaving it
to this document — "Cross-process races are out of scope — shunt owns the file
and one gateway process is the norm."

Sharing one account pool across replicas requires sharing those very files, so
this is not a tuning problem. Scaling out safely needs a shared store for pool
state, sessions, and refresh coordination — which is why the upstream gateway
puts its spend, audit, and identity tables in Postgres.

Supported today: **one instance**. Running several means giving each its own
`state_path` and its own account-store directory — separate gateways that happen
to share a config shape, not one horizontally-scaled gateway. Reaching the admin
surface then means addressing a replica directly rather than going through a load
balancer, which Decision 2 makes natural.

## Storage — evaluated separately

The topology above says scaling out needs a shared store, and a dashboard worth
building needs durable history. Both are storage questions that outgrew this
document; they are recorded in [`storage.md`](storage.md), which evaluates
SQLite, Turso, and PostgreSQL and adopts none of them.

Two conclusions from there bear on the decisions below:

- **History is a prerequisite, not a feature.** Everything the dashboard can show
  today is a point-in-time value in memory, so a store is what separates a
  dashboard from a status page. That argues for keeping Decision 4's frontend
  work independent of the store decision, so neither blocks the other.
- **Single-instance is not created by the UI.** shunt's gateway device-flow
  rendezvous already needs cross-replica state and does not have it. A dashboard
  only makes that visible.

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
  permits.
- With `[server.gateway]` enabled, `/admin/login` no longer shares a socket with
  intentionally unauthenticated routes (`/oauth/token`, `/device`,
  `/.well-known/oauth-authorization-server`), which simplifies any fronting
  proxy's allowlist.
- Under several instances it sidesteps the session and pending-login problems
  structurally rather than papering over them: an admin listener on a management
  interface is addressed per replica, so nothing depends on a load balancer
  honoring session affinity.

**Constraint — the second listener must re-install the shared layers, and access
control is not optional.** `build_router` merges `admin::admin_router()` at
`src/server.rs:214` and then wraps the *whole* router in two layers: the inbound
concurrency gate (`limit_requests`, `:265`, when `max_concurrent_requests > 0`)
and `enforce_http_tuning` (`:276`), which carries both `[server.access_control]`
allow/deny CIDR rules and the `[server.limits]` header and URL caps. Moving
`/admin/*` out of that router therefore drops both — and dropping the second one
is a **fail-open access-control regression** for an operator who already relies on
`[server.access_control]` to restrict who may reach the admin surface. The
previous bullet frames losing the concurrency gate as a benefit, and it is; losing
the CIDR rules is not the same kind of change and must not ride along with it. So
an implementation of this decision must install an equivalent access-control
layer on the admin listener — never leave it bare. The admin listener inherits
`[server.access_control]` rather than getting rules of its own
([Resolutions](#resolutions), item 7); that it must not be bare was never in
question. Inheriting has an operational cost of its own — those rules are then
evaluated against the admin listener's peers, so a loopback `bind` needs the
loopback CIDRs in the allow list — which item 7 covers.

Note that the body limit is *not* part of that layer — `:273-275` records that it
is read per handler so a reload can hot-apply it — so a second listener does not
change body-limit behavior either way.

Whether the route tree is registered stays a **boot-time** decision, as M9
already specifies for `[server.admin]`; a reload that adds or changes `bind` logs
that it needs a restart.



**Implementation hazard.** `shutdown::shutdown_signal()` must not be called
twice. Its documentation explains why: tokio delivers signals through a
process-wide channel with no queue, and the current implementation deliberately
keeps one listener continuously live to close a delivery gap between waits. A
second listener must therefore be driven by a fan-out from a single
`shutdown_signal()` call, not by a second call. `tokio::sync::watch` is the
zero-dependency spelling; `futures_util::FutureExt::shared` is also already
available (`futures-util` is a direct dependency), and
`tokio_util::sync::CancellationToken` is the most idiomatic of the three but
would mean promoting `tokio-util` from a transitive dependency to a direct one.
Any of them works — the load-bearing constraint is *one* call, fanned out.

## Decision 3 — three-way path split; never a catch-all at `/`

| Namespace | Contents | Contract owner |
| :-- | :-- | :-- |
| `/admin/*` | operator UI: HTML shell, SPA client routes, `/admin/assets/*` — minus the server-rendered pages that stay (`/admin/login`, `/admin/oidc/callback`), below | shunt |
| `/admin/api/*` | shunt-specific JSON: pool, status, accounts, observed, provisioning | shunt |
| `/v1/organizations/*` | **reserved** — see below | Anthropic |

### Why the UI and the JSON API must split

`GET /admin/pool`, `/admin/status`, `/admin/accounts`, and `/admin/observed`
return JSON today. Those are exactly the paths an SPA wants as browsable deep
links. Serving both meanings from one path requires content negotiation on
`Accept`, which breaks bookmarking and is fragile under any intermediary that
rewrites headers.

The JSON endpoints move to `/admin/api/*`, and the current paths are **removed,
not aliased** ([Resolutions](#resolutions), item 6). M9 documents them as a
curl-able surface and scripted callers exist, so the removal ships with a
path-migration table in `endpoints.md` rather than a compatibility shim, its
`BREAKING CHANGE:` commit footer pointing there.

**An alias and a deep link cannot share a path**, which is why aliases were
rejected. While `GET /admin/pool` still answers JSON, the SPA cannot claim
`/admin/pool` as a browsable route — that is the same collision this split exists
to remove, just deferred rather than resolved, and every alias would block the
deep link of the same name until it was retired. Hash routing (`/admin/#/pool`)
and a separate SPA prefix (`/admin/ui/pool`) dodge the collision without retiring
anything, at the cost of a permanently uglier URL for the surface an operator
looks at most. The clean break removes the collision instead of deferring it.

**The set of paths that must move is every registered `/admin/*` path, not only
the JSON `GET`s.** A `fallback` answers unmatched *paths*; a request whose path
matches a route registered for other methods gets axum's `405 Method Not Allowed`
instead, so a POST-only or DELETE-only path is just as unavailable to the SPA as
a `GET` alias is. Reading `admin_router` (`src/admin/mod.rs:115-146`), these are
the registered paths that move, and why each would have blocked a deep link had
it stayed:

| Path | Registered | Why it would have blocked a deep link |
| :-- | :-- | :-- |
| `/admin/pool`, `/admin/status`, `/admin/accounts`, `/admin/observed` | `GET` | Answers JSON on the path the SPA wants. |
| `/admin/accounts/codex` | `GET` + `POST` | Same JSON collision — belongs with the four above. |
| `/admin/accounts/claude/{name}`, `/admin/accounts/codex/{name}` | `DELETE` only | `405`, not the shell. An account **detail view** is the most natural deep link the UI will want, and both of its path shapes are taken. |
| `/admin/accounts/claude` | `POST` only | `405` on the "add account" screen's natural URL. |
| `/admin/accounts/claude/{name}/complete`, `/admin/accounts/codex/{name}/complete` | `POST` only | `405`. |
| `/admin/oidc/start`, `/admin/logout` | `POST` only | `405`; unlikely SPA routes, listed for completeness. |

`/admin` (`GET`) is the shell itself and `/admin/login` (`GET`+`POST`) and
`/admin/oidc/callback` (`GET`) are server-rendered pages the SPA does not
replace, so those three stay where they are and never blocked anything.

The resolution covers the method-only paths because the mutations move under
`/admin/api/*` as well, not only the four JSON `GET`s. Once they do, a browser
`GET` on a former mutation-only path — `/admin/accounts/claude`,
`/admin/accounts/claude/{name}` — is an SPA route rather than a `405`, so the
account screens need no method-aware escape of their own. That is exactly what a
retirement limited to the JSON `GET`s would have missed: it would have unblocked
the four list views and left every account screen answering `405`.

### Why not the root

Mounting the UI at `/` with an SPA fallback — the shape a naive reading of
Coder's layout suggests — is closed by three separate facts:

1. `/` is already a handler that also answers `HEAD` for liveness probes.
   Replacing it changes behavior for any deployment probing it.
2. A catch-all fallback would make unmatched paths return HTML `200` instead of
   `404`. That breaks client error handling and, when
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
  topology section says scaling out needs. See [`storage.md`](storage.md); the
  two should be planned together, not separately.
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

The trade-off was **whether `cargo build` may require a Node toolchain**, and it
is settled: it may not. Of the three options below the feature gate is chosen
([Resolutions](#resolutions), item 1); the table stays as the record of what was
weighed.

| | Feature-gated (`--features ui`) | Commit `dist/` | `build.rs` with a prebuilt fallback |
| :-- | :-- | :-- | :-- |
| `cargo build` without Node | works, no UI | works, UI included | works, UI from the fetched artifact |
| Source-of-truth risk | none | built output can drift from source | none — the artifact is built from a tagged source |
| CI | builds assets for release | must verify the committed build reproduces | publishes the artifact per release |
| Default build has a dashboard | no | yes | yes, when the fetch succeeds |
| Cost | none | repo bloat, conflicts on built files | a network fetch in `build.rs`, and a supply-chain surface to secure |

The third option builds locally when Node is present and otherwise fetches a
release artifact. It removes the drift risk that makes committing `dist/`
unattractive without giving up a dashboard in the default build — but a
`build.rs` that reaches the network during `cargo build` is itself a
supply-chain surface, so it trades a repo-hygiene problem for a
verify-the-download problem. Offline builds also need the fetch to be skippable.

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
- **Breaking migration.** Removing the legacy `/admin/*` JSON and mutation paths
  breaks every scripted caller M9 documented. The path-migration table lives in
  `endpoints.md`, and the breaking change must be declared in a commit
  `BREAKING CHANGE:` footer that points at it — the release notes here are
  release-please prose built from commit footers, which cannot carry a table and
  never see a footer written only in a PR body (issue #270). The removal must
  land in one release rather than piecemeal.
- **Second listener, second auth path.** The M9 session cookie is scoped
  `Path=/admin`, and its `Secure` flag keys off request-host loopback-ness. A
  separate admin listener changes the host a browser sees; the cookie scoping
  needs re-verification under that topology, not assumption.
- **The dashboard makes single-instance-ness visible.** Today an operator can run
  several shunt processes and not notice the per-replica pool view, because the
  pool is observable mainly through a response header and logs. A dashboard that
  presents pool health as *the* answer invites the misreading. Whatever the UI
  shows must be labeled as this instance's view.

## Resolutions

The seven questions this document originally left open are now decided:

1. **Assets: feature-gated (`--features ui`).** A default `cargo build` requires
   no Node toolchain and produces no dashboard; release CI enables the feature,
   so release binaries carry the UI. No committed `dist/`, no network access in
   `build.rs` — the drift and supply-chain costs in Decision 4's table are
   avoided entirely, at the price that table already names: a from-source build
   without the feature has no dashboard — a gap release-binary users never
   encounter.
2. **Frontend: React + Vite, in its own toolchain.** The SPA lives in its own
   package with its own lockfile, separate from `site/`'s Astro/Nimbus toolchain
   — the two serve different purposes and upgrade on different schedules. The
   Tauri shell ([Desktop](#desktop)) loads this same bundle.
3. **No TLS on the admin listener.** `[server.admin].bind` is a management
   interface, loopback by default; exposing it beyond loopback assumes a
   fronting proxy that terminates TLS. No certificate configuration enters
   shunt.
4. **`shunt ui` opens the browser and offers setup.** With `[server.admin]`
   configured it resolves the URL and token file and opens the browser. When the
   surface is not configured it does not silently edit config: it offers to run
   `shunt dashboard setup` and proceeds only on confirmation.
5. **Single-instance is a limitation, not a decision.** Scaling out stays on the
   roadmap and is owned by [`storage.md`](storage.md); the `/v1/organizations/*`
   reservation is accordingly a roadmap item, not a non-goal.
6. **No aliases — the admin JSON moves in one breaking change.** Every JSON
   endpoint and every mutation under `/admin/*` outside the server-rendered
   login flow (`/admin/login`, `/admin/oidc/callback`) moves to `/admin/api/*`
   at once (`feat!`, its `BREAKING CHANGE:` commit footer pointing at a
   path-migration table in `endpoints.md`); the legacy paths are removed, not
   aliased. `/admin` itself stays for a different reason than those two — it is
   the shell the SPA takes over, not an endpoint with anywhere to move — which
   completes Decision 3's three. The SPA therefore claims clean `/admin/*` deep
   links from the start, and Decision 3's blocked-path table becomes the
   migration inventory rather than a constraint.
   Hash routing and a separate `/admin/ui/*` prefix were rejected as permanent
   URL costs that only defer the collision. Scripted callers migrate once —
   acceptable while the crate is 0.x.
7. **The admin listener inherits `[server.access_control]` — always.** No
   dedicated admin-scoped block: it is the smaller change, preserves today's
   behavior, and keeps one set of CIDR rules to reason about. Admin-scoped
   *credential* tiers are a separate proposal (#346) and not this listener's
   job. A bare listener remains a fail-open regression (Decision 2's
   constraint), and its regression test under [Testing](#testing) stays.

   **What inheriting costs: the rules are then evaluated against the admin
   listener's own peers.** `AccessControlConfig::allows`
   (`src/config/http_tuning.rs:39-61`) treats a non-empty `allow_cidrs` as
   default-deny, and `enforce_http_tuning` (`src/http_tuning.rs:33-64`) exempts
   only `/` and `/health`, resolving the peer from the connection's
   `ConnectInfo<SocketAddr>`. So an operator whose allow list names their
   gateway clients' subnet, and who then splits admin onto the recommended
   loopback `bind`, gets `403` on every browser request — local or
   SSH-forwarded — until `127.0.0.1/32` and `::1/128` join the list. That fails
   closed, not open, but silently: a page that worked before the split answers a
   flat `permission_error`. An implementation should diagnose it at boot — warn,
   or refuse to start, when the inherited rules cannot admit the admin
   listener's bind address — rather than leave the operator to find it one
   `403` at a time.

## Testing

- A `build_router` smoke test with **every optional surface enabled at once**,
  asserting no panic and that each namespace resolves. This closes the gap under
  Risks and is worth landing before any UI route, independent of the rest of
  this design.
- Path-inventory assertion: the set of registered paths matches this document's
  table, so a new route cannot be added without the conflict review.
- `/` still answers `HEAD` after any UI work.
- An unmatched path **outside the `/admin` mount** (`/v1/nope`, `/nope`) still
  `404`s rather than returning HTML. Scope matters: once Decision 3 adds an SPA
  fallback, an unmatched path *under* `/admin` is exactly what must return the
  shell, so a blanket assertion would either fail the intended deep-link
  behavior or force the admin SPA back to `404`s. The property this protects is
  that the UI fallback stays confined to its mount — which is the
  [Why not the root](#why-not-the-root) argument stated as a test.
- Mutations live only under `/admin/api/*`, so a browser `GET` on
  `/admin/accounts/claude` or `/admin/accounts/claude/{name}` returns the SPA
  shell, and the mutations answer only on their `/admin/api/*` paths. Asserting
  both halves is what stops the method-only blockers from being rediscovered
  once the account screens are built.
- With `[server.admin].bind` set, admin paths 404 on `server.bind` and serve on
  the admin listener — and the reverse when it is unset.
- With `[server.admin].bind` set **and** a `[server.access_control]` deny rule
  covering the client, the admin listener still denies the request. This is the
  regression test for the fail-open in Decision 2's constraint; without it, an
  implementation that simply forgets the layer passes every other test here.
- The companion to that one: with `[server.admin].bind` on loopback and an
  `allow_cidrs` that omits the loopback CIDRs, boot warns (or refuses to start),
  and a build that warns and serves anyway denies the loopback browser. The same
  inheritance that must not fail open must not silently lock the operator out
  either, and only asserting both halves distinguishes the two.
- Graceful shutdown drains both listeners from a single signal.
- Embedded assets: the bundle is non-empty (a build that silently embedded
  nothing must fail, not serve a blank page), `/admin/assets/*` returns the
  file's bytes, and each response carries the `Content-Type` its extension
  implies — `text/javascript` for `.js`, `text/css` for `.css`, and so on. A
  wrong or missing type is not cosmetic: browsers refuse a stylesheet or module
  script served as `text/plain`, and `X-Content-Type-Options: nosniff` removes
  the sniffing that would otherwise mask the bug.

## Documentation impact

- `site/src/content/docs/reference/configuration.md` — `[server.admin].bind`.
- `site/src/content/docs/reference/endpoints.md` — the namespace split, the
  canonical `/admin/api/*` paths, and the removal of the legacy `/admin/*` JSON
  and mutation paths (breaking), including the path-migration table the
  release's `BREAKING CHANGE:` footer points at.
- `site/src/content/docs/reference/cli.mdx` — `shunt ui`, next to
  `shunt dashboard setup`.
- [`m9-admin-surface.md`](m9-admin-surface.md) — its endpoint table becomes the
  pre-split record; add a pointer here rather than rewriting it. It is also
  missing `GET /admin/status`, which `admin_router` has registered since the
  route was added — an existing drift, worth a one-row fix independently of this
  design.
- `docs/running.md` — the single-instance topology statement belongs in the
  operational guide, not only in this design record.
- `README.md` — only if the dashboard becomes a headline capability.
