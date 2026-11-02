# M18 — xAI (Grok) multi-account pool (spec)

> Companion to [`m6-xai-provider.md`](m6-xai-provider.md) (the single-account `xai_oauth` provider
> this extends), [`m10-codex-multi-account.md`](m10-codex-multi-account.md) (the Responses pool
> this rides), and [`m15-kimi-oauth.md`](m15-kimi-oauth.md) (the named-account store and
> read-only admin surface this follows).

## 1. Scope

- Pool several SuperGrok OAuth logins behind `auth = "xai_oauth"` (the built-in `grok` provider,
  `kind = "responses"`, `https://cli-chat-proxy.grok.com/v1`). The pool rotates requests across
  accounts and fails over when one account is unavailable.
- No new config keys. One new environment variable: `SHUNT_XAI_ACCOUNTS_DIR`, which overrides the
  named-account directory.
- A user with only the legacy `~/.shunt/xai-auth.json` sees no change. The legacy file is read
  only while the pool is empty (§3).
- Out of scope for this spec: quota-aware selection, WebSocket transport, a same-account retry
  after a `401`, and admin provisioning routes. §12 lists them.

## 2. Account store

Each named account is one file:

```text
~/.shunt/accounts/xai/<name>.json
```

Override the directory with `SHUNT_XAI_ACCOUNTS_DIR` (`src/auth/xai/store.rs`). A directory shunt
creates is made `0700`; an existing directory keeps its own permissions. Each file is written
`0600`, atomically (temp file and rename), through the shared `write_account_file` helper.

The file has the same shape as the legacy single-account file (§3 of m6). `shunt login xai --name`
writes it, and `XaiAuthStore` reads and refreshes it unchanged:

```jsonc
{
  "tokens": {
    "access_token":  "<JWT>",
    "refresh_token": "<opaque>",
    "id_token":      "<JWT>"      // optional
  },
  "last_refresh": "2026-..."       // ISO-8601
}
```

Both files are built by one shared function, `tokens_value()` (`src/auth/xai/auth.rs`), so the two
shapes cannot drift.

- **Identity.** A pool member's identity is its file name (`<name>`). shunt reads no verified
  upstream account identifier from an xAI login, so a scanned entry carries no `uuid`. Two logins
  under two names are always two pool members. The JWT `sub` is not used to merge accounts, because
  an unverified claim could fold two paid logins into one member.
- **Login.** `shunt login xai --name <name>` runs the same device-code flow as `shunt login xai`
  and writes `~/.shunt/accounts/xai/<name>.json`. The name may contain only lowercase ASCII letters, digits, and `.`, `_`, `@`, `+` or `-`, and must not start or end with a dot.
  `--mode` and `--long-lived` are rejected with xAI-specific
  messages, because xAI tokens are always refreshable.
- **Reuse.** Running `--name` with a name that already exists replaces that file. Use a new name
  to add an account.
- **Legacy login.** Plain `shunt login xai` never writes a named file, and `--name` never writes
  `~/.shunt/xai-auth.json`.
- **Re-login hints.** A store account's credential errors (missing file, refused refresh) name
  `shunt login xai --name <name>`, since the plain login cannot fix it. An account with an explicit
  `credentials` path keeps the plain hint. An upstream `401` cannot tell which login answered, so
  its message names both.

## 3. Legacy file rule

The legacy credential (`$SHUNT_XAI_AUTH_FILE`, default `~/.shunt/xai-auth.json`) is used **only
when the provider's resolved xAI pool is empty**. The pool is empty when all of these hold:

- the provider has no inline `[[providers.grok.accounts]]` entries,
- there is no `account` or `accounts` scope on the route, and
- `$SHUNT_XAI_ACCOUNTS_DIR` is missing or holds no valid `<name>.json`.

The legacy file is never a pool member. Once the pool has at least one account, shunt does not
read the legacy file at all. Each turn stats the store directory and reuses the last scan while
the directory's mtime is unchanged; adding or removing an account changes the mtime, so it becomes
visible on the next turn without a restart. A store directory that cannot be read when a stat or a
scan is needed (an error other than a missing directory) fails the request rather than falling
back to the legacy file, the same rule the Codex pool follows.

This is the same rule Codex (M10) and Antigravity already use. Adding a first named account
therefore moves the provider off the legacy file. The migration is a move, not a copy:

1. Stop shunt.
2. `install -d -m 700 ~/.shunt/accounts/xai`. The directory usually does not exist yet when migrating
   from the single-account setup, and `mv` fails without it.
3. `mv ~/.shunt/xai-auth.json ~/.shunt/accounts/xai/<account-name>.json`
4. Start shunt.

Do not copy the file. xAI rotates the refresh token on every refresh (m6 §3), so two copies
would hold one consumed refresh token and one live one, and the first refresh of either copy
would break the other.

`shunt login xai --name` prints a note when the legacy file exists. The note names the legacy path,
the accounts directory, and the `mv` command above, so the first named login does not silently drop
the legacy account from service.

The routed inbound Codex endpoint applies the same rule (§9), so the `mv` does not break it.

## 4. Configuration

**Store scan.** With no config change, an `xai_oauth` provider scans every account in
`~/.shunt/accounts/xai/`, ordered by file name.

**Per-account settings.** Inline `[[providers.grok.accounts]]` entries take these fields:

| Field | Meaning |
| :-- | :-- |
| `name` | Pool identity. Also the default file name when `credentials` is absent. |
| `credentials` | Path to a credential file, overriding `<name>.json` in the store. |
| `token_env` | Name of an environment variable holding a bearer. Used verbatim and never refreshed. |
| `priority` | Selection order. Lower values are preferred. |
| `disabled` | Excludes the account from selection. |

Setting both `credentials` and `token_env` on one account is an error.

**Scope on `[[upstreams]]`.** Use the `[[upstreams]]` form to scope a route:

```toml
auth = { mode = "xai_oauth", account = "primary" }
# or
auth = { mode = "xai_oauth", accounts = ["primary", "backup"] }
```

- `account` and `accounts` together is an error.
- `accounts = []` is an error (`EmptyUpstreamAccountList`). Omit the key to scan the whole store.
- An entry in `accounts` may also be a full inline account table.
- A scope that names a store file that does not exist fails the request with a gateway-owned `502`:
  `xAI account scope references missing store account "<name>"`.

## 5. Validation

The `xai_oauth` guard is unchanged (`src/config.rs`): the provider must have `kind = "responses"`,
an `https` URL, and an `x.ai` or `grok.com` host. There is no loopback exception, unlike the
Claude, ChatGPT, and Kimi providers.

The guard runs for every provider after the accounts gate, so every pooled account is held to it.
Adding accounts does not relax it.

## 6. Selection

Selection uses the shared `AccountPool`, the same machinery as the Claude, Codex, and Kimi pools:

- **Sticky.** A request with an `x-claude-code-session-id` is pinned to one account for the session.
- **Round-robin.** Other requests rotate across the eligible accounts.
- **`priority`** and **`disabled`** apply as in §4.
- **Admin pause** removes an account from selection until it is unpaused (§11). The pause is
  memory-only and resets on restart.
- **`ramp_initial_concurrency`** applies to xAI accounts as it does to the other pools.

xAI accounts have no quota windows. Selection therefore has no near-quota yield, and the
`max_utilization*` caps stay inert, as they do for Kimi. The pool does not re-probe cooled-down
accounts, and xAI never uses the WebSocket transport.

## 7. Failover

Each upstream response is classified by `accounts::classify_xai`, which is `classify_codex` with
`401` and `402` changed to rotate. Nothing shared changes: the rotation labels still come from the
shared `rotation_reason`.

| Outcome | Pool action | Cooldown | Rotation reason |
| :-- | :-- | :-- | :-- |
| `2xx` | Mark healthy and relay. | — | — |
| `401` | Cool down and rotate, and mark the account's file so its next use refreshes the token even before its `exp`. No same-account retry. | 30 s | `auth` |
| `402` | Cool down and rotate. | 30 s | `membership` |
| `403` | Relay the mapped xAI error and `mark_healthy(false)`. No rotation. | — | — |
| `429` | Cool down and rotate. | `retry-after`, clamped to 1–3600 s; default 60 s | `rate_limit` |
| `5xx` | Cool down and rotate. | 30 s | `server_error` |
| Other `4xx` (400, 404, 413, …) | Relay the mapped error and `mark_healthy(false)`. Classification does not read the body; the mapped response reads it lazily, only when the client consumes it. | — | — |
| Credential resolution fails (missing or corrupt file, refresh `400`/`401` `invalid_grant`, refresh `403` tier gate, refresh `5xx`, unset `token_env`) | Cool down and rotate. | 5 min | `auth` |
| Transport error | Cool down and rotate. | 30 s | `transport` |
| TTFB timeout | Terminal `504`, unchanged. | — | — |
| Exhausted after at least one response | Relay the last response, mapped with the `xai_oauth` `401`/`403` texts. | — | `exhausted` |
| Exhausted with no response | `all xAI OAuth accounts failed before receiving an upstream response`. On `/v1/messages` the outer chain answers `all upstreams failed (N attempted)`. | — | `exhausted` |

**Why `401` rotates.** Expiry is the JWT `exp` claim, read with a 5-minute buffer, so a token xAI
rejects with a `401` was revoked early. The current request rotates to another account instead of
refreshing and retrying the same one, as the Kimi pool does. The pool also calls
`auth::force_xai_refresh` for the account, which marks its credential file so the account's next
use refreshes the token even before its `exp` (a `token_env` bearer is static and is not marked).

**Why `402` rotates.** A `402` is an account-specific, persistent refusal (the subscription does not
cover the request), not a client-fixable one. Rotating lets the healthy accounts serve the request.
When every account returns `402`, the pool is exhausted and the client sees `502`, not `402`: the
upstream status passes through `client_facing_status`, which maps it to `502` (pinned by
`xai_pool_exhausted_by_402s_relays_the_last_one`, which asserts the status only).

**Why `403` relays.** Three reasons, as recorded in the design review:

1. The documented xAI tier gate surfaces at token refresh (m6 §5). The pool already cools a
   resolution failure down and rotates, so a gated account leaves the rotation through that path.
2. An inference `403` can be a content-policy or model-gating refusal. Rotating would re-send a
   flagged request through every account the user owns.
3. Relaying changes nothing shared: no new label and no change to `rotation_reason`.

**Follow-up.** Capture a live `403` body from the CLI proxy (redact tokens) under `docs/notes/`. If it
carries a distinct tier-gate code, rotate on that code only. Every other `403` keeps relaying. This
has not been done yet.

## 8. Refresh and isolation

- **Per-file store.** Each account gets its own `XaiAuthStore` over its own file. A write only
  reaches the path its store was built with, so two accounts cannot overwrite each other's tokens.
- **Single-flight lock, one per file.** Refreshes of the same credential file are serialized; a
  waiter re-reads the file after the lock is released and finds the rotated pair. The lock is keyed
  by the canonicalized path (`auth::shared::refresh_lock`, shared with the Codex store), so
  refreshes on different accounts run in parallel and a stalled refresh on one account never holds
  up the next. The reads, the rotated-pair writeback, and the forced-refresh mark after a `401` use
  that same resolved path, so a credential reached through a symlink is written in place and the
  link survives.
- **Bounded refresh.** The refresh client sets no timeout, so the store bounds two things with
  `REFRESH_TIMEOUT` (30 s). Each caller's wait — for the file's lock, then for the refresh task — has
  one deadline, so a caller queued behind a stalled refresh fails at its own deadline instead of
  waiting out another full attempt, and the pool cools the account down and fails over. The token
  POST inside the refresh task has its own full 30 s from when the task starts, not the caller's
  remaining budget: cutting off a POST xAI may already have honored would consume the refresh
  token. A caller that gives up first leaves the task running; it still writes the rotated pair,
  and the next caller reads it.
- **Detached task.** The refresh and the rotated-pair writeback run in a detached task that owns the
  lock, so the write completes even if the caller is cancelled.
- **Pool resolution does not take `AccountPool::refresh_lock`**, matching the Codex pool.
- **Redirect-hardened client.** A pooled account refreshes through `token_refresh_client()`, which
  refuses any redirect to a non-loopback `http://` URL. The refresh POST carries the refresh token,
  and a `307`/`308` would otherwise re-send that body to the new host. The single-account file
  (`resolve_credential`) keeps the gateway's client. Hardening that path is deferred (§12).

## 9. Routed inbound Codex endpoint

A route from `[[server.codex_endpoint.routes]]` to an `xai_oauth` provider resolves its credential
as follows:

- **Non-empty pool.** The route uses the first account in pool selection order whose credential
  loads. That order honors the provider's scope, `priority`, `disabled`, and the admin pause. An
  account whose credential fails to load (a missing or corrupt file, a refused refresh) is cooled
  down for 5 minutes and skipped, as on the outbound pool, so one dead login cannot block the route.
  The request is then sent once: an upstream failure is relayed, not retried on another account.
  When `max_utilization` caps exclude every account, the route returns the pool's cap-exhaustion
  `429`, as the ChatGPT inbound path does. It is a guard: §12 keeps xAI caps inert, so it applies
  only once utilization is recorded for xAI accounts.
- **Empty pool.** The route uses `~/.shunt/xai-auth.json`, as before (§3).

Errors on this endpoint keep the OpenAI Responses error shape, because `src/codex_endpoint.rs`
re-shapes every adapter error there.

## 10. Response shaping

- Requests to the `grok` provider carry the Grok-CLI identity headers (m6 §6). The headers come
  from the selected account's request, so each account is sent the same way.
- A pooled response names the account that produced it:
  - Non-streaming responses carry an `x-shunt-account` header.
  - Streaming responses carry an `event: account` frame before the first relayed frame.
- The `x-shunt-account` value is the account's pool name. On a shared gateway, use neutral names
  (for example `a`, `b`, or `work`), because the name is visible to every client that can see the
  response headers.

Streaming semantics are those of the pooled Codex path (m10). The response commits before the scan,
and an empty pool falls back to the single-account path inside the committed stream.

## 11. Admin and usage

All read-only:

- `GET /admin/api/pool` lists `grok` with its accounts.
- `PATCH /admin/api/pool/{provider}/accounts/{account_ref}` sets the account's `paused` flag.
- `GET /usage` includes xAI accounts in its report.
- With `[server.pool] usage_refresh_seconds` set, the usage poller reads each pooled account's
  billing from the Grok CLI proxy (`GET /billing?format=credits`, the call the observed `grok` row
  already makes) with that account's own bearer, refreshing it first if stale. The credit total and
  each product's usage (for example "Grok Build") become the account's dashboard usage bars, reset
  at the billing period end. xAI omits a zero-valued field, so a reported period with no credit
  total shows as 0% used; a product with no figure gets no bar. They are display-only, like
  Antigravity's (m17): selection, cooldown,
  and caps ignore them. Only the buckets are stored, never the name or email `/user` returns. A
  `token_env` account is skipped as static, and the legacy single-account file has no pool row to
  show them on.
- `GET /admin/api/accounts/xai` lists each store account's name and xAI user id, the `sub` claim of
  its stored access token, never the token. The observed `grok` row carries the Grok CLI login's
  user id as its `uuid`, so the dashboard folds that login into the pooled account of the same user
  instead of listing it twice, as it does for Codex. The id is decoded without signature
  verification and only matches dashboard rows; the pool identity stays the account's file name
  (§2).

There are no provisioning routes. Add and remove accounts with the CLI (`shunt login xai --name`,
or by deleting the account file), as with Kimi (m15 §9).

## 12. Not covered

Operator-relevant limits in this version:

- No same-account retry on an upstream `401`. The account's next use refreshes its token (§7),
  and the mark is process-local, so a restart forgets it until the next `401`.
- No `needs_relogin` mark on the dashboard. A dead account is cooled down for 5 minutes and retried,
  as with Kimi.
- No quota-aware selection and no `max_utilization` caps. The polled billing buckets (§11) are
  display-only, and the caps stay inert.
- No retry after an upstream failure on the routed inbound Codex endpoint (§9); only an account
  whose credential fails to load is skipped there.
- No model discovery for `xai_oauth` (`src/discovery/upstream.rs` returns nothing).
- No WebSocket transport.
- No change to the `xai_oauth` https and host guard (§5).
- Some `tracing` messages in the shared pool loop still say "ChatGPT OAuth". This is cosmetic.

Deferred to the maintainer (see the design review in
`docs/plans/2026-10-08_xai-account-pool/PLAN.md` §5):

- **Live `403` capture (Q1 follow-up).** Rotate on a distinct tier-gate code only, once one is
  captured (§7).
- **`needs_relogin` for dead accounts (Q2).** Typed store errors would separate a terminal `400`/`401`
  refresh failure from a transient one.
- **Legacy `xai_oauth` client hardening (Q4).** The single-account `resolve_credential` arm still uses
  the gateway client. `token_refresh_client()` also does not refuse cross-origin HTTPS redirects.
