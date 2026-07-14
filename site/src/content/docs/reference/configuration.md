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

The named environment variable must contain one or more credentials, for example `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`. Startup fails closed if the table is present but the variable is unset, empty, or malformed.

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

## `[server.codex_endpoint]` (optional)

Presence of this table enables an inbound OpenAI Responses passthrough so the **Codex CLI** can point its `base_url` at shunt and be load-balanced across a ChatGPT/Codex OAuth account pool ([details](/guides/inbound-codex-endpoint/)). When the table is absent, none of those routes are registered.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `provider` | `codex` | Name of a `[providers.<name>]` table to serve inbound requests; must use `auth = "chatgpt_oauth"` |

Registers `POST /backend-api/codex/responses`, `POST /responses`, and `POST /v1/responses` — all served by the named provider's account pool. When `[server.auth]` is configured they require a valid client token (like the other injected-credential routes); with no `[server.auth]` they are **open** to anyone who can reach them while still injecting the operator's Codex credential, so gate them on anything beyond loopback. Unlike `/v1/messages`, the request is not translated to or from Anthropic Messages; it is relayed to and from the upstream verbatim.

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

A name-only entry reads `~/.shunt/accounts/claude/<name>.json`, created with `shunt login claude --name <name>`; `SHUNT_CLAUDE_ACCOUNTS_DIR` overrides that directory. An empty account list scans all valid store files. `claude_oauth` additionally requires an HTTPS `base_url` whose host is `anthropic.com` or a subdomain, preventing bearer leakage to another origin — except for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`, …), which are exempt from both checks so a local debugging proxy or mock can be used over plain HTTP.

Account selection is session-sticky and quota-aware. On every upstream response handled by the `claude_oauth` account pool, shunt records `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-7d_oi-utilization`, `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, `anthropic-ratelimit-unified-7d_oi-reset`, and `anthropic-ratelimit-unified-status`. At the fixed `0.98` switch threshold, status `rejected`, shared 5-hour utilization at or above threshold, or the model's governing weekly utilization at or above threshold makes an account near quota. Fable models use `7d_oi` when available, falling back to `7d`; all other families, including Sonnet, use shared `7d`. shunt keeps a healthy under-threshold sticky account, but rotates off a near-quota or cooled one and prefers available under-threshold accounts by soonest governing-weekly reset, then near-quota accounts, then cooled accounts. Expired quota buckets clear automatically, and every account remains selectable. Reactive failover remains active. Storm-control for freshly switched account concurrency is not implemented.

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
| `metrics` | `false` | Also send usage metrics — the `shunt.requests` / `shunt.latency` series (aggregates only) |
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
| `metrics` | `true` | Export the `shunt.requests` / `shunt.latency` series |
| `logs` | `true` | Export `tracing` log events (stderr logs unaffected) |
| `include_session_id` | `false` | Attach the client session id to request spans |

## `[otel.headers]` (optional)

Extra headers on every OTLP request (e.g. a hosted-collector token). Merged under the standard `OTEL_EXPORTER_OTLP_HEADERS`.

| Key | Meaning |
| :-- | :-- |
| any | Header name → value, e.g. `authorization = "Bearer <token>"` |

## Routing precedence

Exact `[[routes]]` match → `[[route_prefixes]]` prefix match → `server.default_provider`.
