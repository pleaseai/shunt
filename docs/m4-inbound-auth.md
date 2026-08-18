# M4 — Inbound client authentication (shared-gateway tokens)

## 0. Problem

shunt has no inbound authentication. For **passthrough** providers that is correct — the
caller's own Anthropic credential is forwarded, so every caller pays for themselves. But for
providers where shunt **injects a server-side credential** (`auth = "api_key"` or
`auth = "chatgpt_oauth"`), any client that can reach the listener spends the operator's
account. That is fine for a loopback-only personal gateway; it is not fine once the gateway
is shared with other people over a VPN / tunnel.

M4 adds an **optional, per-client token check** on exactly those injected-credential routes.
Transport security stays out of scope: shunt still serves plain HTTP and relies on the
deployment (WireGuard, Tailscale, Cloudflare Tunnel TLS, loopback) for encryption.

## 1. Configuration

New optional `[server.auth]` table. Absent ⇒ behavior unchanged (no inbound auth).

```toml
[server]
bind = "0.0.0.0:3001"
default_provider = "anthropic"

[server.auth]
# Header carrying the client token. Optional; default "x-shunt-token".
header = "x-shunt-token"
# Env var holding the client tokens. Optional; default "SHUNT_CLIENT_TOKENS".
# Value format: comma-separated `name:token` pairs, e.g.
#   SHUNT_CLIENT_TOKENS="minsu:3f9c…,alice:a41b…"
# Names are labels for logging only; tokens are the secrets.
tokens_env = "SHUNT_CLIENT_TOKENS"
```

Rules:

- Tokens live in the **environment**, never in the TOML (matches `api_key_env`).
- `[server.auth]` present but env var unset/empty ⇒ **startup error** (fail closed at boot,
  like the existing config validation — never silently run open when auth was requested).
- Parse errors (entry without `:`, empty name or token, duplicate name) ⇒ startup error.
- Token value = everything after the **first** `:` (tokens may contain `:`). Surrounding
  whitespace around entries is trimmed; whitespace inside a token is preserved.

## 2. Enforcement

Checked in the `/v1/messages` and `/v1/messages/count_tokens` handlers **after routing
resolves the provider**, and only when that provider's `auth` mode injects a server-side
credential (`ApiKey`, `ChatgptOauth`, `ClaudeOauth`, …). `GET /v1/models` is checked
whenever `[server.auth]` is configured, because it exposes the configured model list:

- `Passthrough` provider ⇒ no check (caller uses their own credential), regardless of config.
- Both the injected-credential inference gate and `GET /v1/models` accept the client token
  in any of the standard Anthropic credential slots, with an explicit priority when several
  carry valid tokens: the configured header (default `x-shunt-token`), then
  `Authorization: Bearer <token>`, then `x-api-key`. A Claude Code client on a pool-only
  gateway therefore authenticates with the `ANTHROPIC_AUTH_TOKEN` it already sends — no
  `ANTHROPIC_CUSTOM_HEADERS` line needed. (Before #130 the inference gate accepted only the
  configured header; discovery gained the wider set in #90.)
- `HEAD /` and `GET /routes` ⇒ never checked (`/` is liveness; `/routes` remains shunt-native metadata).
- Injected-credential route with valid token ⇒ proceed; log the client **name** (never the
  token) as a tracing field on the request span / relevant log lines.
- Missing or unknown token ⇒ `401` with an Anthropic-shaped error body:

```json
{"type":"error","error":{"type":"authentication_error","message":"missing or invalid credential: this gateway requires a client token (via x-shunt-token, Authorization: Bearer, or x-api-key) for mapped models; ask the operator for one"}}
```

  (message uses the configured header name; a `warn` log records the failure and the
  provider, never the presented token value.)

Safety boundary: a credential accepted as a gate token must never be forwarded upstream.
On a gated (injected-credential) route the gateway strips `authorization` and `x-api-key`
from the forwarded headers after a successful check — the injected-credential adapters
replace those headers with the provider credential anyway, but the boundary must not
depend on adapter behavior. Passthrough routes are never gated, so the client's `Bearer` /
`x-api-key` are forwarded as the caller presented them — except a slot that holds a credential
that gated this request's admission, whether a static `[server.auth]` token or shunt's own
`[server.gateway]` JWT, which is cleared from that slot regardless of gating (see below).
Operators mixing passthrough and mapped models on one shared gateway can still keep handing
out dedicated `x-shunt-token` values as good hygiene — it keeps the `Bearer` slot free for the
real Anthropic credential on passthrough models by convention — but it is no longer load-bearing
for the safety boundary: the by-value check below enforces it even when a client delivers the
gate credential through a rotating mechanism such as `apiKeyHelper`, which puts the same value
in **both** `Authorization` and `x-api-key` (that is
[how the credential variable maps to a header](https://code.claude.com/docs/en/llm-gateway-connect#how-the-credential-variable-maps-to-a-header)
and leaves no free slot to move it out of).

Both credential kinds hit the same mixed-chain scenario: "gated" is decided per route
**chain**, not per route, so a chain mixing mapped and passthrough entries is authenticated by
the gate credential and then reaches its passthrough attempt with the caller's headers intact.
A purely passthrough chain is never gated at all, so the gate credential (if the caller sends
one anyway) can still land in either slot there too. Either way, each slot is checked by the
value it holds rather than by whether the request was gated:

On a same-origin passthrough attempt, `authorization` and `x-api-key` are checked
independently, by what each slot actually holds: a slot is cleared if its own value is either
shaped like a JWT shunt itself issued or matches a configured `[server.auth]` static token, via
the shared `auth::inbound::consumed_by` check used identically on the passthrough attempt of a
chain (`proxy/failover.rs`) and in upstream model discovery (`discovery/upstream.rs`). The
gateway-JWT half of that check (`jwt::has_shunt_shape`) is deliberately by shape — three base64url
segments whose payload's `aud` claims `"shunt"`, whose `iss` claims this gateway's `public_url`, or
whose `shunt_token_use` claim is `"gateway-session"`, a dedicated marker that only shunt mints — not
by whether the token currently authenticates: a do-not-forward decision has to ask "did shunt
issue this?", not "is this valid right now?". An expired token, one minted by a sibling instance
under a different `public_url` (a fleet sharing one `jwt_secret` across differing `public_url`
values — the case a strict authenticate-only check misses, since it is still live), or one that no
longer verifies after a `jwt_secret` rotation is still shunt's own credential and must still be
stripped; forwarding it leaks the caller's identity in the unencrypted payload and hands a
third-party upstream a valid (message, tag) pair over `jwt_secret` as an offline oracle. `consumed_by`
therefore checks `authenticate_token` first (so the `GatewayJwt` reason label keeps meaning "this
authenticated the caller" whenever it can) and falls back to the shape check only if that fails.
Forging `aud`/`iss` to force a strip only removes the forger's own credential — the check is
per-value, never per-request — so the fail-safe direction is correct. The marker claim is an
additional arm on that shape check, never a required one: a token minted before the marker
existed still matches by `aud`/`iss`, and `verify` itself does not require the marker either, so
such a token still authenticates within its TTL — requiring it would reject still-live tokens from
an older shunt version, trading a silent leak-prevention gap for a loud, self-inflicted logout
regression. Precision only improves as pre-marker tokens age out, without ever narrowing the check
in the direction that matters. The
`authorization` slot is evaluated in **both** shapes it can carry a gate credential in — the
`Bearer <token>` payload, and the entire header value — because `[server.auth] header` is a
free-form header name that an operator may set to `authorization`, in which case
`authenticate_client` gates on the whole unprefixed value and a payload-only check would relay
the gate token itself upstream (`auth::inbound::authorization_consumed_by`). The origin
filter runs first — an off-origin failover attempt strips both slots outright before this
by-value check ever runs; the by-value check applies only to whichever slots the origin filter
retained. Within a same-origin attempt, the other slot's presence never triggers a strip by
itself. Consequence to expect: a caller who presents the gate credential in one slot and a
genuine upstream credential in the other still has that upstream credential forwarded on a
same-origin attempt — only the gate-credential-bearing slot is cleared. A gate credential with
no accompanying upstream credential in either slot still falls back to the builtin catalog for
discovery, since no forwardable credential remains.

Caveat for `[server.auth] header = "authorization"` (or `"x-api-key"`): on the **inference**
path `check_inbound_auth` removes the configured header from the forwarded map for *every*
request, before any route handling, so that slot never reaches the by-value check and never
carries a caller's own upstream credential to a passthrough route — whether or not it holds a
gate token. The by-value check is what covers the slot on the **discovery** path, which passes
the request headers through unmodified. The consequence is that pointing `header` at a slot
callers also need for their real credential costs them that slot on inference; the default
dedicated `x-shunt-token` avoids the collision entirely. Over-stripping is the deliberate
direction here — the alternative, deciding per value at the gate, would forward a slot the gate
had already accepted under some configurations.

### 2.1 The slot enumeration and its mirror invariant (#363)

Four consecutive fixes (#352, #357, #361, #356) landed the *same* defect: a credential shunt
accepts as its own gate token reached a third-party upstream, because one enumeration of a slot
was missed while an accept predicate widened or a credential kind was added. The rule that was
being broken each time is stated once here, and implemented once in **`src/auth/slots.rs`**:

> **Mirror invariant.** If any accept site would authenticate value `V` presented in slot `S`,
> then every forward site must remove `V` from `S`.

The strip predicate is the mirror image of the accept predicate — not an approximation of it,
and never a per-request boolean.

The accept table is **exhaustive for header slots**, which is the scope the invariant is about:
a header is the only channel a forward site copies from the caller's request into an upstream
request.

| Accept site | Header slots it reads |
| --- | --- |
| `InboundAuth::authenticate` | `[server.auth] header`, raw |
| `InboundAuth::authenticate_bearer` | `[server.auth] header` raw, `Authorization: Bearer` payload |
| `InboundAuth::authenticate_client` | `[server.auth] header` raw, `Authorization: Bearer` payload, `x-api-key` raw |
| `GatewayAuth::authenticate_bearer` / `authenticate_token` | `Authorization: Bearer` payload / a bare token value (reached in production only through that bearer path and through `consumed_by`) |
| `AdminAuth::authenticate_credential` | `[server.admin] header` raw **and** `x-api-key` raw, over `write_keys`, `read_keys`, and the legacy `tokens_env`/`tokens_file` pairs alike |
| `admin::authenticate` → `session_cookie` | the `cookie` header — a **write-tier** `shunt_admin_session` accepted when no credential header matched |

shunt also accepts its own values, and admin credentials, out of **form bodies and query
strings**: `admin::login_submit` (a write-tier admin credential in a form field, via
`authenticate_login_token`), `gateway::oauth`, `gateway::device`, `gateway::idp`, `admin::oidc`,
and `auth::callback`. None of them needs a strip, and the reason is structural rather than a rule
anyone has to remember: no forward site copies an inbound body or query string into an outbound
request. Every upstream URL is rebuilt from config (`responses_url` and friends), and the body a
forward site sends is the caller's inference payload, which never carries these values. They are
recorded so the enumeration is honest about its scope, not because they are a risk.

| Forward site | What it does |
| --- | --- |
| `proxy::failover::check_inbound_auth` + `headers_for_route` | reserved names, then the by-value strip on the same-origin passthrough branch. Every inference adapter receives only what `headers_for_route` produced, so the pair is the single choke point for `/v1/messages`. |
| `discovery::upstream::upstream_headers` (`AuthMode::Passthrough`) | builds a fresh map holding only the two shared slots, each judged by value |
| `adapters::responses::inbound::passthrough_request_headers` | relays the client's headers verbatim minus a strip list, then `strip_reserved_slots` |

`gateway::telemetry_ingest`'s relay, `adapters::responses::request`, and
`adapters::responses::codex_ws::connect` are deliberately *not* forward sites: each builds its
outbound header map from an allowlist, so no caller header can cross.

`ShuntCredentials::from_state` is the single wiring point from request state — and, since the
struct is neither `Default` nor built from public fields, the only production constructor — so a
credential kind added to `AppState` cannot be picked up by the accept path while a forward site
that hand-rolled its own field list silently misses it. An all-`None` value would strip nothing
at all, so making one unconstructible outside tests also means each forward site provably passes
the real request state.

"One enumeration" is true of the **credential-table wiring**, not of the slot-name strings: the
accept side still spells `"x-api-key"` literally in `auth/inbound.rs` and `admin/mod.rs`. Those
two remain decoupled from `SHARED_SLOTS`.

Two behavior changes came with the consolidation:

- **The reserved names are now stripped unconditionally.** `x-shunt-token`,
  `x-shunt-admin-token`, and `x-shunt-inbound-client` are removed on every forward, whether or
  not the corresponding `[server.auth]`/`[server.admin]` table is configured and whether or not
  the endpoint is gated. None of them is ever a legitimate upstream header, so removing a name
  a client sent cannot break a legitimate relay — the argument the Codex strip list already made
  for `x-shunt-token` alone.
- **The `cookie` header is now stripped outright on every forward.** `admin::authenticate`
  falls back to `session_cookie`, which reads a write-tier `shunt_admin_session` out of `cookie`,
  making it an accept slot the first version of this enumeration missed — and two of the three
  forward sites relayed it verbatim (`headers_for_route` starts from a clone of the caller's map
  on both branches; the Codex strip list had no `cookie` entry). Whole-header removal is safe
  because shunt keeps no cookie jar: `Cargo.toml` builds reqwest **without** the `cookies`
  feature and nothing in `src/` constructs a `cookie_store`/`cookie_provider`, so shunt never
  participates in upstream edge or affinity cookies (`__cf_bm`, `cf_clearance`). The mirror
  direction already made this call — `PASSTHROUGH_STRIP_RESPONSE_HEADERS` strips
  `set-cookie`/`set-cookie2` on the way back for the same reason. A surgical
  `shunt_admin_session=` pair parser was rejected: it would have to track `session_cookie`'s own
  parse and would reintroduce exactly the accept/strip drift this module exists to eliminate.
  The accepted cost is that a benign `cookie: theme=dark` is dropped too; the tests assert that
  over-strip explicitly so it is a recorded decision.
- **The `[server.admin]` header gap on the Codex endpoint is closed.** The inbound Codex
  passthrough stripped `authorization`, `x-api-key`, `x-shunt-token`, `x-shunt-inbound-client`,
  and the configured `[server.auth] header` — but not the `[server.admin] header` (default
  `x-shunt-admin-token`). An admin credential presented in that slot on a
  `[server.codex_endpoint]` route was relayed verbatim to the ChatGPT backend, even though
  `AdminAuth::authenticate_credential` authenticates on exactly that slot. It is the
  highest-value credential shunt holds: it can provision upstream accounts.

The asymmetry between the two configurable headers is preserved exactly as described in the
caveat above: `strip_reserved_slots` removes `[server.auth] header` **always**, even when it
names `authorization` or `x-api-key`, while it removes `[server.admin] header` only when that
name is *not* one of the two shared slots — dropping a shared slot outright for the admin header
would delete a genuine caller credential sight unseen, so that case is handled by value in
`strip_consumed_slots` instead.

**Tests.** `src/auth/slots/tests.rs` encodes the invariant rather than one scenario at a time: it
walks the cross product of every credential kind (a verifying gateway JWT, a shunt-shaped JWT
that no longer verifies, a `[server.auth]` token, a `write_keys` entry, a `read_keys` entry, a
legacy `tokens_env` pair) against every delivery shape (`Authorization: Bearer <v>`,
`Authorization: <v>`, `x-api-key: <v>`, the configured auth header, the configured admin header),
computes acceptance by calling the **real** accept predicates rather than a hand-written table,
and asserts the value appears in no header of any forward site's output. It runs under two
configurations — the default header names, and one that points `[server.auth] header` at
`authorization` and `[server.admin] header` at `x-api-key`, because only the latter makes the raw
`Authorization` shape an accepted credential at all, which is what #361 missed. Non-vacuity is
guarded by a hard-coded count of accepted pairs per configuration, by a non-empty-output check
per site, and by a control credential (`sk-ant-genuine-upstream-key`) that must *survive*.

**Tripwire.** `every_header_producing_site_is_classified` walks `src/**/*.rs` and asserts that the
set of files which either bulk-apply a header map to an outbound request *or* declare a function
returning `HeaderMap` equals a hard-coded 9-entry allowlist, so a new relay path must be
classified — registered forward site, allowlist-built map, or test scaffolding — rather than
merely compile.

The type-signature half is what makes it useful. A bulk-application-only scan left **both**
hand-rolled forward sites invisible: `discovery/upstream.rs` feeds its map to a
`request.header(k, v)` loop, and `proxy/failover.rs` returns a clone of the caller's map. A new
site written by copying either one escaped detection entirely. Matching a site by what it
*produces* catches it however it builds the map. (`.header(`, `.insert(`, and `HeaderMap::new()`
were measured as alternatives and are unusable — 38, 71, and 24 files respectively.)

Residual hole, narrower than the bulk-only version but still real: a site that mutates a request
in place and never returns a `HeaderMap` — the shape `codex_ws::connect` has — is caught only by
the extend-into-`headers_mut` pattern, so a different in-place idiom would slip through. Files
named `tests.rs` are skipped so fixtures need no entry; an in-file `#[cfg(test)] mod tests`
helper is not skipped and is allowlisted as noise.

Considered and deferred: a `SanitizedHeaders` newtype that only these methods can produce, making
the strip a compile-time obligation rather than a convention a tripwire polices. It is a real
improvement, but the three sites hand their map to three different HTTP clients, so the churn is
larger than this change should carry.

## 3. Comparison & hygiene

- **Constant-time comparison**, no new dependency: compare presented token against every
  configured token with a length check folded into a byte-XOR accumulator (compare against
  all entries even after a match to keep timing independent of position).
- The auth header is **always stripped** before forwarding upstream (add it to the strip
  logic beside `HOP_BY_HOP_HEADERS` in `src/headers.rs` — dynamic name, so a function that
  takes the configured header name rather than a const entry).
- Never log token values at any level, including debug.

## 4. Client setup (docs)

Document in `docs/running.md` (new §5 subsection) and `shunt.toml.example`:

```bash
# Claude Code side — ANTHROPIC_CUSTOM_HEADERS supports one "Name: Value" per line
export ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <your token>"
```

Note the composition guidance: transport encryption comes from the tunnel (WireGuard /
Tailscale / Cloudflare Tunnel); the token distinguishes and revokes **users** on top.

## 5. Tests

Pure unit tests (no network, no loopback bind — Codex-sandbox safe):

- token env parsing: happy path, token containing `:`, whitespace trimming, and the
  startup-error cases (missing env, empty, duplicate name, malformed entry).
- constant-time equality helper: equal / unequal / different-length.

Integration tests (wiremock, alongside the existing suites):

- mapped (injected-credential) route, auth configured, no token ⇒ 401, upstream never called.
- mapped route, wrong token ⇒ 401.
- mapped route, valid token ⇒ 200, upstream called, **auth header absent** from the
  forwarded request.
- passthrough route, auth configured, no token ⇒ still forwarded (unchanged behavior).
- auth not configured ⇒ mapped route works without a token (backward compat).

Cross-cutting (added by #363, see §2.1): `src/auth/slots/tests.rs` asserts the mirror invariant
over the whole credential-kind × delivery-shape cross product at every registered forward site,
and holds the tripwire that forces a new header-producing site to be classified — whether it
bulk-applies a header map or merely declares a function that returns one. Matching only the
bulk-application idioms left both hand-rolled forward sites invisible.

## 6. Out of scope

- TLS termination, OIDC/SSO (deployment-layer concerns; see running.md guidance).
- Per-client rate limits or spend accounting (a possible M5; the `name` label in logs is the
  hook for it).
- Interactive token minting — operators generate tokens themselves (e.g. `openssl rand -hex 32`).
