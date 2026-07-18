# M10 — Codex/ChatGPT multi-account + load balancing

M10 adds an account pool to a Codex/ChatGPT provider authenticated with ChatGPT subscription OAuth, mirroring [M8's](m8-anthropic-multi-account.md) Anthropic account pool onto `auth = "chatgpt_oauth"`. shunt chooses an account per request, injects that account's ChatGPT bearer, and retries another account after an upstream failure before relaying a response to Claude Code.

Since issue #195, M10 selection is **quota-aware**, sharing M8's selection machinery. Three differences from the Anthropic pool are worth calling out up front:

1. **Same proactive selection engine, Codex-shaped quota signal.** shunt parses the ChatGPT/Codex backend's `x-codex-*` 5-hour and 7-day windows and feeds them into the same `select_order()` used by the Anthropic pool: a near-quota sticky account proactively yields, available accounts order by `priority` then (with `[server.pool]`) burn-rate headroom, and `[server.pool]` thresholds/`burn_rate_avoidance` apply. Codex has no `7d_oi` (Fable) analog, so only the 5-hour and shared weekly windows govern; the optional `x-codex-rate-limit-reached-type` value is recorded for display but is not the Anthropic `rejected` signal and does not itself mark an account near quota. Reactive cooldown-based failover remains the safety floor.
2. **No `account_uuid` body rewrite.** M8's Anthropic pool rewrites an existing `metadata.user_id.account_uuid` to the selected account's Anthropic UUID. Codex has no equivalent request-body field — its account id is sent via the `chatgpt-account-id` header. The pool stores the resolved `account_id` in the shared `AccountConfig.uuid` field as its provider-independent stable identity so aliases can be coalesced; it is never written into the Codex request body.
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
| `uuid` | no | Stable upstream identity used to coalesce aliases. A name-only entry (resolved by a store scan) is filled in automatically from `tokens.account_id` or the access-token JWT *before* selection runs, so store-scanned aliases coalesce without any config change. A `credentials`- or `token_env`-configured entry's account id is resolved only *after* selection, for the request it sends — it never populates `uuid`, so such an entry's identity is its `uuid` when set, else its `name`; it coalesces with another alias whenever that identity equals the other alias's explicit `uuid` or name-fallback identity — set a matching nonblank `uuid` on both entries for a clear, intentional coalesce (shunt also warns on an accidental cross `uuid`/`name` collision). It is not written into the Codex request body. |
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

If neither the store nor the JWT yields an account id, resolving that account fails and it is treated as a credential-resolution failure (cooled down, pool rotates — see below). Only a store-scanned account (an empty `accounts` list) has its resolved id written into the in-memory account's `uuid` field ahead of selection, which is what lets store-scanned aliases coalesce automatically (see `uuid` above); a `credentials`/`token_env`-configured account's id is resolved per request and never feeds back into `uuid`. The store *scan* itself is cached by the account-store directory mtime, so steady-state discovery performs one directory `stat` and no credential-file reads — but that only covers discovering which accounts exist; resolving the selected account's actual credential (the `get_valid_chatgpt()` call above) still reads its credential file per request as needed to check/refresh expiry.

### Identity coalescing

Accounts with the same resolved `account_id` are one logical pool candidate even when imported under different names. They share cooldown, health, and refresh locks, and failover skips duplicate aliases instead of retrying the same ChatGPT account twice. Sticky hashing and round-robin operate over distinct identities. The deterministic representative is the enabled alias with the lowest `priority`, then the first entry; only that representative token is attempted. Duplicate identities emit a warning — configured (`credentials`/`token_env`) collisions are logged on every `Config::validate` call (including hot-reload), while store-discovered collisions are logged once per distinct collision set (deduped by a process-wide fingerprint keyed by provider + store directory, so a filesystem that cannot report mtime does not re-log on every request), not once per request. If the representative token is invalid while a non-representative alias remains valid, the latter is intentionally not tried because both aliases count as one account. This coalescing needs each alias's `uuid` to actually be populated at selection time — see the `uuid` field and "Account id resolution" above for when that happens automatically versus only when set explicitly.

Deleting a store-managed account through the admin web surface (`DELETE /admin/accounts/codex/<name>`) clears that identity's process-lifetime pool health only once no other stored alias still resolves to the same identity — a scan of the remaining store accounts confirms that first, and a scan failure preserves the health rather than risking wiping out state a sibling alias still relies on. This is admin-store delete semantics specifically; removing a `credentials`/`token_env` entry from the TOML config (or deleting a credentials file directly) does not go through this cleanup at all.

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
- Successful pooled responses record quota from `x-codex-primary-*` and `x-codex-secondary-*` headers. Each group is mapped by `window-minutes` (about 300 minutes → 5h; about 10080 minutes → 7d); other windows are ignored, and Codex has no `7d_oi` analog. The recorded windows feed both the admin dashboard and `select_order()` (issue #195): a sticky account at or past its threshold (the built-in `0.98`, or the `[server.pool]`/per-account soft thresholds) proactively yields to an account with more headroom before it ever returns a 429.
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

## Storm control (`ramp_initial_concurrency`)

Issue #195 also closes M10's storm-control gap. With `[server.pool] ramp_initial_concurrency = N` set (unset or `0` disables it, the default), every account pool — Codex and Anthropic alike — gates concurrent admissions per upstream identity with a slow-start ramp:

- An identity that just started taking traffic (fresh entry, back from a cooldown, or idle for 60 seconds) admits at most `N` concurrent requests.
- Each successful response doubles the allowance, so a healthy account leaves the ramp within a handful of turns.
- A request denied admission spills to the next account in selection order instead of piling on; the **last** remaining candidate is always attempted regardless of the gate, so a saturated pool degrades to pre-#195 behavior rather than failing the request.
- A failover-worthy failure (429/5xx/401/transport) restarts that identity's ramp.
- Gating is per upstream identity and only ever defers to *other* candidates, so a pool whose accounts all resolve to a single identity is effectively ungated — its only candidate is also its last candidate and is always admitted.

This prevents the post-failover stampede where every in-flight request lands on the freshly selected account at once.

## Out of scope / follow-up

- **Quota-aware rotation and storm control shipped with issue #195** (see above); both former follow-ups are closed.
- **Out-of-band usage reconciliation.** The Anthropic pool can poll `GET /api/oauth/usage` (`usage_refresh_seconds`); Codex has no equivalent usage API, so its quota state refreshes only from response headers.
