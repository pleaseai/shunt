# M9 — Opt-in admin web surface

M9 adds an opt-in, admin-authenticated web surface to shunt so an operator can
provision upstream Claude accounts from a browser and observe account-pool health
without shell access. It builds directly on M8
([`m8-anthropic-multi-account.md`](m8-anthropic-multi-account.md)): the store, the
per-request account resolution, and the `AccountPool` quota/cooldown state all
already exist — M9 only adds an HTTP surface over them.

The surface is deliberately co-designed to share its foundations (session auth,
server-rendered page + CSRF convention, a single-use pending-login store, one
`[server.admin]` opt-in) with the planned Claude-apps gateway-login milestone,
which is *inbound* (client → shunt authorization server) where this feature is
*outbound* (operator provisions shunt → Anthropic upstream accounts). M9 lands
first and stands alone; gateway login builds on the same session/page layer
rather than growing a second stack.

## Motivation

After M8, the only way to add an account to a deployed shunt is SSH (or
`docker exec`) plus the interactive `shunt login claude --name <n> --long-lived`
paste flow — the router exposed only `/`, `/health`, `/protocol`, `/v1/models`,
`/routes`, and the two `/v1/messages*` endpoints. Pool health (per-account quota
utilization, cooldowns) was observable only through the `x-shunt-account`
response header and logs. M9 relocates provisioning from a TTY to a browser form
and surfaces the pool state that already lives in memory.

## Scope

- **Setup-token flow only.** The browser flow is the web equivalent of
  `shunt login claude --name <n> --long-lived`: an inference-only, one-year PKCE
  setup token. The import flow (a refreshable Claude Code login) stays CLI-only.
  This also sidesteps the cross-process refresh-token rotation hazard by
  construction — web-provisioned accounts are static tokens that are never
  refreshed (see #73).
- **Full CRUD.** Add (provision), list (metadata only), remove (delete the store
  file), and replace (re-run the flow with the same name, which the store already
  supports).
- **Read-only pool dashboard.** A JSON endpoint plus a table, over the state
  `AccountPool` already tracks. No new state collection.

## Configuration

A new `[server.admin]` block under `[server]`. **Absent ⇒ no admin routes are
registered at all** — the default HTTP surface is unchanged. Present ⇒ the routes
exist and authenticate every request.

```toml
[server.admin]
# header carrying the admin token for API/curl calls
header = "x-shunt-admin-token"
# env var holding admin credentials as name:token pairs (SEPARATE from
# [server.auth] client tokens)
tokens_env = "SHUNT_ADMIN_TOKENS"
session_ttl_secs = 3600   # browser session lifetime after login
pending_ttl_secs = 600    # time to open the authorize URL and paste the code back
```

```bash
export SHUNT_ADMIN_TOKENS="ops:3f9c…"   # comma-separated name:token pairs
```

Admin credentials reuse the inbound-auth token format
([`m4-inbound-auth.md`](m4-inbound-auth.md)) and its constant-time compare, but
are a **separate credential** from `[server.auth]`: client tokens are handed to
devices; admin tokens add upstream accounts. Configuration validation is
**fail-closed** — a present `[server.admin]` whose tokens env is unset, empty, or
malformed is a startup error, never a silently-open admin surface (identical
discipline to `[server.auth]`).

## Runtime wiring

The split mirrors how M4/M8 already separate hot-reloadable config from
process-lifetime state:

- `RuntimeState.admin_auth: Option<Arc<AdminAuth>>` — re-resolved on every reload,
  so admin token/header edits hot-apply just like `[server.auth]`.
- `AppState.admin_stores: Arc<AdminStores>` — the session, pending-login, and
  rate-limiter stores, created once in `build_router` (like `Arc<AccountPool>`)
  and threaded through the per-request snapshot so a reload never drops a live
  browser session.
- Whether the `/admin*` route tree is registered is decided **once at boot** from
  the initial config (a reload cannot add or drop routes, like `server.bind`). A
  reload that toggles the block on or off logs a `warn!` that it needs a restart;
  disabling it on an already-registered surface makes every admin route reject
  requests (`admin_auth` becomes `None`).

## Authentication and hardening

- **Two credentials, never mixed.** Admin auth is the `[server.admin]` credential;
  it is never the `[server.auth]` client tokens.
- **Browser:** sign in at `/admin/login` with an admin token → an opaque session
  id in an in-memory `SessionStore`, set as cookie `shunt_admin_session`
  (`HttpOnly`, `SameSite=Strict`, `Path=/admin`). The cookie is marked `Secure`
  **unless the request host is loopback**, so local HTTP dev and tests work while
  any real deployment host gets a Secure cookie (reusing M8's `host_is_loopback`
  loopback carve-out).
- **API/curl:** send the admin token in the configured header
  (`x-shunt-admin-token`). Header-token callers carry no ambient cookie and are
  therefore **CSRF-exempt**.
- **CSRF** on every cookie-authenticated JSON mutation: a per-session synchronizer
  token, presented as `x-csrf-token`, plus a same-origin check (`Sec-Fetch-Site`,
  falling back to comparing `Origin`'s authority to `Host`). No CORS. `POST
  /admin/logout` is a plain navigation form that cannot send the header, so it is
  guarded by the same-origin check plus the `SameSite=Strict` cookie instead of
  the synchronizer token.
- **Pending-login store** is in-memory only, single-use, and TTL-bound; each
  completion attempt is counted and the entry is discarded after a small cap. The
  256-bit OAuth `state` already makes guessing infeasible.
- **Rate-limit** on the completion and login endpoints (a coarse global fixed
  window each) against code- and admin-token-guessing storms.
- **Secrets never leak:** the verifier and setup token are never logged and never
  returned to the browser. The OAuth `state` is intentionally carried in the
  authorize URL and the opaque session id only in the `HttpOnly` session cookie —
  both are protocol values the browser must receive, not bearer secrets. Account
  add/remove is audit-logged by name only.
- Docs recommend binding the admin surface behind HTTPS / a tunnel, same as the
  shared-gateway guide.
- **Emergency token rotation:** browser sessions are validated only against the
  in-memory session store, so rotating/removing `SHUNT_ADMIN_TOKENS` does **not**
  invalidate already-issued sessions — they persist until `session_ttl_secs`
  (default 1h) expires. If an admin token is compromised, **rotate/remove the
  compromised token first** (so it can no longer mint a new session), then
  **restart the process** (not just a config reload) to drop the sessions it had
  already issued. Rejecting stale sessions on reload is tracked in #100.

## Endpoints (registered only when `[server.admin]` is set)

| Method | Path | Purpose |
| :-- | :-- | :-- |
| `GET` | `/admin` | Dashboard (HTML); redirects to `/admin/login` when not signed in |
| `GET`,`POST` | `/admin/login` | Token login form → session cookie |
| `POST` | `/admin/logout` | Clear the session |
| `GET` | `/admin/accounts` | JSON: store metadata (name, kind, expiry, UUID — never the token) |
| `GET` | `/admin/pool` | JSON: per-`claude_oauth`-provider pool health |
| `POST` | `/admin/accounts/claude` | `{name}` → start provisioning; returns `{authorize_url}` |
| `POST` | `/admin/accounts/claude/{name}/complete` | `{code}` → finish; stores the account |
| `DELETE` | `/admin/accounts/claude/{name}` | Remove the account's store file |

Gateway-owned errors keep the Anthropic error shape (`ShuntError`); page routes
render minimal server-side HTML with inline CSS/JS and no external requests.

## Phase 1 — provisioning flow

The browser flow reuses the CLI setup-token internals in `auth/claude_login.rs`
(`generate_pkce`, `build_authorize_url`, `exchange_code`) and stores via the
already-public `claude_store::store_setup_token`. The only relocation is the
paste: the Anthropic setup-token redirect URI is fixed to
`platform.claude.com/oauth/code/callback`, so a full server-as-redirect-target
flow is impossible for the upstream leg — the operator pastes `<code>#<state>`
into a form instead of a TTY.

1. `POST /admin/accounts/claude {name}` validates the name, generates a PKCE
   verifier/challenge + `state`, stores a single-use pending login (TTL
   `pending_ttl_secs`), and returns the authorize URL
   (`https://claude.com/cai/oauth/authorize`, scope `user:inference`).
2. The operator opens the URL, signs in to the target Claude account, approves,
   and pastes the resulting `<code>#<state>`.
3. `POST /admin/accounts/claude/{name}/complete {code}` verifies `state`
   (constant-time), exchanges the code at the token endpoint (honoring
   `SHUNT_CLAUDE_TOKEN_URL` for tests), and writes the setup token via
   `store_setup_token` (atomic `0600`, UUID captured from the exchange). The
   pending entry is consumed.
4. The completion response reports whether the account is **live immediately** (a
   `claude_oauth` provider with an empty `accounts` list scans the store each
   request) or needs a name-only `[[providers.<name>.accounts]]` entry + reload.

Removal deletes the store file directly, path-guarded so a caller-supplied name
can never escape the accounts directory. This is new writeback behavior over an
operator-owned store file (issue-sanctioned) and touches no upstream state.

## Phase 2 — pool dashboard

`AccountPool::snapshot(provider, &[AccountConfig], model)` returns a token-free,
serializable view per account: 5h/7d/7d_oi utilization + reset, unified status,
cooldown-seconds-remaining, `near_quota`, and a derived `available` flag. It reads
the same `entries` map `select_order` reads, clears only already-past quota
buckets (as the next selection would), never mutates the round-robin cursor, and
never inserts entries for accounts the pool has not yet seen (reported as
`has_state: false`). `AccountPool` tracks no sticky flag or last-selected
timestamp, so the dashboard reports what is actually stored rather than inventing
it. `GET /admin/pool` enumerates each `claude_oauth` provider's accounts (its
configured list, or `claude_store::scan_accounts()` for an empty list — the same
resolution the adapter uses).

## Shared foundations with gateway login

The gateway-login milestone (Claude Code `/login` against shunt) is inbound and
separate, but should reuse rather than duplicate:

- the browser/admin **session-auth layer** — the `/device` approval page needs an
  authenticated human, the same session mechanism as `/admin`;
- the server-rendered **page + CSRF** convention;
- the **`[server.admin]` opt-in** surface — the gateway-login block can nest
  beside it;
- the single-use, TTL-bound **pending store** — the device-flow "pending
  authorization" is the same shape (`session::PendingStore` is written generically
  for this reuse).

## Testing

- Unit: session/pending TTL + single-use + attempt cap, rate limiter, CSRF
  accept/reject, constant-time admin auth, cookie `Secure` loopback carve-out,
  `AccountPool::snapshot`, `claude_store::list_account_meta`/`remove_account`.
- Integration (`tests/admin_surface.rs`): the routes are absent without the block
  (404); API requires auth (401); the full add → complete (wiremock token
  endpoint) → list → pool → delete flow stores the account without ever returning
  the token; cookie mutations without a CSRF token are rejected (403);
  fail-closed startup without the tokens env.
