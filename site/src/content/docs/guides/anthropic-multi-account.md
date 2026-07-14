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

For remote operators, the opt-in [admin web surface](/guides/admin-remote-provisioning/) can provision one-year setup-token accounts in a browser and show the pool's current health; the refreshable import flow remains CLI-only.

The non-`--long-lived` command copies the current `~/.claude/.credentials.json` login into shunt's store, preserves its refresh capability, and records the current account UUID. `--long-lived` runs the same one-year, inference-only PKCE flow as `claude setup-token`; after approval, shunt exchanges the displayed authorization code and stores both the token and its issuing account UUID without printing the token. This keeps `metadata.user_id.account_uuid` aligned when the pool selects a different account. Reusing a name replaces that account's store file. Existing external setup tokens still need `token_env` plus an explicit `uuid`.

## Account fields

| Field | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Unique label containing only lowercase letters, digits, and hyphens. Without another source field, resolves the matching shunt store file. |
| `credentials` | one usable source | Claude Code `.credentials.json`-shaped file. `~/` is expanded. shunt refreshes near expiry and atomically writes refreshed tokens back. |
| `token_env` | one usable source | Environment variable containing a setup token. The value is used verbatim and cannot be refreshed after a 401. |
| `uuid` | no | Selected account's Anthropic UUID for rewriting an existing `metadata.user_id.account_uuid`. |
| `threshold` | no | Per-account soft quota threshold in `[0.0, 1.0]` for every window without a per-window value. A low value marks a backup account that rotates out early. |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | no | Per-window soft thresholds; each beats `threshold` for its window. |
| `priority` | no | Selection priority when the sticky account is unhealthy; lower is preferred, default `100`. |
| `disabled` | no | `true` removes the account from selection entirely while keeping it in config and on the admin dashboard. |

Do not set both `credentials` and `token_env` on one account.

## Selection and proactive rotation

- With `x-claude-code-session-id`: a stable hash picks the sticky account. If that account is available and under the switch threshold, shunt keeps it first.
- Without the header: each provider has its own round-robin counter.
- On every upstream response handled by the `claude_oauth` account pool, shunt records these headers when present:
  - `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, and `anthropic-ratelimit-unified-7d_oi-utilization`;
  - `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, and `anthropic-ratelimit-unified-7d_oi-reset` (Unix seconds); and
  - `anthropic-ratelimit-unified-status`.
- The default switch threshold is `0.98`. An account is near quota when unified status is `rejected`, shared 5-hour utilization reaches its threshold, or the governing weekly utilization reaches its threshold. Thresholds can be lowered per account (`threshold*` fields above) or pool-wide (see [Tuning selection](#tuning-selection-serverpool)).
- The 5-hour bucket applies to every model. Fable model ids use the `7d_oi` weekly bucket when its utilization is present, with shared `7d` as fallback. Every other model family uses shared `7d`; Sonnet also uses `7d` because there is no Sonnet-specific header today.
- A near-quota, cooled, or disabled sticky account rotates off proactively. shunt prefers available under-threshold accounts ordered by `priority` (lower first), then by the soonest-resetting governing weekly bucket, spending use-or-lose quota first. Accounts with unknown weekly reset sort first. Available near-quota accounts follow, then cooled accounts ordered by soonest recovery. With `[server.pool]` configured, burn-rate headroom replaces the weekly-reset tiebreak (see below).
- shunt never fails closed because of local quota state: every non-`disabled` account remains in the attempt order, even if all are near quota or cooled.
- Quota buckets are cleared automatically after their reset timestamp passes. A successful response clears the selected account's cooldown.

The pool's selection, cooldown, and quota state survives config hot reloads for the life of the process. Reactive failover remains active if proactive rotation cannot avoid the upstream limit.

## Tuning selection (`[server.pool]`)

The optional `[server.pool]` table (issue #135) adds soft per-window thresholds and burn-rate–aware ordering on top of the behavior above. Without the table, selection uses the single built-in `0.98` threshold exactly as before.

```toml
[server.pool]
# hard_threshold = 0.98      # (default) backstop; at/above always sorts last
default_threshold = 0.9      # soft default for every window
default_threshold_5h = 0.95  # per-window overrides
default_threshold_fable = 0.85
burn_rate_avoidance = true   # avoid accounts projected to hit a threshold before reset
usage_refresh_seconds = 300  # reconcile out-of-band usage for refreshable accounts

[[providers.anthropic.accounts]]
name = "primary"
priority = 1                 # preferred whenever the sticky account is unhealthy

[[providers.anthropic.accounts]]
name = "backup"
threshold = 0.5              # backup: rotate out once half its quota is spent

[[providers.anthropic.accounts]]
name = "spare"
disabled = true              # kept configured, never selected
```

- **Threshold resolution.** For each window `X` (`5h`, `7d`, `fable`), the effective soft threshold is: account `threshold_X` → account `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, capped at `hard_threshold`. All values are utilization fractions in `[0.0, 1.0]`; out-of-range values fail `shunt check`.
- **Burn-rate headroom.** From each window's utilization and reset instant (window lengths are fixed at 5 hours and 7 days), shunt projects the time until the soft threshold is hit at the observed average pace, minus the time until the window resets. Positive headroom means the account survives to its reset at the current pace. Available accounts of equal `priority` order by largest headroom; unobserved windows count as unlimited headroom.
- **Predictive avoidance.** With `burn_rate_avoidance = true`, an account with negative projected headroom is treated as near quota and rotated off *before* it hits a threshold. Off by default — ordering by headroom happens regardless.
- **All-near guard.** When every account is past a soft threshold (or predicted to exhaust), the pool does not empty: near accounts serve ordered by best headroom, while accounts at or above `hard_threshold` still sort last, followed only by cooling accounts.
- **Scope.** The quota knobs act on Claude (Anthropic) pools only — the Codex backend sends no quota headers, so for [Codex pools](/guides/codex-multi-account/) they are inert, while `priority` and `disabled` still apply.
- The admin pool endpoint (`GET /admin/pool`) reports each account's `priority`, `disabled` flag, and — when `[server.pool]` is configured — its current headroom projection in seconds; the dashboard's state column marks disabled accounts.

## Usage-API reconciliation

Quota headers only reflect traffic that flowed through shunt. `usage_refresh_seconds` closes that gap by polling `GET /api/oauth/usage` and applying authoritative utilization and reset times to the same 5-hour, shared weekly (`7d`), and Fable-scoped weekly (`7d_oi`) windows.

Polling is off when the field is absent or `0`; positive values below 60 are clamped to 60 seconds. Only imported, refreshable accounts are eligible. Long-lived `claude setup-token` and `token_env` accounts are skipped because their tokens cannot call the endpoint. The interval is fixed at boot, so a config reload does not start, stop, or re-tune the poller. This periodic correction complements rather than replaces reactive header state.

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

These startup checks prevent an OAuth bearer from being sent off-origin or over plaintext. The HTTPS and host checks are **relaxed for loopback hosts** (`localhost`, `127.0.0.1`, `[::1]`, etc.): a loopback `base_url` may use plain HTTP and any host, so a local debugging proxy or mock can receive the traffic — the bearer cannot leave the operator's machine. Non-loopback hosts are always held to HTTPS + `anthropic.com`. On a shared deployment, also configure [`[server.auth]`](/guides/shared-gateway/#inbound-client-tokens) because `claude_oauth` spends gateway-owned credentials. Clients then authenticate with the `ANTHROPIC_AUTH_TOKEN` they already send (accepted as the client token via `Authorization: Bearer`, alongside `x-shunt-token` and `x-api-key`) — on a pool-only gateway no `ANTHROPIC_CUSTOM_HEADERS` line is needed.

## Remaining follow-up

- **Storm-control:** ramping a freshly switched account's concurrency remains a later follow-up and is not implemented.

The implementation behavior was informed by [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) and the shipped Claude Code binary. shunt has no runtime dependency on teamclaude.
