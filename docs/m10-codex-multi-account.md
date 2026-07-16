# M10 — Codex/ChatGPT multi-account + load balancing

M10 adds an account pool to a Codex/ChatGPT provider authenticated with ChatGPT subscription OAuth, mirroring [M8's](m8-anthropic-multi-account.md) Anthropic account pool onto `auth = "chatgpt_oauth"`. shunt chooses an account per request, injects that account's ChatGPT bearer, and retries another account after an upstream failure before relaying a response to Claude Code.

M10 remains **reactive-only**, unlike M8. Three differences from the Anthropic pool are worth calling out up front:

1. **Cooldown-based reactive failover, no proactive rotation.** shunt now parses the ChatGPT/Codex backend's `x-codex-*` 5-hour and 7-day windows for display in the admin dashboard, but deliberately excludes that state from `select_order()`. Selection remains pure session-sticky-or-round-robin, filtered by cooldown and account priority/disabled state; an account is only avoided after it has actually failed.
2. **No `account_uuid` rewrite.** M8's Anthropic pool rewrites an existing `metadata.user_id.account_uuid` to the selected account's Anthropic UUID. Codex has no equivalent request-body identity field to rewrite — the account id is embedded in the OAuth token itself (see below) and sent via the `chatgpt-account-id` header, not a JSON body field. The shared `AccountConfig.uuid` field exists structurally (it's the same struct as Anthropic's) but is never read on the Codex path; setting it on a `chatgpt_oauth` account is harmless but has no effect.
3. **Errors are translated, not relayed verbatim.** When the pool is exhausted after seeing at least one upstream response, shunt re-shapes that last response into an Anthropic-style error envelope (`build_upstream_error`) rather than relaying the raw Codex/OpenAI body — the opposite of M8, which relays the last upstream response unchanged. If every account fails before any upstream response exists (for example, every credential fails to resolve), shunt returns a gateway-owned `502 bad gateway` with the fixed message `all Codex OAuth accounts failed before receiving an upstream response`.

## Configuration

Set `auth = "chatgpt_oauth"` on a `kind = "responses"` provider and configure one or more `[[providers.<name>.accounts]]` entries. The built-in `codex` provider (see [`codex-configuration.md`](codex-configuration.md)) is the usual target, but any `responses` provider using `chatgpt_oauth` qualifies.

Provision store-managed accounts before editing the provider configuration. `shunt login codex` does not perform its own OAuth login — it imports the credential the external `codex` CLI already wrote:

```bash
# Log in with the Codex CLI first, if you haven't already.
codex login

# Import ~/.codex/auth.json (or $CODEX_AUTH_FILE) into shunt's account store.
shunt login codex --name main
```

Then configure the provider and its accounts:

```toml
[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"

# Store-managed account from ~/.shunt/accounts/codex/main.json.
[[providers.codex.accounts]]
name = "main"

# A second Codex CLI login, imported under a different name.
[[providers.codex.accounts]]
name = "backup"
credentials = "~/.shunt/accounts/codex/backup.json"

# A raw ChatGPT access token supplied out of band. Not refreshed.
[[providers.codex.accounts]]
name = "static"
token_env = "CODEX_STATIC_ACCESS_TOKEN"
```

Then set the token environment variable before starting shunt, if you used `token_env`:

```bash
export CODEX_STATIC_ACCESS_TOKEN='<a valid ChatGPT access token>'
shunt check
shunt run
```

Each account has these fields:

| Field | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Stable account label. Must contain only lowercase ASCII letters, digits, and hyphens. Names must be unique within the provider. A name-only entry resolves from the shunt account store. |
| `credentials` | no | Path to a Codex CLI `auth.json`-shaped file. shunt reads the `tokens` block, refreshes near expiry, and writes refreshed tokens back atomically — same read/refresh/write-back cycle as `~/.codex/auth.json` itself (see [`codex-configuration.md`](codex-configuration.md#4-authentication-codexauthjson)). |
| `token_env` | no | Environment variable containing a raw ChatGPT access token. The value is used verbatim and is **not** refreshed; a 401 cools the account down instead of retrying. |
| `uuid` | no | Present on the shared `AccountConfig` struct for parity with the Anthropic pool, but **unused by the Codex path** — the account id comes from the JWT claim or store instead (see below). |
| `priority` | no | Selection priority among available accounts; lower is preferred, default `100`. Unlike `uuid`, this **is** honored on the Codex path. |
| `disabled` | no | `true` removes the account from selection entirely while keeping it in config. Honored on the Codex path. |

`credentials` and `token_env` are mutually exclusive. A name-only account reads `~/.shunt/accounts/codex/<name>.json` (override the directory with `SHUNT_CODEX_ACCOUNTS_DIR`). With an entirely empty `accounts` list, shunt scans that directory and uses every valid `*.json` account in filename order. Store files are written atomically at `0600`, and the store directory is `0700` on Unix — the account is stored **verbatim** in the Codex CLI's own `auth.json` shape (no `claudeAiOauth`-style wrapper, unlike the Claude store).

`shunt login codex --name <name>` imports the current login from `$CODEX_AUTH_FILE` (or `~/.codex/auth.json`) into the store without modifying the source file, and validates it is in `ChatGPT` auth mode with non-empty access and refresh tokens. Reusing a name replaces that store file. There is no `--long-lived` equivalent — Codex has no setup-token concept; every store-managed account is a refreshable OAuth login.

The built-in `codex` provider remains `auth = "chatgpt_oauth"` by default even without a pool — a single-account `chatgpt_oauth` provider with no `accounts` configured behaves exactly as before M10 (see the "existing single-account path" note below). Multi-account pooling is opt-in via `[[providers.codex.accounts]]`.

### Account id resolution

Unlike Anthropic accounts (which carry an explicit `uuid` field), a Codex account's id is derived automatically:

- A store/`credentials` account prefers the stored `tokens.account_id`; if absent, shunt decodes the `access_token` JWT and reads the `chatgpt_account_id` claim at `https://api.openai.com/auth.chatgpt_account_id`.
- After a refresh, the new credential's account id comes **only** from the new access token's JWT claim (the refresh response has no separate `account_id` field to fall back to).
- A `token_env` account's id also comes from decoding its JWT.

If neither the store nor the JWT yields an account id, resolving that account fails and it is treated as a credential-resolution failure (cooled down, pool rotates — see below).

## Validation and security guards

Configuration validation rejects:

- `accounts` on a provider whose auth mode is neither `claude_oauth` nor `chatgpt_oauth`;
- a `chatgpt_oauth` provider whose `kind` is not `responses`;
- a non-HTTPS `base_url` for a `chatgpt_oauth` provider (unless the host is loopback — see below);
- a `chatgpt_oauth` `base_url` host other than `chatgpt.com` or one of its subdomains (unless loopback);
- duplicate or invalid account names; and
- an account that sets both `credentials` and `token_env`.

`chatgpt_oauth` requires `kind = "responses"` (the Codex backend's kind, shared with the `openai` and `xai` providers), just as `claude_oauth` requires `kind = "anthropic"` and `xai_oauth` requires `kind = "responses"`. This is a bearer-leak guard, not a cosmetic check: only the Responses adapter injects the Codex bearer, so a mismatched `kind = "anthropic"` provider would instead be dispatched to the Anthropic adapter, which has no `ChatGptOAuth` injection and would forward the *client's own* credential off-origin to `chatgpt.com`. Validation rejects that combination at boot.

The HTTPS and host checks are the same bearer-leak guard M8 uses for Anthropic: a ChatGPT subscription OAuth token is never injected toward an arbitrary gateway or over plaintext. Both checks are **skipped when the `base_url` host is a loopback address** (`localhost`, `127.0.0.1`, `[::1]`, etc.), so a local debugging proxy or mock can be pointed at over plaintext HTTP. Every non-loopback host is still held to HTTPS + `chatgpt.com` (or a subdomain).

Because `chatgpt_oauth` is an injected-credential mode, a configured `[server.auth]` also protects it on a shared shunt gateway.

## Selection and cooldowns

Selection state is per provider and survives config hot reloads for the life of the shunt process.

- If the request includes `x-claude-code-session-id`, shunt hashes it with SHA-256 to choose the sticky account — the same mechanism M8 uses, unchanged.
- Without that header, shunt uses an independent round-robin counter for each provider.
- Successful pooled responses populate the admin dashboard from `x-codex-primary-*` and `x-codex-secondary-*` headers. Each group is mapped by `window-minutes` (about 300 minutes → 5h; about 10080 minutes → 7d); other windows are ignored, and Codex has no `7d_oi` analog. This is display-only: `select_order_cooldown()` ignores the recorded quota, so there is no proactive switch away from a healthy sticky account. An account is only skipped once it is in cooldown.
- A successful response clears that account's cooldown.

Cooldown durations:

| Trigger | Cooldown |
| :-- | :-- |
| Credential-resolution failure (account id or tokens unresolvable) | 5 minutes |
| Transport/connection failure | 30 seconds |
| 5xx upstream response | 30 seconds |
| 429 upstream response | `retry-after` (numeric seconds), clamped to 1–3600 seconds, default 60 seconds if absent/unparsable |
| 401 from a `token_env` account (cannot be refreshed) | 5 minutes |
| 401 from a store/`credentials` account, refresh fails | 5 minutes |
| 401 from a store/`credentials` account, refresh succeeds but the retry is still 401 | 5 minutes |

## Failover behavior

shunt classifies the upstream response before streaming its body. It never retries a response after streaming has begun.

| Upstream result | Action |
| :-- | :-- |
| 2xx | Relay immediately and mark the selected account healthy. |
| 429 | **Always** cool the account down (per the table above) and rotate — unlike M8's Anthropic pool, there is no `PauseSame`/same-account-retry branch for Codex. The optional `x-codex-rate-limit-reached-type` rejection signal is recorded for display but does not change failover classification. Every 429 is treated as exhaustion of that account for now. |
| 401 from a `credentials`/store account | Force-refresh via `CodexAuthStore::force_refresh()` (skips the local expiry check and always refreshes), retry the same account once. If the refresh itself fails, cool the account for 5 minutes and rotate. If the retry succeeds, relay it. If the retry is still 401, cool the account for 5 minutes and rotate ("genuinely broken"). If the retry fails a different way, fall through to that failure's own classification. |
| 401 from a `token_env` account | Cannot be refreshed (the token is used verbatim); cool the account for 5 minutes and rotate. |
| 5xx | Cool the account for 30 seconds and rotate. |
| Credential-resolution failure (account id/tokens unresolvable before any request is sent) | Cool the account for 5 minutes and rotate. |
| Other status (e.g. a client-error `400`) | Relay immediately as a translated client error and mark the account healthy — no rotation. |

When attempts are exhausted after receiving at least one upstream response, shunt relays a **translated Anthropic-style error envelope** built from that last response (`build_upstream_error`) — the response status (e.g. `429`) is preserved, but the body is re-shaped, not relayed verbatim. This is the opposite of M8, which relays the Anthropic pool's last upstream response byte-for-byte. If every account fails before any upstream response exists (for example, every account's credentials fail to resolve), shunt returns a gateway-owned `502 bad gateway` with the message `all Codex OAuth accounts failed before receiving an upstream response`.

## Request and response shaping

For the selected account, shunt sends the same Codex-CLI identity headers as the single-account path (see [`codex-configuration.md` §4.4](codex-configuration.md#4-authentication-codexauthjson)), with the bearer and account-id header populated from the *selected pool account* rather than the default `~/.codex/auth.json`:

```http
authorization: Bearer <selected account's access token>
chatgpt-account-id: <selected account's account id>
originator: codex_cli_rs
OpenAI-Beta: responses=experimental
```

A pooled upstream response includes:

```http
x-shunt-account: backup
```

Same caveat as M8: use neutral labels (`primary`, `backup-1`, `pool-a`) rather than names or emails on a shared gateway, since this header exposes the configured account name to clients. The final translated-error relay after pool exhaustion does not include `x-shunt-account`; a name that fails the `[a-z0-9-]+` pre-validation is silently omitted rather than causing an error.

### WebSocket transport (`websocket = true`)

If the provider also opts into the experimental Codex WebSocket transport, the pooled connection cache key is prefixed per account — `format!("{}::{key}", account.name)` — so two accounts never share a pooled socket or its `previous_response_id` continuation state. A WebSocket failure that happens **before** any token has streamed falls back to plain HTTP **on the same account** (not a pool rotation); only an HTTP-path failure triggers the account failover described above. A failure **after** streaming has begun surfaces as an error event, same as the single-account WS path. Rate-limit headers from a successful WebSocket upgrade are recorded for that account; reused/prewarmed sockets have no fresh handshake, so their dashboard usage refreshes only when a new connection is established.

## Existing single-account path is unaffected

A `chatgpt_oauth` provider with no `[[providers.<name>.accounts]]` configured (the default `codex` provider, as documented in [`codex-configuration.md`](codex-configuration.md)) behaves byte-identically to before M10: it reads and refreshes `~/.codex/auth.json` (or `$CODEX_AUTH_FILE`) directly, with no pool, no cooldowns, and no `x-shunt-account` header.

## Out of scope / follow-up

- **Quota-aware proactive rotation.** Codex's `x-codex-*` windows are currently display-only. Feeding them into selection would be a separate behavior change; failover intentionally remains cooldown-based.
- **Storm-control.** Same open follow-up as M8: ramping concurrency after switching to a fresh account is not implemented for either pool.
