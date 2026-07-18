---
title: Configuration Reference
description: Every shunt.toml key — server, providers, routes, models.
---

The keys below are shown in TOML, but a config file may also be written in YAML (`shunt.yaml`/`shunt.yml`) — the schema is identical, only the syntax differs. See [Configuration](/guides/configuration/) for file locations, precedence, and an annotated example. Full template: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

## `[server]`

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | Address shunt listens on |
| `default_provider` | `anthropic` | Provider for any model with no matching route |
| `sse_keepalive_seconds` | `30` | Idle seconds before an SSE `ping` is injected; `0` disables ([details](/guides/shared-gateway/#sse-keepalive-pings)) |

## `[server.auth]` (optional)

Presence of this table enables inbound client-token auth ([details](/guides/shared-gateway/)):

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | Header carrying the client token |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | Env var holding comma-separated `name:token` pairs |

The named environment variable must contain one or more credentials, for example `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`. Startup fails closed if the table is present but the variable is unset, empty, or malformed. Gated routes (mapped `/v1/messages` inference and `GET /v1/models` discovery) accept the token via the configured header, `Authorization: Bearer`, or `x-api-key` — the dedicated header wins when several carry valid tokens.

## `[server.admin]` (optional)

Presence of this table enables the admin web surface for browser account provisioning and account-pool health ([details](/guides/admin-remote-provisioning/)). When the table is absent, none of the `/admin*` routes are registered.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | Header carrying the admin token for API/curl calls |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | Env var holding comma-separated `name:token` pairs |
| `session_ttl_secs` | `3600` | Browser session lifetime after login, in seconds |
| `pending_ttl_secs` | `600` | Time allowed to finish a started provisioning flow, in seconds |

The named environment variable must contain one or more credentials, for example `SHUNT_ADMIN_TOKENS="ops:<token>"`. Startup fails closed if the table is present but the variable is unset, empty, or malformed.

Admin tokens are separate credentials from the client tokens configured under `[server.auth]`; do not reuse one credential for both surfaces.

## `[server.gateway]` (optional)

Presence of this table enables the [OAuth device-flow gateway login](/guides/gateway-login/) used by Claude Code's managed `forceLoginMethod: "gateway"`. When absent, shunt does not register `/.well-known/oauth-authorization-server`, `/oauth/device_authorization`, `/oauth/token`, `/device`, `/device/authorize`, `/device/callback`, or `/managed/settings`.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `public_url` | required | Externally reachable HTTPS origin used as the JWT issuer and base for advertised OAuth endpoints; `http` is accepted only for loopback |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | Env var holding the HS256 signing secret (at least 32 bytes) |
| `users_env` | `SHUNT_GATEWAY_USERS` | Env var holding comma-separated `email:secret` approval users; optional when `[server.gateway.oidc]` is configured |
| `token_ttl_seconds` | `3600` | Access-token lifetime; returned as `expires_in` |
| `trust_forwarded_for` | `false` | Trust `X-Forwarded-For`/`X-Real-IP` as the `/device` rate-limit identity; enable only behind a trusted proxy that replaces client-supplied values |
| `state_path` | `~/.shunt/gateway-sessions.json` | File persisting refresh sessions across restarts; tokens are stored as SHA-256 hashes and written atomically with owner-only permissions (0600 on Unix). Set `""` for memory-only sessions (also the fallback when no home directory resolves) |

Startup fails closed when the URL is not a bare HTTPS origin (`http` is allowed only on loopback), the TTL is zero, the secret is missing or shorter than 32 bytes, or neither a valid static-user list nor a valid external IdP is configured. Static-user secrets may contain `:` because only the first colon separates the email and secret. Changes to the environment-backed secrets, users, and IdP configuration hot-apply on config reload; adding or removing the gateway table requires a restart because the route tree is fixed at boot.

### `[server.gateway.oidc]` (optional)

Presence of this subtable replaces or supplements the password approval form with an OIDC provider such as Google. An allowlist is always required and is matched case-insensitively.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `issuer` | required | OIDC discovery issuer. Must use HTTPS, except HTTP on loopback; a path is allowed |
| `client_id` | required | OIDC client id |
| `client_secret_env` | `SHUNT_GATEWAY_OIDC_SECRET` | Env var holding the non-empty client secret |
| `allowed_domains` | `[]` | Case-insensitive email domains allowed to approve a device |
| `allowed_emails` | `[]` | Case-insensitive full email addresses allowed to approve a device |
| `scopes` | `openid email profile` | Scopes sent to the authorization endpoint; custom values must include `openid` and `email` |
| `authorization_endpoint` | discovery | Advanced authorization URL override; HTTPS or loopback HTTP only |
| `token_endpoint` | discovery | Advanced token URL override; HTTPS or loopback HTTP only |
| `userinfo_endpoint` | discovery | Advanced OIDC UserInfo URL override; HTTPS or loopback HTTP only |

At least one non-empty `allowed_domains` or `allowed_emails` entry is mandatory. shunt accepts only a non-empty UserInfo email with `email_verified = true`. The browser flow uses a single-use ten-minute state and PKCE, and callback/token/UserInfo failures produce generic browser messages without echoing provider input. The redirect URI registered at the provider is `{public_url}/device/callback`. For GitHub, SAML, or another non-OIDC provider, use an OIDC broker such as Dex; direct provider-specific OAuth2 integrations are out of scope.

The issued bearer gates `/v1/models` and `/v1/messages`/`/v1/messages/count_tokens` requests whenever the selected provider injects a server-side credential; passthrough providers remain open. If `[server.auth]` is also present, either credential grants access. Refresh sessions persist across restarts by default: `state_path` (tokens hashed at rest) is restored at boot, so users keep silently refreshing. The file must not be shared between concurrent shunt processes. With `state_path = ""`, sessions are memory-only — a config reload preserves them, but restarting shunt invalidates them and users sign in again once their access JWT expires. Device grants and rate-limit counters are always memory-only; a restart mid-login only costs that attempt. Expired grants and idle rate-limit identities are swept opportunistically. Device grants and rate-limit identities are each capped at 4,096 entries. Used refresh-token tombstones are retained for 30 days and capped at 64 per family; active refresh tokens idle for 30 days expire.

### `[[server.gateway.policies]]` (optional)

Presence of `[server.gateway]` registers authenticated `GET /managed/settings`; an ordered, non-empty policy list supplies its managed document. Each policy has an optional `[server.gateway.policies.match]` table and a required open-schema `[server.gateway.policies.cli]` object. `match` omitted, `match = {}`, or no `emails` means catch-all; an explicit empty `emails` list or blank entry fails startup.

All catch-all policies merge in order, then the first exact, case-sensitive email match merges on top. Objects merge recursively; arrays replace except keys containing `deny`, whose arrays union without duplicates. Known keys are validated at startup and hot reload: `availableModels`, when present, must be an array containing only strings, and `env`, when present, must be a table containing only scalar string, number, or boolean values. Unknown keys remain open-schema, but every value must be JSON-representable; non-finite floats are rejected.

No `policies` key makes the endpoint return `404`. With policies configured but no matching user-specific or catch-all settings, it returns `200` with a telemetry-only `settings.env` when telemetry is enabled, and `settings: {}` otherwise. Responses carry `uuid`, `checksum`, and a quoted `ETag` containing the checksum; matching `If-None-Match` returns `304`.

If the resolved `cli.availableModels` is an array of strings, gateway-JWT requests to `/v1/messages` and `/v1/messages/count_tokens` are rejected with `400 invalid_request_error` when their top-level `model`, after stripping one trailing Claude Code context-window hint (`[1m]` or `[1M]`), is absent from the list. Static `[server.auth]` credentials remain unrestricted because they do not identify a gateway policy user.

### `[server.gateway.telemetry]` (optional)

`forward_to` is an array of destinations with a required HTTP(S) `url` and optional string `headers` map. A non-empty list injects six values into managed `settings.env`: `CLAUDE_CODE_ENABLE_TELEMETRY=1`, the `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER`, and `OTEL_TRACES_EXPORTER` values set to `otlp`, `OTEL_EXPORTER_OTLP_ENDPOINT` set to `public_url`, and `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`. Policy env values win on conflicts. This table gates only the environment push in M-B; inbound OTLP ingest/relay is M-C (#189).

```toml
[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
headers = { "x-api-key" = "..." }
```

By default, `/device` ignores forwarding headers and rate-limits the socket peer. Set `trust_forwarded_for = true` only when shunt is reachable exclusively through a trusted reverse proxy that removes client-provided forwarding headers before setting its own value. Do not enable it on a directly exposed gateway.

## `[server.codex_endpoint]` (optional)

Presence of this table enables an inbound OpenAI Responses passthrough so the **Codex CLI** can point its `base_url` at shunt and be load-balanced across a ChatGPT/Codex OAuth account pool ([details](/guides/inbound-codex-endpoint/)). When the table is absent, none of those routes are registered.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `provider` | `codex` | Name of a `[providers.<name>]` table to serve inbound requests; must use `auth = "chatgpt_oauth"` |

Registers `POST /backend-api/codex/responses`, `POST /responses`, and `POST /v1/responses` — all served by the named provider's account pool. When `[server.auth]` is configured they require a valid client token (like the other injected-credential routes); with no `[server.auth]` they are **open** to anyone who can reach them while still injecting the operator's Codex credential, so gate them on anything beyond loopback. Unlike `/v1/messages`, the request is not translated to or from Anthropic Messages; it is relayed to and from the upstream verbatim.

## `[server.usage]` (optional)

Presence of this table registers a client-facing `GET /usage` endpoint that returns a **sanitized, aggregated** view of the shared account pool's quota state, so a non-admin client can anticipate throttling without the admin surface ([endpoint details](/reference/endpoints/)). When the table is absent, the route is not registered.

The table has no keys today — presence alone opts in. It **requires [`[server.auth]`](#serverauth-optional)**: the endpoint identifies its caller by client token, so shunt fails startup if `[server.usage]` is set without inbound auth rather than serve pool telemetry unauthenticated.

`GET /usage` authenticates the same client token as `/v1/messages` (configured header, `x-api-key`, or `Authorization: Bearer`) and reports per-window remaining headroom (`1 - min(utilization)` across non-disabled accounts, i.e. the least reported utilization among non-disabled accounts — a pool-wide aggregate, not a prediction of which account the next request will actually route to), each window's reset, and a coarse `ok`/`degraded`/`exhausted` status. It never exposes account names, counts, priorities, `disabled` flags, thresholds, or per-account numbers — the full per-account detail stays behind the admin-only [`GET /admin/pool`](#serveradmin-optional). The Codex backend publishes no quota headers, so its windows report `null`.

## `[server.pool]` (optional)

Quota-aware load-balancing tuning for the account pools — Claude (Anthropic) ([details](/guides/anthropic-multi-account/#tuning-selection-serverpool)) and, since issue #195, Codex/ChatGPT ([details](/guides/codex-multi-account/)). When the table is absent, selection uses the single built-in `0.98` threshold exactly as before this table existed.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | Safety backstop for every quota window; an account at or above it always sorts last among available accounts |
| `default_threshold` | unset | Soft default threshold for any window without a more specific value |
| `default_threshold_5h` | unset | Soft default for the 5-hour window |
| `default_threshold_7d` | unset | Soft default for the shared weekly (`7d`) window |
| `default_threshold_fable` | unset | Soft default for the fable-only weekly (`7d_oi`) window |
| `burn_rate_avoidance` | `false` | Also avoid accounts projected to exhaust a window's soft threshold before that window resets |
| `usage_refresh_seconds` | disabled (`0`/absent) | Poll interval, in seconds, for `GET /api/oauth/usage`; a positive value below 60 is clamped up to a 60-second floor |
| `state_path` | unset | File the pool's per-account quota state is persisted to, so a restart warm-starts from the last observed utilization instead of an empty pool. Absent disables persistence (the default) |
| `ramp_initial_concurrency` | disabled (`0`/absent) | Storm control: initial concurrent-admission allowance for an account identity that just started taking traffic. `0` or absent disables admission gating |

For each window `X`, the effective soft threshold resolves as: account `threshold_X` → account `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, and is capped at `hard_threshold`. All thresholds are utilization fractions in `[0.0, 1.0]`; out-of-range values fail startup. The threshold and burn-rate knobs govern both pool families: the Anthropic pool from its `anthropic-ratelimit-unified-*` headers, and the Codex/ChatGPT pool from its `x-codex-*` 5-hour/weekly windows (Codex has no Fable-scoped `7d_oi` window, so `default_threshold_fable` is inert there). `usage_refresh_seconds` is Anthropic-only — Codex has no out-of-band usage API.

A positive `usage_refresh_seconds` additionally starts a background poller that reconciles Claude account-pool quota state against the Anthropic OAuth usage API ([details](/guides/anthropic-multi-account/#usage-api-reconciliation)); absent or `0` disables it (the default). Only imported (refreshable) `claude_oauth` accounts are polled — a long-lived `claude setup-token` or `token_env` account is skipped because the usage endpoint rejects a non-refreshable token. The poller reconciles the pool's header-derived 5h/weekly/Fable (`7d_oi`) quota state with authoritative usage, including out-of-band consumption of the same account outside shunt. The interval is fixed at boot; a config reload does not start, stop, or re-tune the poller.

`state_path` persists the pool's quota state (per-window utilization and reset, across every provider's accounts) to disk. Without it, a restart begins with an empty pool: every account looks unseen until its first post-restart response, which disables burn-rate avoidance and leaves `GET /usage` blank until traffic re-populates the pool. The file is a best-effort cache, not a source of truth — quota is re-derived from upstream responses regardless, so a missing, stale, or corrupt file only costs a cold start, never a boot failure. Writes use a private (`0600` on Unix) temp file, atomically rename it over the target, and happen on a background timer only when quota changed; failed writes retry on the next tick. Cooldowns are not persisted (they lapse on restart), and any restored window whose reset has already passed is dropped lazily by the first selection or snapshot after restore. The path is fixed at boot; a config reload does not start, stop, or re-point persistence.

A positive `ramp_initial_concurrency` enables **storm control** on every account pool: after a failover switch, concurrent in-flight requests would otherwise all land on the freshly selected account at once. With the gate on, an identity that just started taking traffic (fresh, back from a cooldown, or idle for 60 seconds) admits at most the configured number of concurrent requests; each successful response doubles the allowance (slow start), a failover-worthy failure restarts the ramp, and a denied request spills to the next account in selection order. The last remaining candidate is always attempted regardless of the gate, so gating can defer but never fail a request that an ungated pool would have served. Note this also means a pool whose accounts all resolve to a single upstream identity is effectively ungated: its only candidate is always the last candidate, so the setting only takes effect with two or more distinct account identities.

## `[providers.<name>]`

Each provider is a table under a name of your choosing. Built-ins (`anthropic`, `openai`, `codex`, `xai`, `grok`, `cursor`) can be partially overridden — config maps deep-merge.

| Key | Values | Meaning |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` | Upstream protocol / adapter. `anthropic` = Messages API (passed through, optionally re-keyed); `responses` = Anthropic Messages translated to the OpenAI Responses API; `cursor` = the native Cursor ConnectRPC/protobuf AgentService adapter. |
| `base_url` | URL | Upstream base; shunt appends the endpoint path. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough` forwards the client's own credential; `api_key` injects a key from `api_key_env`; `chatgpt_oauth` reuses `~/.codex/auth.json`; `claude_oauth` selects from explicit Anthropic accounts; `xai_oauth` reuses `~/.shunt/xai-auth.json` from `shunt login xai` (only sent to x.ai/grok.com hosts over HTTPS); `cursor_oauth` reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`). |
| `api_key_env` | env var name | Where the key is read from, when `auth = "api_key"`. |
| `api_key_header` | `bearer` (default) \| `x_api_key` | Header the injected key is sent in. |
| `accounts` | array of account tables | Anthropic OAuth account pool. Valid only with `kind = "anthropic"` and `auth = "claude_oauth"`; see below. |
| `effort` | `low` … `max` | Optional default reasoning effort (`responses` providers). |
| `count_tokens` | `tiktoken` (default) \| `estimate` | `responses` and `cursor` providers: local tiktoken count vs. `501 not_supported` fallback ([details](/guides/effort-and-context/#token-counting-count_tokens)). |
| `websocket` | `true` \| `false` (default) | Opt in to the Codex Responses WebSocket v2 transport (ChatGPT/Codex backend only; falls back to HTTP on any transport failure before the first event reaches the client, so it can never do worse than plain HTTP). |
| `tool_search` | `true` \| `false` (default) | Opt in to the native client-executed `tool_search` protocol for Claude Code's tool search (stock OpenAI / ChatGPT-Codex flavors on GPT-5.4+ models; otherwise the text-based shim is kept). See [Codex → Tool search](/guides/codex/#native-protocol-opt-in). |
| `retry` | sub-table | Bounded retry/backoff for transient upstream failures. On by default (conservative); see below. |

### `[providers.<name>.retry]`

Bounded retry for **transient** upstream failures on this provider's single-credential upstream calls — the `passthrough`/`api_key` Anthropic path, the single-credential Responses path (`api_key`, `xai_oauth`/Grok, and a `chatgpt_oauth` provider with no pooled accounts), and the Cursor path. It re-issues the request (full body, before any bytes reach the client) on connection-level transport errors (connect reset/refused, timeout) for all of those paths. A transient response *status* — `429`, `502`, `503`, `504`, `529` (Anthropic's "Overloaded") — is retried **only on the Cursor path**; the Anthropic Messages and single-credential Responses calls are non-idempotent creation POSTs, so once a response arrives it is surfaced immediately rather than retried, because the upstream may already have accepted a billable generation (issue #126). It **never** retries any other `4xx` (a request error an identical retry cannot fix), and never retries once a response body has started streaming to the client.

Backoff is exponential with randomized (full) jitter, capped at `max_backoff_ms`. A server-supplied `Retry-After` takes precedence (both the delta-seconds and HTTP-date forms are honored); if it asks for longer than `max_backoff_ms`, the response is surfaced immediately rather than slept past budget. Retry is **held off `count_tokens`** regardless of this setting. The `claude_oauth` / `chatgpt_oauth` account pools drive their own account-rotation failover and are unaffected by this table.

```toml
[providers.openai.retry]
max_retries = 2          # default; 0 disables retry entirely
initial_backoff_ms = 500 # default
max_backoff_ms = 8000    # default; also caps an honored Retry-After
multiplier = 2.0         # default; exponential growth factor (>= 1.0)
```

| Key | Values | Meaning |
| :-- | :-- | :-- |
| `max_retries` | integer (default `2`, max `10`) | Extra attempts after the first. `0` disables retry. |
| `initial_backoff_ms` | milliseconds (default `500`, must be `> 0` when `max_retries > 0`) | Backoff ceiling before the first retry (jitter fills `[0, this]`), grown by `multiplier` per attempt. |
| `max_backoff_ms` | milliseconds (default `8000`, must be `> 0` when `max_retries > 0`) | Upper bound on any single backoff and on an honored `Retry-After`. |
| `multiplier` | finite number ≥ 1.0 (default `2.0`) | Exponential growth factor applied to the backoff per attempt. |

### `[[providers.<name>.accounts]]`

Explicit account entries for an Anthropic provider using `auth = "claude_oauth"`:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

[[providers.anthropic.accounts]]
name = "primary"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000"

[[providers.anthropic.accounts]]
name = "backup"
token_env = "CLAUDE_BACKUP_OAUTH_TOKEN"
```

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Unique account label containing only lowercase ASCII letters, digits, and hyphens. A name-only entry resolves from the shunt-managed store. Returned to the client in `x-shunt-account`; avoid personal information. |
| `credentials` | one usable source | Path to a Claude Code `.credentials.json`-shaped file. `~/` is expanded. shunt refreshes near expiry and atomically writes refreshed tokens back. |
| `token_env` | one usable source | Environment variable holding a setup token. Used verbatim and not refreshable. Mutually exclusive with `credentials`. |
| `uuid` | no | Replaces an existing `metadata.user_id.account_uuid` in requests selected for this account. |
| `threshold` | no | Per-account soft quota threshold in `[0.0, 1.0]`, for every window without a per-window value. A low value marks a backup account that rotates out early. |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | no | Per-window soft thresholds; each beats `threshold` for its window. See [`[server.pool]`](#serverpool-optional) for the full resolution order. |
| `priority` | no | Selection priority when the sticky account is unhealthy; lower is preferred, default `100`. Applies to Codex pools too. |
| `disabled` | no | `true` removes the account from selection entirely (kept in config and on the admin pool dashboard). Applies to Claude and Codex pools. |

A name-only entry reads `~/.shunt/accounts/claude/<name>.json`, created with `shunt login claude --name <name> --mode <mode>` (`<mode>` is one of `oauth`, `import`, or `setup-token`); the interactive CLI prompts for these three modes and recommends refreshable OAuth. `--long-lived` remains a deprecated alias for `--mode setup-token`. `SHUNT_CLAUDE_ACCOUNTS_DIR` overrides the store directory. An empty account list scans all valid store files. Refreshable OAuth/import files are updated in place when the provider rotates their refresh token, so each file must have one active owner: do not share or independently copy it across running shunt processes. Provision each process separately, or use a static setup token when appropriate. `claude_oauth` additionally requires an HTTPS `base_url` whose host is `anthropic.com` or a subdomain, preventing bearer leakage to another origin — except for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`, …), which are exempt from both checks so a local debugging proxy or mock can be used over plain HTTP.

Account selection is session-sticky and quota-aware. On every upstream response handled by the `claude_oauth` account pool, shunt records `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-7d_oi-utilization`, `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, `anthropic-ratelimit-unified-7d_oi-reset`, and `anthropic-ratelimit-unified-status`. Status `rejected`, shared 5-hour utilization at or above its threshold, or the model's governing weekly utilization at or above its threshold makes an account near quota — the threshold is the built-in `0.98` unless tuned per account (`threshold*` above) or pool-wide ([`[server.pool]`](#serverpool-optional)). Fable models use `7d_oi` when available, falling back to `7d`; all other families, including Sonnet, use shared `7d`. shunt keeps a healthy under-threshold sticky account, but rotates off a near-quota or cooled one and prefers available under-threshold accounts by `priority`, then by soonest governing-weekly reset (or, with `[server.pool]` configured, by largest burn-rate headroom — the projected time to threshold at the observed pace minus the time to reset; `burn_rate_avoidance = true` additionally treats a negative projection as near quota), then near-quota accounts (best headroom first when `[server.pool]` is set, so an all-near pool degrades to best-margin-first while accounts past `hard_threshold` still sort last), then cooled accounts. Expired quota buckets clear automatically, and every non-disabled account remains selectable. Reactive failover remains active. Storm control for freshly switched account concurrency is available via [`[server.pool]` `ramp_initial_concurrency`](#serverpool-optional) (off by default).

See [Anthropic Multi-Account](/guides/anthropic-multi-account/) for the complete selection and failover behavior. The behavior reference is [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude); shunt has no runtime dependency on it.

## `[[routes]]`

Exact-match routing entries — checked first:

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `model` | ✅ | The exact `model` id Claude Code sends |
| `provider` | ✅ | Name of a `[providers.<name>]` table |
| `upstream_model` | — | Rewrite the model id forwarded upstream |
| `effort` | — | Per-route reasoning-effort override |

## `[[route_prefixes]]`

Prefix-match routing entries — checked after exact routes:

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `prefix` | ✅ | Model-id prefix, e.g. `gpt-` |
| `provider` | ✅ | Name of a `[providers.<name>]` table |

## `[[models]]`

Entries returned by `GET /v1/models` for [model discovery](/guides/model-discovery/). Ids must begin with `claude` or `anthropic` or Claude Code ignores them.

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `id` | ✅ | Model id exposed to Claude Code |
| `display_name` | — | Label shown in the `/model` picker |

## `[sentry]` (optional)

Opt-in error reporting to your own Sentry project. Off unless `dsn` is set; independent of `[otel]`. Reports gateway-owned diagnostics only — fatal gateway startup/serve errors, panics, and `error`-level log events (`warn`/`info` as breadcrumbs, message only); request/response bodies, headers, and credentials are never sent. Metrics and tracing are each a further, separate opt-in.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `dsn` | — | Sentry project DSN. Empty disables; an invalid DSN is a startup error. |
| `environment` | — | Optional environment tag on reported events |
| `metrics` | `false` | Also send usage metrics — the gateway metric series documented in the OpenTelemetry guide (aggregates only) |
| `traces_sample_rate` | `0.0` | Also send performance traces: the per-request span becomes a Sentry transaction, head-sampled at this rate in `[0.0, 1.0]`. `0.0` sends no spans; out of range is a startup error. |
| `include_session_id` | `false` | Attach the client session id to request spans sent to Sentry |

## `[otel]` (optional)

Opt-in OpenTelemetry (OTLP/HTTP) export of traces, metrics, and logs to your own collector ([details](/guides/opentelemetry/)). Off unless `endpoint` is set; independent of Sentry.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `endpoint` | — | OTLP/HTTP base URL (e.g. `http://localhost:4318`); shunt appends `/v1/{traces,metrics,logs}`. Empty disables; a non-`http(s)` URL is a startup error. |
| `service_name` | `shunt` | `service.name` resource attribute (takes precedence over `OTEL_SERVICE_NAME`) |
| `environment` | — | Optional `deployment.environment.name` |
| `sample_ratio` | `1.0` | Head-based trace sampling in `[0.0, 1.0]`; out of range is a startup error |
| `traces` | `true` | Export the per-request `proxy_request` span |
| `metrics` | `true` | Export the gateway metric series documented in the OpenTelemetry guide |
| `logs` | `true` | Export `tracing` log events (stderr logs unaffected) |
| `include_session_id` | `false` | Attach the client session id to request spans |

## `[otel.headers]` (optional)

Extra headers on every OTLP request (e.g. a hosted-collector token). Merged under the standard `OTEL_EXPORTER_OTLP_HEADERS`.

| Key | Meaning |
| :-- | :-- |
| any | Header name → value, e.g. `authorization = "Bearer <token>"` |

## Routing precedence

Exact `[[routes]]` match → `[[route_prefixes]]` prefix match → `server.default_provider`.
