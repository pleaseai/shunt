# M8 — Anthropic multi-account + load balancing

M8 adds an account pool to an Anthropic provider authenticated with Claude subscription OAuth. It combines session-sticky, quota-aware proactive selection with reactive failover: shunt chooses an account, injects that account's OAuth bearer, rotates before a model-relevant quota is exhausted when possible, and can still retry another account after an upstream failure before relaying a response to Claude Code.

The behavior is based on [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) and observations of the shipped Claude Code binary. shunt ports the relevant behavior into Rust; it does not depend on teamclaude at runtime.

## Configuration

Set `auth = "claude_oauth"` on an Anthropic provider and configure one or more `[[providers.<name>.accounts]]` entries:

```toml
# Store-managed refreshable account: imports the current Claude Code login.
shunt login claude --name main

# Store-managed one-year token: runs the interactive Claude setup-token flow.
shunt login claude --name ci --long-lived

[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

# Store-managed account from ~/.shunt/accounts/claude/main.json.
[[providers.anthropic.accounts]]
name = "main"

# Existing Claude Code credentials file. `~/` is expanded while loading config.
[[providers.anthropic.accounts]]
name = "backup"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000" # optional

# Long-lived value produced by `claude setup-token`.
[[providers.anthropic.accounts]]
name = "ci"
token_env = "CLAUDE_CI_OAUTH_TOKEN"
uuid = "11111111-1111-1111-1111-111111111111" # optional
```

Then set the token environment variable before starting shunt:

```bash
export CLAUDE_CI_OAUTH_TOKEN='<value from claude setup-token>'
shunt check
shunt run
```

Each account has these fields:

| Field | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Stable account label. Must contain only lowercase ASCII letters, digits, and hyphens. Names must be unique within the provider. A name-only entry resolves from the shunt account store. |
| `credentials` | no | Path to a Claude Code `.credentials.json`-shaped file. shunt reads `claudeAiOauth`, refreshes near expiry, and writes refreshed tokens back atomically. |
| `token_env` | no | Environment variable containing a setup token. The value is used verbatim and is not refreshed. |
| `uuid` | no | Anthropic account UUID used for the request-body rewrite described below. |

`credentials` and `token_env` are mutually exclusive. A name-only account reads `~/.shunt/accounts/claude/<name>.json` (override the directory with `SHUNT_CLAUDE_ACCOUNTS_DIR`). With an entirely empty `accounts` list, shunt scans that directory and uses every valid `*.json` account in filename order. Store files are written atomically at `0600`, and the store directory is `0700` on Unix.

`shunt login claude --name <name>` imports the current refreshable Claude Code credential from `~/.claude/.credentials.json` (or `CLAUDE_CREDENTIALS`) into that store without modifying the source. `--long-lived` launches the installed `claude setup-token` command on the terminal, then asks you to paste its generated token into a hidden prompt. Claude's command does not save the token itself; shunt stores it without printing it. Reusing a name replaces that store file.

The built-in `anthropic` provider remains `auth = "passthrough"` by default. Multi-account behavior is opt-in.

## Validation and security guards

Configuration validation rejects:

- `accounts` on a provider whose auth mode is not `claude_oauth`;
- `claude_oauth` on a provider whose `kind` is not `anthropic`;
- a non-HTTPS `base_url`;
- a host other than `anthropic.com` or one of its subdomains;
- duplicate or invalid account names; and
- an account that sets both `credentials` and `token_env`.

The HTTPS and host checks are bearer-leak guards: a Claude subscription OAuth token is never injected toward an arbitrary gateway or over plaintext.

Because `claude_oauth` is an injected-credential mode, a configured `[server.auth]` also protects it on a shared shunt gateway. Configure inbound client tokens before exposing the gateway beyond loopback.

## Selection, quota state, and cooldowns

Selection state is per provider and survives config hot reloads for the life of the shunt process. Quota state is tracked per configured account.

- If the request includes `x-claude-code-session-id`, shunt hashes it to choose the sticky account. A healthy sticky account that is available and under the switch threshold stays first, preserving Phase 1 session stickiness.
- Without that header, shunt uses an independent round-robin counter for each provider.
- On every upstream response handled by the `claude_oauth` account pool, shunt parses the following headers when present:
  - utilization: `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, and `anthropic-ratelimit-unified-7d_oi-utilization` as floating-point values;
  - reset: `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, and `anthropic-ratelimit-unified-7d_oi-reset` as Unix seconds; and
  - status: `anthropic-ratelimit-unified-status`.
- `SWITCH_THRESHOLD` is `0.98`. An account is near quota when unified status is exactly `rejected`, its shared 5-hour utilization is at least `0.98`, or its governing weekly utilization is at least `0.98`.
- The 5-hour bucket applies to every request. Weekly governance is model-aware: model ids containing `fable` (case-insensitive) use `7d_oi` when that utilization is available, falling back to shared `7d`; every other model family uses shared `7d`. There is no Sonnet-specific header today, so Sonnet uses `7d`.
- shunt keeps the sticky account until it is near quota or in cooldown. It then proactively rotates before the quota wall when possible. Available under-threshold accounts come first, ordered by the soonest-resetting governing weekly bucket so use-or-lose quota is spent first; an unknown weekly reset sorts before a known reset. Available near-quota accounts follow in normal sticky/round-robin rotation order, then cooled accounts in soonest-cooldown-expiry order.
- Selection never fails closed on local quota or cooldown state: every configured account remains in the attempt order.
- When a quota bucket's reset timestamp has passed, shunt clears that bucket's utilization and reset automatically. Expiring any bucket also clears the cached unified status.
- A successful response clears that account's cooldown.

Credential-resolution failures cool an account for five minutes. Transport failures and upstream 5xx responses cool it for 30 seconds.

## Failover behavior

shunt classifies the upstream response before streaming its body. It never retries a response after streaming has begun.

| Upstream result | Action |
| :-- | :-- |
| 2xx | Relay immediately and mark the selected account healthy. |
| 429 with any `anthropic-ratelimit-unified-5h-status`, `-7d-status`, or `-7d_oi-status` equal to `rejected` | Treat as quota exhaustion, cool the account for numeric `retry-after` (default 60 seconds, clamped to 1–3600 seconds), and rotate. |
| Plain 429 without a rejected quota status | Treat as transient throttling, sleep for numeric `retry-after` (default 1 second, capped at 300 seconds), retry the same account once, then relay that retry response without rotating. |
| 401 from a `credentials` account | Force-refresh the credentials file even if its token has not expired, retry the same account once, then cool it for five minutes and rotate if it is still 401. |
| 401 from a `token_env` or store-managed setup-token account | It cannot be refreshed; cool it for five minutes and rotate. |
| 5xx | Cool the account for 30 seconds and rotate. |
| Other status | Relay immediately. |

When attempts are exhausted after receiving upstream responses, shunt relays the **last upstream response body and status** rather than replacing it with a generic gateway error. If every account fails before any upstream response exists (for example, all credentials fail to resolve), shunt returns a gateway-owned upstream error.

The `POST /v1/messages/count_tokens` Anthropic path uses the same account injection and failover behavior.

## OAuth request shaping

For the selected account, shunt:

1. strips the client's `authorization` and `x-api-key` headers;
2. sets `Authorization: Bearer <selected token>`;
3. ensures `anthropic-beta` includes `oauth-2025-04-20`, appending it without duplicating an existing value; and
4. preserves other end-to-end headers, including `anthropic-version` and `x-claude-code-session-id`.

A pooled upstream response includes:

```http
x-shunt-account: backup
```

This is useful for observing selection and failover. On a shared gateway it also exposes configured account names to clients, so use neutral labels such as `primary`, `backup-1`, or `pool-a`, not names or email addresses. The final last-upstream-response relay after pool exhaustion does not include `x-shunt-account`.

## `account_uuid` rewrite

Claude Code may send `metadata.user_id` as a string containing JSON with an `account_uuid` field. When the selected account has `uuid` configured, shunt parses both JSON layers and replaces the existing inner `account_uuid` with the selected account's UUID.

The rewrite is deliberately narrow: it only occurs when the outer request is JSON, `metadata.user_id` is a JSON string, that inner value is an object, an `account_uuid` field already exists, and the selected account has a UUID. shunt does not inject missing metadata or infer a UUID for `token_env` accounts.

## Proactive rotation and reactive failover

Phase 2 quota-aware proactive rotation is implemented. It is model-aware and switches away from a near-quota sticky account before rejection when another account is available under threshold. Phase 1 reactive failover remains the floor: rejected quota responses, authentication failures, transport failures, and 5xx responses still trigger the retry/cooldown behavior above, and every account remains selectable when all choices are near quota or cooled.

Storm-control—ramping concurrency after switching to a fresh account—remains a later follow-up and is not implemented.

## Account store

Phase 3 store-managed accounts and `shunt login claude` are implemented. Each login writes one Claude Code-compatible JSON file under `~/.shunt/accounts/claude/`. Name-only entries select those files explicitly; an empty configured account list scans the directory. Imported logins retain their refresh token and are refreshed through the existing `ClaudeAuthStore`; setup-token accounts are static until their one-year token expires or receives a 401.
