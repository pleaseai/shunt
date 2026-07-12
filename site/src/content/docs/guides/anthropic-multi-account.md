---
title: Anthropic Multi-Account
description: Pool Claude subscription OAuth accounts with session-sticky, model-aware proactive rotation and reactive failover.
---

shunt can pool multiple Claude subscription OAuth credentials behind the built-in `anthropic` provider. Requests are session-sticky when Claude Code supplies `x-claude-code-session-id`; requests without it use per-provider round-robin. shunt tracks each account's upstream quota headers and proactively rotates when the sticky account nears the model-relevant quota, while quota rejection, authentication failures, and upstream failures retain reactive failover as the safety floor.

:::caution[Subscription terms]
Use subscription credentials only where your account terms permit it. shunt is an unofficial client and does not change Anthropic's account or subscription policies.
:::

## Configure the pool

Set `auth = "claude_oauth"` and add explicit account entries:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

# Existing Claude Code credentials file. shunt refreshes and writes it back.
[[providers.anthropic.accounts]]
name = "primary"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000" # optional

# Long-lived `claude setup-token` value. Used verbatim; not refreshed.
[[providers.anthropic.accounts]]
name = "backup"
token_env = "CLAUDE_BACKUP_OAUTH_TOKEN"
uuid = "11111111-1111-1111-1111-111111111111" # optional
```

```bash
export CLAUDE_BACKUP_OAUTH_TOKEN='<value from claude setup-token>'
shunt check
shunt run
```

Store accounts with either login mode:

```bash
# Import your current refreshable Claude Code login.
shunt login claude --name primary

# Or generate and store a one-year setup token.
shunt login claude --name backup --long-lived
```

Then use name-only entries:

```toml
[[providers.anthropic.accounts]]
name = "primary"

[[providers.anthropic.accounts]]
name = "backup"
```

Store files live at `~/.shunt/accounts/claude/<name>.json`; set `SHUNT_CLAUDE_ACCOUNTS_DIR` to override the directory. If the configured `accounts` list is empty, shunt scans the store and uses all valid JSON account files in filename order. Store files are private (`0600`, with a `0700` directory on Unix).

The non-`--long-lived` command copies the current `~/.claude/.credentials.json` login into shunt's store and preserves its refresh capability. `--long-lived` runs the interactive `claude setup-token` browser flow, then asks you to paste the generated token into a hidden prompt. shunt does not print the token. Reusing a name replaces that account's store file.

## Account fields

| Field | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Unique label containing only lowercase letters, digits, and hyphens. Without another source field, resolves the matching shunt store file. |
| `credentials` | one usable source | Claude Code `.credentials.json`-shaped file. `~/` is expanded. shunt refreshes near expiry and atomically writes refreshed tokens back. |
| `token_env` | one usable source | Environment variable containing a setup token. The value is used verbatim and cannot be refreshed after a 401. |
| `uuid` | no | Selected account's Anthropic UUID for rewriting an existing `metadata.user_id.account_uuid`. |

Do not set both `credentials` and `token_env` on one account.

## Selection and proactive rotation

- With `x-claude-code-session-id`: a stable hash picks the sticky account. If that account is available and under the switch threshold, shunt keeps it first.
- Without the header: each provider has its own round-robin counter.
- On every upstream response handled by the `claude_oauth` account pool, shunt records these headers when present:
  - `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, and `anthropic-ratelimit-unified-7d_oi-utilization`;
  - `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, and `anthropic-ratelimit-unified-7d_oi-reset` (Unix seconds); and
  - `anthropic-ratelimit-unified-status`.
- The switch threshold is `0.98`. An account is near quota when unified status is `rejected`, shared 5-hour utilization is at least `0.98`, or the governing weekly utilization is at least `0.98`.
- The 5-hour bucket applies to every model. Fable model ids use the `7d_oi` weekly bucket when its utilization is present, with shared `7d` as fallback. Every other model family uses shared `7d`; Sonnet also uses `7d` because there is no Sonnet-specific header today.
- A near-quota or cooled sticky account rotates off proactively. shunt prefers available under-threshold accounts ordered by the soonest-resetting governing weekly bucket, spending use-or-lose quota first. Accounts with unknown weekly reset sort first. Available near-quota accounts follow, then cooled accounts ordered by soonest recovery.
- shunt never fails closed because of local quota state: every account remains in the attempt order, even if all are near quota or cooled.
- Quota buckets are cleared automatically after their reset timestamp passes. A successful response clears the selected account's cooldown.

The pool's selection, cooldown, and quota state survives config hot reloads for the life of the process. Reactive failover remains active if proactive rotation cannot avoid the upstream limit.

## Failover rules

| Response | Behavior |
| :-- | :-- |
| 2xx | Relay and mark healthy. |
| 429 plus a `rejected` value in `anthropic-ratelimit-unified-5h-status`, `-7d-status`, or `-7d_oi-status` | Quota exhausted: cooldown using numeric `retry-after` (default 60s, clamped to 1–3600s), then rotate. |
| Plain 429 | Transient throttle: wait using numeric `retry-after` (default 1s, cap 300s), retry the **same** account once, then relay that retry response. |
| 401 with `credentials` | Force-refresh, retry the same account once; if still 401, cooldown 5 minutes and rotate. |
| 401 with `token_env` or a store-managed setup token | Cannot refresh: cooldown 5 minutes and rotate. |
| 5xx or transport failure | Cooldown 30 seconds and rotate. |
| Other status | Relay without failover. |

Classification happens before the response body streams, so a mid-stream failure is never replayed. If the pool exhausts its attempts after receiving responses, the client gets the last real upstream status and body. If every account fails before any upstream response, shunt returns a gateway-owned error.

Anthropic-routed `POST /v1/messages/count_tokens` requests use the same pool.

## Request and response changes

For the selected account, shunt replaces client auth with:

```http
Authorization: Bearer <selected OAuth token>
anthropic-beta: ...,oauth-2025-04-20
```

It removes both incoming `authorization` and `x-api-key`, appends `oauth-2025-04-20` only when absent, and preserves other end-to-end headers.

Pooled responses identify the account:

```http
x-shunt-account: backup
```

Use neutral account names on a shared gateway. This header exposes the configured label to every authorized client that receives the response. The final last-upstream-response relay after pool exhaustion omits `x-shunt-account`.

### `account_uuid`

Claude Code may encode account metadata as JSON inside the string-valued `metadata.user_id`. If the selected account has `uuid`, shunt replaces an **existing** inner `account_uuid` with that value. It leaves the body untouched if the metadata is absent, malformed, lacks `account_uuid`, or the selected account has no UUID. It does not inject missing metadata.

## Security constraints

`claude_oauth` is accepted only when:

- the provider has `kind = "anthropic"`;
- `base_url` uses HTTPS; and
- its host is `anthropic.com` or a subdomain such as `api.anthropic.com`.

These startup checks prevent an OAuth bearer from being sent off-origin or over plaintext. On a shared deployment, also configure [`[server.auth]`](/guides/shared-gateway/) because `claude_oauth` spends gateway-owned credentials.

## Remaining follow-up

- **Storm-control:** ramping a freshly switched account's concurrency remains a later follow-up and is not implemented.

The implementation behavior was informed by [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) and the shipped Claude Code binary. shunt has no runtime dependency on teamclaude.
