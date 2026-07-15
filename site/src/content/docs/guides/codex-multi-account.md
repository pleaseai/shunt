---
title: Codex Multi-Account
description: Pool ChatGPT/Codex subscription OAuth accounts with session-sticky selection and cooldown-based reactive failover.
---

shunt can pool multiple ChatGPT subscription OAuth credentials behind a `chatgpt_oauth` provider — the built-in `codex` provider, or any `responses` provider using that auth mode. Requests are session-sticky when Claude Code supplies `x-claude-code-session-id`; requests without it use per-provider round-robin. Unlike the [Anthropic account pool](/guides/anthropic-multi-account/), this pool is **reactive-only**: the ChatGPT/Codex backend sends no per-account quota headers, so there is no proactive near-quota rotation — an account is only avoided after it has actually failed.

:::caution[Subscription terms]
Use subscription credentials only where your account terms permit it. shunt is an unofficial client and does not change OpenAI's account or subscription policies.
:::

## Configure the pool

Log in with the Codex CLI first — shunt never performs its own ChatGPT login, only imports the credential `codex login` already wrote:

```bash
codex login
```

Then set `auth = "chatgpt_oauth"` and add explicit account entries:

```toml
[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"

# A second Codex CLI login, imported under a distinct name.
[[providers.codex.accounts]]
name = "primary"
credentials = "~/.shunt/accounts/codex/primary.json"

# A raw ChatGPT access token supplied out of band. Used verbatim; not refreshed.
[[providers.codex.accounts]]
name = "backup"
token_env = "CODEX_BACKUP_ACCESS_TOKEN"
```

```bash
export CODEX_BACKUP_ACCESS_TOKEN='<a valid ChatGPT access token>'
shunt check
shunt run
```

Store accounts by importing a Codex CLI login into shunt's own store:

```bash
shunt login codex --name primary
```

Then use name-only entries:

```toml
[[providers.codex.accounts]]
name = "primary"

[[providers.codex.accounts]]
name = "backup"
```

Store files live at `~/.shunt/accounts/codex/<name>.json`; set `SHUNT_CODEX_ACCOUNTS_DIR` to override the directory. If the configured `accounts` list is empty, shunt scans the store and uses all valid JSON account files in filename order. Store files are private (`0600`, with a `0700` directory on Unix) and hold the Codex CLI's own `auth.json` shape verbatim — unlike the Claude store, there is no wrapper object.

There is no `--long-lived` flag for `shunt login codex` — Codex has no setup-token concept, so every store-managed account is a refreshable OAuth login imported from an existing `codex login`.

## Account fields

| Field | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Unique label containing only lowercase letters, digits, and hyphens. Without another source field, resolves the matching shunt store file. |
| `credentials` | one usable source | Codex CLI `auth.json`-shaped file. shunt refreshes near expiry and atomically writes refreshed tokens back, same as `~/.codex/auth.json` itself. |
| `token_env` | one usable source | Environment variable containing a raw ChatGPT access token. Used verbatim; cannot be refreshed after a 401. |
| `uuid` | no | Present for structural parity with Anthropic accounts, but **unused** by the Codex path — the account id is resolved from the store or the access token's JWT claim instead. |
| `priority` | no | Selection priority among available accounts; lower is preferred, default `100`. Honored on the Codex path. |
| `disabled` | no | `true` removes the account from selection entirely while keeping it in config. Honored on the Codex path. |

Do not set both `credentials` and `token_env` on one account.

## Account id resolution

Codex has no explicit `uuid` field to configure. Instead, for each account shunt:

1. prefers a stored `tokens.account_id`;
2. otherwise decodes the `access_token` JWT and reads the `chatgpt_account_id` claim; and
3. after any refresh, recomputes the id **only** from the new access token's JWT claim (a refresh response has no separate account-id field).

If neither source yields an id, that account fails to resolve and is treated as a credential-resolution failure below.

## Selection and cooldowns

- With `x-claude-code-session-id`: a stable hash picks the sticky account, same mechanism as the Anthropic pool.
- Without the header: each provider has its own round-robin counter.
- No quota headers are parsed on this path — Codex sends none — so there is no proactive switch away from a healthy sticky account. An account keeps its turn until it actually fails.
- A successful response clears that account's cooldown.

| Trigger | Cooldown |
| :-- | :-- |
| Credential-resolution failure (account id/tokens unresolvable) | 5 minutes |
| Transport/connection failure | 30 seconds |
| 5xx upstream response | 30 seconds |
| 429 upstream response | numeric `retry-after`, clamped to 1–3600s, default 60s |
| 401 from a `token_env` account | 5 minutes |
| 401 from a `credentials`/store account, refresh fails or the retry is still 401 | 5 minutes |

## Failover rules

| Response | Behavior |
| :-- | :-- |
| 2xx | Relay and mark healthy. |
| 429 | **Always** cooldown and rotate — Codex has no per-account quota header to distinguish quota exhaustion from transient throttling, so (unlike the Anthropic pool's plain-429 same-account retry) every 429 is treated as exhaustion of that account. |
| 401 with `credentials`/store | Force-refresh, retry the same account once; if the refresh fails, or the retry is still 401, cooldown 5 minutes and rotate. |
| 401 with `token_env` | Cannot refresh: cooldown 5 minutes and rotate. |
| 5xx or transport failure | Cooldown 30 seconds and rotate. |
| Credential-resolution failure | Cooldown 5 minutes and rotate. |
| Other status (e.g. `400`) | Relay without failover; mark the account healthy. |

Classification happens before the response body streams, so a mid-stream failure is never replayed. If the pool exhausts its attempts after receiving at least one upstream response, shunt relays a **translated** Anthropic-style error envelope built from that last response — the status (e.g. `429`) is preserved, but the body is re-shaped, not relayed verbatim. This is the opposite of the Anthropic pool, which relays the last upstream response byte-for-byte. If every account fails before any upstream response exists, shunt returns a gateway-owned `502` with the message `all Codex OAuth accounts failed before receiving an upstream response`.

## Request and response changes

For the selected account, shunt sends the same Codex-CLI identity headers as the single-account path, populated from that account's credential:

```http
authorization: Bearer <selected account's access token>
chatgpt-account-id: <selected account's account id>
originator: codex_cli_rs
OpenAI-Beta: responses=experimental
```

Pooled responses identify the account:

```http
x-shunt-account: backup
```

Use neutral account names on a shared gateway — this header exposes the configured label to every authorized client that receives the response. The final translated-error relay after pool exhaustion omits `x-shunt-account`.

### WebSocket transport

If the provider also sets `websocket = true` (see [ChatGPT / Codex](/guides/codex/)), the pooled connection cache key is prefixed per account so two accounts never share a socket or its `previous_response_id` continuation state. A WebSocket failure **before** any token streams falls back to HTTP on the **same** account (not a pool rotation); only an HTTP-path failure triggers the failover above.

## Security constraints

`chatgpt_oauth` is accepted only when:

- `base_url` uses HTTPS; and
- its host is `chatgpt.com` or a subdomain.

Like `xai_oauth`, **`chatgpt_oauth` requires `kind = "responses"`** — the Codex backend's kind, shared with `openai` and `xai`. This is a bearer-leak guard: only the Responses adapter injects the Codex bearer, so a mismatched `kind = "anthropic"` provider would be routed to the Anthropic adapter and forward the client's own credential off-origin to `chatgpt.com` instead. These startup checks prevent an OAuth bearer from being sent off-origin or over plaintext. The HTTPS and host checks are **relaxed for loopback hosts** (`localhost`, `127.0.0.1`, `[::1]`, etc.): a loopback `base_url` may use plain HTTP and any host, so a local debugging proxy or mock can receive the traffic. Non-loopback hosts are always held to HTTPS + `chatgpt.com`. On a shared deployment, also configure [`[server.auth]`](/guides/shared-gateway/) because `chatgpt_oauth` spends gateway-owned credentials.

## Unaffected: the single-account path

A `chatgpt_oauth` provider with no `accounts` configured (the default `codex` provider) behaves exactly as before: it reads and refreshes `~/.codex/auth.json` (or `$CODEX_AUTH_FILE`) directly, with no cooldowns and no `x-shunt-account` header.

## Remaining follow-up

- **Quota-aware proactive rotation:** the Anthropic pool's near-quota early switch has no Codex equivalent yet — it needs a live probe of what (if anything) the Codex backend exposes as per-account rate-limit state.
- **Admin web provisioning:** the opt-in [admin surface](/guides/admin-remote-provisioning/) can run ChatGPT OAuth in the browser, store a refreshable Codex account, and show it in the shared pool table. Codex provides no quota headers, so utilization columns remain empty (`—`).
- **Storm-control:** ramping a freshly switched account's concurrency remains unimplemented for both pools.

See the [M10 behavior specification](https://github.com/pleaseai/shunt/blob/main/docs/m10-codex-multi-account.md) for the full account-pool internals, and the [ChatGPT / Codex guide](/guides/codex/) for single-account setup, model routing, and effort/context configuration.
