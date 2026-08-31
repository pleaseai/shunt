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
- **Usage observation and credential ownership are separate.** The primary
  **Accounts and usage** view automatically discovers supported provider logins on
  the host: Claude Code, Codex CLI, Gemini CLI, Kimi Code, Grok CLI, and
  Cursor.app. It reads current access/session material only in memory and never
  refreshes, copies, or writes the source; Cursor.app's SQLite state is opened
  read-only. An expired source is reported as unavailable. Claude usage is
  cached for 60 seconds; Codex remains response-derived; the other integrations
  call their first-party read-only quota surfaces.
- **Refresh rotation ownership.** A managed full-OAuth account is refreshed and written
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
# header carrying the admin credential for API/curl calls; `x-api-key` is
# accepted alongside it on the admin and spend-limit routers only
header = "x-shunt-admin-token"
# env var holding admin credentials as name:token pairs (SEPARATE from
# [server.auth] client tokens). These are the WRITE tier
tokens_env = "SHUNT_ADMIN_TOKENS"
session_ttl_secs = 3600   # browser session lifetime after login
pending_ttl_secs = 600    # time to open the authorize URL and paste the code back

# Per-credential keys with an id the audit trail records. The key must come
# from ${VAR} / ${file:...} / a SHUNT_* override — a literal is rejected at load
[[server.admin.write_keys]]
id = "terraform"
key = "${SHUNT_ADMIN_KEY_TERRAFORM}"

[[server.admin.read_keys]]
id = "reporting"
key = "${file:/run/secrets/shunt-reporting-key}"
```

```bash
export SHUNT_ADMIN_TOKENS="ops:3f9c…"   # comma-separated name:token pairs
```

Admin credentials reuse the inbound-auth token format
([`m4-inbound-auth.md`](m4-inbound-auth.md)) and its constant-time compare, but
are a **separate credential** from `[server.auth]`: client tokens are handed to
devices; admin credentials add upstream accounts and administer spend limits.
Configuration validation is **fail-closed** — a present `[server.admin]` whose
three credential sources (`tokens_env`/`tokens_file`, `write_keys`, `read_keys`)
are *all* unset, empty, or malformed is a startup error, never a silently-open
admin surface (identical discipline to `[server.auth]`). An array-only
deployment, with `tokens_env` unset, boots.

The token source can be a **file** instead of the environment: `tokens_file` (a
path, `~` expanded) holds the same `name:token` pairs, one per line or
comma-separated. When `tokens_env` is non-empty it wins; otherwise the file is
read, and an unreadable configured file is a startup error. This exists so the
token does not have to live in the launch environment. `shunt dashboard setup`
automates the whole enablement: it writes a random `admin:<token>` to
`~/.shunt/admin-token` (owner-only, `0600`, via the same atomic-write path as
credential writeback), records it as `[server.admin].tokens_file`, adds
`[server.oauth_usage]`, and prints the dashboard URL. It is idempotent —
re-running reuses the token and appends no duplicate block — and leaves any
pre-existing `[server.admin]` untouched.

### Two access tiers

`read < write`, and `write` implies `read`:

- **`write`** — full access. The `tokens_env`/`tokens_file` `name:token` pairs
  are this tier (retained for compatibility; new deployments should prefer
  `[[server.admin.write_keys]]`, which carries an `id`), as is every
  `write_keys` entry.
- **`read`** — passes every `GET` on the admin surface and on the spend-limit
  API, and is refused with `403 permission_error` on every mutation. It also
  cannot sign in: `POST /admin/login` rejects it with `401` through the login
  form's own path, because a browser session carries full access and minting one
  from a read key would silently escalate it.

A credential's privilege is the **maximum** over every set it matches, not
whichever set happened to be scanned last. Array `id`s must be non-blank and
array keys at least 32 characters; ids *and* key values must each be unique
across all three sets, and a collision names the colliding ids without logging a
key value. A legacy `tokens_env` token shorter than 32 characters warns rather
than failing, because those tokens predate the rule. Array keys are
[`Secret`](config-secrets.md) values: a key written literally in the config file
is rejected at load and must come from `${VAR}`, `${file:/abs/path}`, or a
`SHUNT_*` override.

### Optional OIDC browser login

`[server.admin.oidc]` adds an allowlisted OIDC/SSO button to the token login page.
Admin tokens remain mandatory as the API/curl credential and fallback browser
login; OIDC is an additional browser path, not a replacement security boundary.

```toml
[server.admin.oidc]
public_url = "https://admin.example.com"
issuer = "https://accounts.example.com"
client_id = "shunt-admin"
client_secret_env = "SHUNT_ADMIN_OIDC_SECRET"
allowed_domains = ["example.com"]
# allowed_emails = ["operator@example.net"]
# scopes = ["openid", "email", "profile"]
```

The registered redirect URI is
`{public_url}/admin/oidc/callback`. `public_url` must be a bare HTTPS origin
(loopback HTTP is allowed for local development). The issuer and optional endpoint
overrides use HTTPS or loopback HTTP only. Startup also fails closed for an empty
issuer/client id, missing or empty client-secret environment variable, or an empty
email/domain allowlist.

`POST /admin/oidc/start` is same-origin guarded and rate-limited, discovers or uses
the configured authorization endpoint, creates PKCE plus a short-lived single-use
state, and redirects to the provider. The callback consumes that state before any
exchange, obtains a verified-email identity through the provider's token and
UserInfo endpoints, and re-checks the **current hot-reloaded allowlist** before
minting the ordinary admin session cookie. Provider input and exchange failures
produce generic browser pages; secrets, codes, and tokens are never logged or
echoed. Success always redirects to the fixed `/admin` target.

## Runtime wiring

The split mirrors how M4/M8 already separate hot-reloadable config from
process-lifetime state:

- `RuntimeState.admin_auth: Option<Arc<AdminAuth>>` — re-resolved on every reload,
  so admin credential/header edits (including the key arrays) hot-apply just like
  `[server.auth]`.
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
- **Browser:** sign in at `/admin/login` with a **write-tier** admin credential →
  an opaque session
  id in an in-memory `SessionStore`, set as cookie `shunt_admin_session`
  (`HttpOnly`, `SameSite=Strict`, `Path=/admin`). The cookie is marked `Secure`
  **unless the request host is loopback**, so local HTTP dev and tests work while
  any real deployment host gets a Secure cookie (reusing M8's `host_is_loopback`
  loopback carve-out). A session therefore always carries write access.
- **API/curl:** send the admin credential in the configured header
  (`x-shunt-admin-token`) or in `x-api-key`; both slots are accepted, and the
  resolved privilege is the maximum over whichever slots matched. When both
  slots carry a *different* valid credential of the same tier, the configured
  header supplies the audit actor, because it is checked first. This merged
  acceptance is scoped to the admin and spend-limit routers — on inference routes
  `x-api-key` is the caller's *own* Anthropic credential slot and an admin
  credential never authenticates there. Whatever these routers accept,
  `auth::inbound::consumed_by` strips from the same slot before any upstream
  request, alongside the gateway JWT and the `[server.auth]` static token, so an
  admin credential is never relayed to a provider. Header callers carry no
  ambient cookie and are therefore **CSRF-exempt**.
- **Tier check on mutations:** every account-provisioning mutation calls
  `require_write` before the CSRF check; a read credential gets
  `403 permission_error` ("read-only admin credential cannot perform this
  action"). `POST /admin/logout` needs no credential at all — it only clears a
  session — and is guarded by the same-origin check instead.
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
  before restarting (with no key arrays configured, an empty `SHUNT_ADMIN_TOKENS`
  fails closed at startup).
  Rejecting stale sessions on reload is tracked in #100. A `${file:...}`-backed
  `[[server.admin.write_keys]]`/`read_keys` entry is the exception to the first
  half of this: its value is re-read on every config load, so overwriting the
  referenced file and triggering a reload does rotate that key without a
  restart. Sessions already minted still survive until `session_ttl_secs`.

## Endpoints (registered only when `[server.admin]` is set)

| Method | Path | Purpose |
| :-- | :-- | :-- |
| `GET` | `/admin` | Dashboard (HTML); redirects to `/admin/login` when not signed in |
| `GET`,`POST` | `/admin/login` | Token or OIDC login form → session cookie |
| `POST` | `/admin/oidc/start` | Start the optional same-origin OIDC/PKCE browser login |
| `GET` | `/admin/oidc/callback` | Complete OIDC login, enforce the current allowlist, and mint a session |
| `POST` | `/admin/logout` | Clear the session |
| `GET` | `/admin/accounts` | JSON: Claude store metadata (name, kind, expiry, UUID — never the token) |
| `GET` | `/admin/accounts/codex` | JSON: Codex store metadata (name, expiry, account ID — never the token) |
| `GET` | `/admin/observed` | JSON: read-only observed Claude, Codex, Gemini, Kimi, Grok, and Cursor identity, state, and provider-native usage — never token material |
| `GET` | `/admin/pool` | JSON: per-`claude_oauth`/`chatgpt_oauth` managed-pool state; account objects may include an optional `plan` string |
| `POST` | `/admin/accounts/claude` | `{name, mode}` → start Claude provisioning (`oauth` or `setup_token`); omitted `mode` defaults to `setup_token`; returns `{authorize_url}` |
| `POST` | `/admin/accounts/claude/{name}/complete` | `{code}` → finish; stores the Claude account |
| `POST` | `/admin/accounts/claude/{name}/refresh` | Exercise an **imported** account's refresh grant now and report whether the login is still alive; returns the new `expires_at` and never token material. `400` for a `setup_token` account (no refresh grant exists) or a terminal verdict (`invalid_grant`, no stored refresh token, or a rotated pair that could not be persisted); `502` for a non-terminal failure |
| `DELETE` | `/admin/accounts/claude/{name}` | Remove the Claude account's store file |
| `POST` | `/admin/accounts/codex` | `{name}` → start ChatGPT OAuth; returns `{authorize_url}` |
| `POST` | `/admin/accounts/codex/{name}/complete` | `{code}` with a full callback URL or `<code>#<state>` → finish and store the Codex account |
| `DELETE` | `/admin/accounts/codex/{name}` | Remove the Codex account's store file |

Gateway-owned errors keep the Anthropic error shape (`ShuntError`); page routes
render minimal server-side HTML with inline CSS/JS and no external requests.

Every `GET` above is reachable with a **read** credential. `POST /admin/login`
and the seven account-provisioning routes (`POST`/`DELETE` under
`/admin/accounts/...`) require **write**. `POST /admin/logout` and the two OIDC
routes are login-flow plumbing and are guarded by the same-origin/state checks
rather than by tier.

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

## Phase 2 — observed usage and managed-pool health

The dashboard is usage-first. `GET /admin/observed` discovers supported local
credentials on each request but keeps every token/session in a private,
non-serializable model. Claude Code checks its configured/default credential file,
then macOS Keychain service `Claude Code-credentials`; Codex, Gemini, Kimi, and
Grok read their CLI credential stores; Cursor opens Cursor.app's `state.vscdb`
with `SQLITE_OPEN_READ_ONLY`. The endpoint masks account identity, labels
ownership as `observed`, and never invokes a refresh/writeback store. Provider
requests have a 15-second timeout. Claude reads `/api/oauth/usage`, with the
token-free snapshot cached process-wide for 60 seconds. The Claude row is the
one exception to the identity masking above: it carries the account `uuid` so
the table can tell an observation and a managed pool account holding the same
subscription apart from two genuinely different accounts. The value is already
returned unmasked by `GET /admin/accounts` to the same authenticated caller, so
this adds no disclosure the admin surface did not already make. Gemini returns every
Code Assist model bucket, Kimi returns weekly and 5-hour windows, Grok returns
credit/product usage, and Cursor returns billing-cycle, Auto + Composer, and
named-model usage. Codex remains `response-derived`: both translated Messages
traffic and raw inbound Responses attach the default CLI account id to
`x-codex-*` quota capture without importing the credential into the managed
store. Before such traffic its row explicitly says it is waiting for traffic.

The usage table groups by provider: the provider is named once and its accounts
nest beneath it, folding managed pool accounts in alongside the read-only
observations rather than stranding them in the advanced section. An observation
and a managed account are coalesced into one row when their account `uuid`
matches — one subscription is one row, labelled with the managed account name,
with the observation's windows preferred because the pool only learns a window
from a response header it has actually received.

Coalescing is deliberately conservative: identity resolves to `None` whenever
`CLAUDE_CONFIG_DIR` or `CLAUDE_CREDENTIALS` is set, and an unidentified
observation never matches. Both Claude credential sources are profile-agnostic
(the fixed Keychain service name above; a hardcoded
`$HOME/.claude/.credentials.json`, unless `CLAUDE_CREDENTIALS` relocates it)
while `oauthAccount.accountUuid` is read from a config directory that
`CLAUDE_CONFIG_DIR` relocates, so with either variable set the recorded
identity can name a different account than the credential that was actually
read — observed against a live two-account pool, where it labelled an
exhausted account's usage with the other account's uuid. Declining to merge
costs a combined row; guessing costs a silently mis-attributed quota bar.
Closing that gap needs a token-scoped identity source, which does not exist
today: `/api/oauth/usage` is the only Claude endpoint read here, and
`shuntAccountUuid` is captured only at login/import time.

The known `uuid` is still attached even when the local token has expired: the
account id is read independently of token validity, so an expired observation
keeps matching its managed row instead of rendering as an unrelated duplicate.

Which state the coalesced row then displays follows a precedence, not a
blanket override: a managed operational state (`disabled`/`cooling`/
`near-quota`) is an actionable gateway-side fact and wins over a stale local
observation error, so a cooling account still shows its cooldown remediation
even though the client's last local check happened to see an expired token.
An observed error (`expired`/`unavailable`) still surfaces over a merely idle
managed state (`available`/`unseen`), which is the "Needs login" case this
matching was built for.

Managed provisioning and store metadata remain available under a collapsed
**Manage pool accounts (advanced)** section. `AccountPool::snapshot(provider, &[AccountConfig], model)` returns a token-free,
serializable view per account: 5h/7d/7d_oi utilization + reset, unified status,
account-wide cooldown-seconds-remaining, Fable-only cooldown-seconds-remaining,
`near_quota`, a `needs_relogin` flag, and a derived `available` flag. The Fable-only cooldown counts
toward `available` only when `model` is a Fable model, so an account cooling on
its `7d_oi` bucket still reports available to every other family. Because the
admin snapshot is taken with `model = None`, the dashboard carries the
Fable-only cooldown as its own `cooling-fable` row state ("Cooling (Fable)",
with a `Fable retries in …` note) rather than folding it into `available` — the
account is genuinely live for every non-Fable family, so neither "Live" nor a
plain "Cooling" describes it. It reads
the same `entries` map `select_order` reads, clears only already-past quota
buckets (as the next selection would), never mutates the round-robin cursor, and
never inserts entries for accounts the pool has not yet seen (reported as
`has_state: false`).

### `needs_relogin` — a dead credential, not a pause

`needs_relogin` marks an account whose credential no operator-free retry can
revive, and **both** dashboard tables report it as **needs re-login** — the
primary Accounts table ("Needs re-login", with a "Re-add this account to sign in
again" note) and the collapsed managed-pool table's State column — ahead
of every cooldown state. It exists because the two are otherwise
indistinguishable on the dashboard: a cooldown expires after five minutes and
the account is selected again, so a permanently dead account shows the same
`cooling` a quota pause does, forever — one 401 (and, for an imported account,
one already-rejected refresh POST) every five minutes with nothing durable to
see.

The mark also records *why* it was set (`ReloginCause`, internal — the JSON
field stays a boolean). Only one decision reads it: whether a successful refresh
grant is enough to clear the mark. `RefreshGrant` means the grant itself failed,
so a later grant that succeeds disproves it and the admin probe may clear it.
`ServedRequest` means the provider rejected a bearer the account actually
presented — the grant was never the broken part, so a working grant proves
nothing and only a served response clears it.

It is set from exactly three places, all terminal by construction:

- a 401 on a credential that carries no refresh grant at all — a `token_env`
  token, or a setup token **still inside its local `expiresAt`** that the
  provider has revoked early (`adapters/anthropic/mod.rs`, the `RefreshRetry`
  branch). Resolution succeeds because the stored token still looks valid, so
  the verdict can only come from the upstream 401,
- a 401 on the *retry* after a refresh that itself succeeded. A live grant
  yielding a bearer the API still rejects means the account is de-authorized
  upstream, not momentarily unlucky; the adapter already cools it for five
  minutes and rotates, so without the mark it cycles there forever reported as
  plain `cooling`, and
- a refresh that cannot be retried into success, classified by
  `auth::claude::auth::is_terminal_refresh_failure`. Three reasons carry the
  typed `TerminalRefresh` marker, mirroring the one the gateway store already
  uses (`auth/gateway/auth.rs`) and the code-based classification in the Kimi
  store (`auth/kimi/auth.rs`):
  - `InvalidGrant` — the provider returned the OAuth `invalid_grant` code.
  - `NoRefreshToken` — the credential file carries no refresh token, so there
    is no grant left to send. A setup token **past its local `expiresAt`**
    reaches its dead state through *this* path rather than the 401 above:
    resolution fails before any upstream request is sent, so the `RefreshRetry`
    branch is never entered. The two setup-token bullets are the same
    credential at different times — revoked-before-expiry surfaces via the 401,
    expired surfaces at resolution — and an expired token takes this path
    whether or not it was also revoked, because nothing reaches the provider.
  - `WritebackFailed` — the provider rotated the token but the new pair could
    not be persisted. The grant already consumed the refresh token on disk, so
    every later attempt replays a spent one. Attached only when the response
    actually carried a *different* refresh token: a provider that omits the
    field leaves the stored one live, and that writeback failure stays
    non-terminal.

  A transient failure (5xx, network, timeout, an unparseable body) is
  deliberately **not** terminal: marking one would report a healthy account as
  dead after a momentary provider blip. Both the 401 → force-refresh path and
  credential *resolution* classify this way: once a dead account's access token
  expires — its steady state within hours — the refresh is rejected on read,
  before any upstream POST, and that path marks the account too
  (`auth::resolve_claude_account_classified`).

It is cleared by proof the credential works again — but the proof has to match
the cause. A served response (`mark_healthy_scoped`) clears any mark, and so
does a re-login through `POST /admin/accounts/claude/{name}/complete`, which
replaces the credential outright. A successful
`POST /admin/accounts/claude/{name}/refresh` probe clears only a
`RefreshGrant`-caused mark.

A successful refresh on the *proxy* path clears nothing on its own. The adapter
deliberately waits for the retried request: a live grant proves the refresh
token works, not that the account can serve inference, and the retry may still
come back 401 — in which case the mark is set rather than cleared.

The mark follows the *credential*, not the provider row that tripped over it.
One store account activated by name in two `[[providers.*]]` tables gets two
health entries — `resolve_pool_accounts` leaves a name-only entry UUID-less, so
`account_key` keys it as `UpstreamInline`, which carries the upstream name — yet
both are backed by one credential file. Marking (and clearing) therefore fans
out across every entry for the same store account, exactly as the admin
re-login and probe paths already do. The fan-out is narrow on purpose: an
account carrying its own `credentials` path or `token_env` is a different
credential that merely shares a name, so a failure on it condemns nothing else.
An entry keyed by uuid (`Verified`) is reached only from an account that carries
that same uuid, because `AccountKey` keeps no name to match on there.

The fan-out reaches three things it would otherwise miss. It stamps `observed`
on the sibling, because `snapshot` reports an unobserved entry as `unseen` and
drops every field on it — a row `select_order` has picked but no response has
answered yet would carry the mark and still render clean. It runs on
`mark_healthy_scoped` as well, so a response served through *any* row clears the
mark on all of them rather than leaving a live credential condemned until each
row happens to serve traffic of its own. And on the success path it clears the
mark alone: cooldowns and storm-control ramps stay per-row, because this is a
signal and not a routing policy.

That success-path clear is gated on an `any_needs_relogin` flag, because it sits
on the path every served response takes and scanning the map there cost ~2x on
the `account_pool_healthy_updates` benchmark for a signal that is absent in the
steady state. The flag is conservative in one direction only — `false` means no
entry carries a mark, `true` may outlive the last one and cost a single extra
scan — and every mutation of it happens under the entries lock, so the recompute
can never store `false` over a mark that landed beside it.

The flag is deliberately **independent of the cooldown** and changes nothing
about routing, selection, or the cooldown clock — it adds a signal, it does not
add a policy. It is memory-only: the opt-in `[server.pool] state_path`
persister carries quota alone, so a restart clears the mark and the account's
next terminal failure re-establishes it.

`POST /admin/accounts/claude/{name}/refresh` is the operator's on-demand probe
for the same question. It is write-tier, CSRF-checked, and rate-limited through
the same `complete_rate` limiter as the completion route (it POSTs to the
provider's token endpoint). It refuses a `setup_token` account up front rather
than attempting a grant that cannot exist, and it always goes through
`ClaudeAuthStore::force_refresh`, never a hand-rolled grant: that store holds
the process-global `REFRESH_LOCK` across read → POST → atomic writeback, which
is what keeps the probe from racing the proxy's own refresh and stranding a
rotated refresh token. It does not additionally take the per-account
`AccountPool::refresh_lock` the proxy takes — that lock is keyed by an
`AccountConfig` from a provider table, and a store account may be reachable
through zero, one, or several provider entries, so there is no single right one
to pick; it is also unnecessary, because both of the proxy's critical sections
sit inside the same global lock. Success returns the new `expires_at` and
nothing else — no access token, no refresh token, and no provider body that
carried them. It clears a `RefreshGrant`-caused mark, but deliberately not a
`ServedRequest`-caused one: the probe never sends a request the account has to
serve, so its success is no evidence against a bearer the provider already
rejected. The response's `needs_relogin` is read back from the pool *after* that
clear, so a probe that succeeded against a still-dead account says so rather
than reporting a recovery `/admin/pool` would contradict. `AccountPool` tracks no sticky flag or last-selected
timestamp, so the dashboard reports what is actually stored rather than inventing
it. `GET /admin/pool` enumerates each `claude_oauth` and `chatgpt_oauth`
provider's accounts (its configured list, or the corresponding Claude/Codex store
scan for an empty list — the same resolution the adapters use). Codex successful
responses now populate the 5h/7d fields from `x-codex-*` rate-limit headers;
unsupported windows are ignored and `7d_oi` remains `None` because Codex has no
analog. Since issue #195 this recorded state also feeds Codex account selection (see `m10-codex-multi-account.md`), in addition to the dashboard display.

The optional `plan` field is derived from credential data already held by
shunt: Claude reads `claudeAiOauth.subscriptionType`, and Codex reads the
`chatgpt_plan_type` claim from its stored JWT. Whenever an imported Claude
credential's on-disk access token is still valid, the request also makes a
bounded `GET /api/oauth/profile` backfill and caches the result — this runs
even for an account whose file already carries a subscription type, since
that value alone carries no multiplier detail and the profile lookup can
refine it toward a more precise one, while never discarding it if the lookup
is coarser or fails; this backfill only ever reads a token already on disk,
it never refreshes and never writes back. An account whose on-disk token has
already expired keeps whatever plan (or lack of one) it already had until a
later view, once normal traffic elsewhere refreshes it; an idle account with
no traffic and no usage polling can stay at that same value indefinitely
while its on-disk token stays expired. Setup-token and `token_env` accounts
are not backfilled. A missing, failed, or unrecognized lookup leaves the
existing plan (or its absence) unaffected.

Budget exhaustion never erases a file-derived plan: the file-read phase is
guaranteed its own `min_slice` floor above the shared deadline, so an earlier
provider's stalled backfill can never starve this cheap local read. Even in
the pathological case where the file phase does not finish within that
floor, any plan already cached from an earlier resolution still appears in
the response.

Plans, tokens, and credential identities are tracked **positionally** against
the resolved account list, never keyed by display name.
`resolve_pool_accounts` appends the configured accounts after any scoped store
entries without deduplicating names, so one provider can legitimately resolve
two distinct accounts that share a label; a name-keyed map collapses them,
which would both show one account's subscription on the other and — because
the same map supplies the bearer token — probe one account with the other's
credential.

The profile cache is keyed by the credential's own identity where one exists.
An account whose credential file carries the `shuntAccountUuid` shunt's import
stamps in is keyed by that value: it survives a token refresh and changes when
the account is re-provisioned, so a resolved plan is held for 24 hours. The
file's uuid outranks a `uuid` carried in config, because the cached value is
the plan of whoever *that file's token* authenticates as, and the config field
never selects the file — the credential path is derived from `credentials` or
the account name. A config `uuid`, or the process-lifetime inline-identity memo
that fills it in when config omits it, can therefore still name the previous
occupant of a re-provisioned path; keying off it would file the new account's
plan under the old account's identity and serve the old plan back for the full
day. A config `uuid` is used only when the file yielded none. Only when no uuid
exists anywhere does the key fall back to the account's name, which is stable
but not unique over time — the same name may later belong to a different
account. Nothing in production
clears this cache, so that fallback caps its entries at 10 minutes instead,
which is also the retry interval for a failed lookup.

Two further rules follow from that fallback being a name rather than an
identity. A name key shared by two accounts *within one resolved list* is
ambiguous, not merely imprecise: the shared cache cannot separate them, so
neither account reads or writes it and both are resolved fresh. And because a
uuid-less account's identity lives only in its credential file, the uuid last
seen at each credential **path** is memoized, so a resolution whose file phase
timed out can still reconstruct the key and serve an already-cached plan. That
memo is keyed by path rather than name for the same reason as everything else
on this path: two accounts a name cannot separate still have distinct paths,
and when they share a path they are reading one file and so share one identity.

That memo is consulted **only** when the file phase produced no result at all.
A completed read is authoritative even when it finds no uuid — the account
holding that path today has no identity, whatever an earlier one had — and such
a read also clears the remembered value, so a later timeout cannot resurrect
it. Every way a read can fail to produce a uuid clears it, not just the
parses-but-carries-none case: a missing, unreadable, or unparsable file drops
the entry too. The bias is deliberate. Clearing costs at most a fallback to
the name key and its 10-minute ceiling, while keeping a no-longer-evidenced
uuid risks serving the path's previous occupant's plan for the full
exact-identity day.

More than the path memo goes stale on a failed read, so such an account does
not use the shared cache at all for that pass. Every identity still available
to it names a *previous* holder: `account.uuid` is filled by
`resolve_pool_accounts` from the very file that just failed to read and is
memoized for the process lifetime, so it outlives that credential; and the
name fallback is stable but not unique over time. There is nothing to write
either, since a failed read yields no token. So the plan is omitted for that
pass and reappears once the credential reads again — an honest gap rather
than another account's subscription.

A file that parses and merely carries no `shuntAccountUuid` is not a failed
read and none of this applies to it: that is an ordinary hand-placed
credential, keyed as it always was.

The batched read walks its candidates in order, so it can record one
account's failure and then be abandoned mid-list when a later credential
stalls past the deadline. The per-account failures are therefore tracked
outside the result the timeout discards — a failure the read already observed
is knowledge the request keeps, and dropping it would hand that account back
the cache access this rule exists to withhold.

A failure also has to outlive the request that saw it, for the same reason
one step further out. Per-request failure state says nothing about the *next*
request, so a later one that times out would carry none and fall back to the
still-memoized `uuid` — restoring precisely the identity the failed read
refuted. The path memo therefore records a read failure rather than merely
dropping what it knew, and an account whose path is remembered that way stays
out of the cache on a timed-out pass too. Only a pass that reads the file
again can clear it: a timeout is not evidence that the path became readable.

The batched credential read is single-flight, process-wide. A read holds its
permit until it genuinely finishes, not until the request waiting on it gives
up, because a `spawn_blocking` task cannot be cancelled once started. A
credential file on a hung network or FUSE mount therefore leaks one blocking
worker rather than one per `/admin/pool` request: later requests fail to
acquire the permit, skip the file phase, and fall back to their cached plans.

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
