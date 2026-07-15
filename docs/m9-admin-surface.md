# M9 — Opt-in admin web surface

M9 adds an opt-in, admin-authenticated web surface to shunt so an operator can
provision upstream Claude and Codex/ChatGPT accounts from a browser and observe
account-pool health without shell access. It builds directly on M8
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

After M8, adding an account to a deployed shunt required SSH (or
`docker exec`) plus an interactive `shunt login claude --name <n>` flow.
Depending on `--mode`, that flow creates a refreshable full-OAuth login,
imports an existing Claude Code login, or creates a one-year setup token. The
router exposed only `/`, `/health`, `/protocol`, `/v1/models`, `/routes`, and
the two `/v1/messages*` endpoints. Pool health (per-account quota utilization,
cooldowns) was observable only through the `x-shunt-account` response header
and logs. M9 relocates directly provisionable OAuth/setup-token flows from a
TTY to a browser form and surfaces the pool state that already lives in memory.

## Scope

- **Claude and Codex provisioning.** The Claude browser form offers a refreshable
  full-scope OAuth flow and the inference-only, one-year PKCE setup token. The
  Codex form always uses ChatGPT OAuth and stores a refreshable credential.
  Claude uses its fixed hosted manual redirect. ChatGPT redirects to
  `http://localhost:1455/auth/callback`; that localhost page is expected to fail
  in the operator's browser, which copies the full address-bar URL (or its
  `<code>#<state>` values) back to shunt. Importing an existing Claude Code or
  Codex credential file stays CLI-only because the source file lives on the host.
- **Refresh rotation ownership.** A full-OAuth account is refreshed and written
  back by `ClaudeAuthStore`. Because the provider can rotate the refresh token,
  its store file must have one active process owner; copying or sharing it
  across running hosts can invalidate another process. Setup-token accounts
  remain static and avoid this hazard.
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
- **Secrets never leak:** the verifier, authorization code, access token, and
  refresh token are never logged and never returned to the browser. The OAuth
  `state` is intentionally carried in the authorize URL and the opaque session
  id only in the `HttpOnly` session cookie — both are protocol values the
  browser must receive, not bearer secrets. Account add/remove is audit-logged
  by name and provisioning mode only.
- Docs recommend binding the admin surface behind HTTPS / a tunnel, same as the
  shared-gateway guide.
- **Emergency token rotation:** browser sessions are validated only against the
  in-memory session store, and the running process's environment is fixed — a
  config reload re-reads `SHUNT_ADMIN_TOKENS` from the *same* startup environment,
  so it neither rotates the token nor drops issued sessions (those persist until
  `session_ttl_secs`, default 1h). If an admin token is compromised, replace it in
  the environment source (systemd unit, `.env`, …) and **restart the process**: the
  restart both loads the new token set and drops every session the old token
  minted. To disable the last admin credential, remove the `[server.admin]` block
  before restarting (an empty `SHUNT_ADMIN_TOKENS` fails closed at startup).
  Rejecting stale sessions on reload is tracked in #100.

## Endpoints (registered only when `[server.admin]` is set)

| Method | Path | Purpose |
| :-- | :-- | :-- |
| `GET` | `/admin` | Dashboard (HTML); redirects to `/admin/login` when not signed in |
| `GET`,`POST` | `/admin/login` | Token login form → session cookie |
| `POST` | `/admin/logout` | Clear the session |
| `GET` | `/admin/accounts` | JSON: Claude store metadata (name, kind, expiry, UUID — never the token) |
| `GET` | `/admin/accounts/codex` | JSON: Codex store metadata (name, expiry, account ID — never the token) |
| `GET` | `/admin/pool` | JSON: per-`claude_oauth`/`chatgpt_oauth`-provider pool state |
| `POST` | `/admin/accounts/claude` | `{name, mode}` → start Claude provisioning (`oauth` or `setup_token`); omitted `mode` defaults to `setup_token`; returns `{authorize_url}` |
| `POST` | `/admin/accounts/claude/{name}/complete` | `{code}` → finish; stores the Claude account |
| `DELETE` | `/admin/accounts/claude/{name}` | Remove the Claude account's store file |
| `POST` | `/admin/accounts/codex` | `{name}` → start ChatGPT OAuth; returns `{authorize_url}` |
| `POST` | `/admin/accounts/codex/{name}/complete` | `{code}` with a full callback URL or `<code>#<state>` → finish and store the Codex account |
| `DELETE` | `/admin/accounts/codex/{name}` | Remove the Codex account's store file |

Gateway-owned errors keep the Anthropic error shape (`ShuntError`); page routes
render minimal server-side HTML with inline CSS/JS and no external requests.

## Phase 1 — provisioning flow

The browser flow reuses the CLI OAuth/setup-token internals in
`auth/claude/login.rs` (`generate_pkce`, `build_authorize_url`,
`exchange_code`) and stores through `claude_store`. The upstream redirect URI is
fixed to `platform.claude.com/oauth/code/callback` for this remote/manual flow —
the CLI's full-OAuth mode can use a localhost callback, but that loopback would
return to the operator's browser host rather than a remote shunt server. The
operator therefore pastes `<code>#<state>` into the form for both web modes.

1. `POST /admin/accounts/claude {name, mode}` validates the name and mode,
   generates a PKCE verifier/challenge + `state`, stores a single-use pending
   login with its authoritative flow kind (TTL `pending_ttl_secs`), and returns
   the authorize URL (`https://claude.com/cai/oauth/authorize`). `mode =
   "oauth"` requests the full refreshable Claude scope; `mode = "setup_token"`
   requests `user:inference`. Omitting `mode` defaults to `setup_token` for API
   backward compatibility, while the dashboard explicitly sends `oauth` by
   default.
2. The operator opens the URL, signs in to the target Claude account, approves,
   and pastes the resulting `<code>#<state>`.
3. `POST /admin/accounts/claude/{name}/complete {code}` verifies `state`
   (constant-time), exchanges the code at the token endpoint (honoring
   `SHUNT_CLAUDE_TOKEN_URL` for tests), then dispatches by the server-stored
   pending kind. Setup-token mode requests the one-year expiry, requires an
   account UUID, and calls `store_setup_token`. Full OAuth omits that expiry
   override, requires a non-empty refresh token, accepts an optional account
   UUID, computes `expiresAt`, and calls `store_oauth_tokens`. Both writes are
   atomic at `0600`; the pending entry is consumed. The completion request
   cannot switch modes.
4. The completion response reports whether the account is **live immediately** (a
   `claude_oauth` provider with an empty `accounts` list scans the store each
   request) or needs a name-only `[[providers.<name>.accounts]]` entry + reload.

Removal deletes the store file directly, path-guarded so a caller-supplied name
can never escape the accounts directory. This is new writeback behavior over an
operator-owned store file (issue-sanctioned) and touches no upstream state.

### Codex/ChatGPT OAuth

The Codex form reuses the shared PKCE generator but follows the Codex CLI OAuth
contract from [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md): authorize at
`https://auth.openai.com/oauth/authorize` with the fixed
`http://localhost:1455/auth/callback` redirect, then exchange the code using an
`application/x-www-form-urlencoded` POST. The operator may paste either the full
redirect URL from the browser address bar or `<code>#<state>`. Completion checks
the pending state in constant time, requires a refresh token, derives the account
ID from the access-token JWT, and writes the verbatim `auth.json` shape at `0600`.
`SHUNT_CODEX_TOKEN_URL` overrides the exchange endpoint for local tests; an
invalid or non-HTTPS/non-loopback override is ignored with a warning (mirroring the
Claude completion flow) instead of silently, and the exchange POST uses the
redirect-hardened client so a permitted endpoint cannot 3xx the single-use code to
an unsafe plaintext host. No access, refresh, or ID token is returned or logged.

Like Claude, an empty-account `chatgpt_oauth` provider scans the Codex store and
makes the new account live on its next request. Explicit-account providers need a
name-only account entry and reload.

## Phase 2 — pool dashboard

`AccountPool::snapshot(provider, &[AccountConfig], model)` returns a token-free,
serializable view per account: 5h/7d/7d_oi utilization + reset, unified status,
cooldown-seconds-remaining, `near_quota`, and a derived `available` flag. It reads
the same `entries` map `select_order` reads, clears only already-past quota
buckets (as the next selection would), never mutates the round-robin cursor, and
never inserts entries for accounts the pool has not yet seen (reported as
`has_state: false`). `AccountPool` tracks no sticky flag or last-selected
timestamp, so the dashboard reports what is actually stored rather than inventing
it. `GET /admin/pool` enumerates each `claude_oauth` and `chatgpt_oauth`
provider's accounts (its configured list, or the corresponding Claude/Codex store
scan for an empty list — the same resolution the adapters use). Codex publishes no
quota headers, so its utilization and reset fields remain `None` and render as
`—`; the dashboard adds no Codex usage or rate-limit parser.

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
  (404); API requires auth (401); setup-token mode keeps the legacy omitted-mode
  behavior and one-year exchange; full Claude OAuth requests the full scope,
  omits the expiry override, and persists a refreshable account; ChatGPT OAuth
  carries the Codex CLI authorize parameters, accepts both callback paste forms,
  uses a form-encoded exchange, persists verbatim auth.json, and appears in the
  pool; malformed or unknown modes and invalid account names fail without storing
  a file; missing refresh tokens fail closed; list/pool/response payloads never
  expose token material; cookie mutations without a CSRF token are rejected
  (403); fail-closed startup without the tokens env.
