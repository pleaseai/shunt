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
| `count_tokens` | `tiktoken` (default) \| `estimate` | `responses` providers only: local tiktoken count vs. 404 fallback ([details](/guides/effort-and-context/#token-counting-count_tokens)). |
| `websocket` | `true` \| `false` (default) | Opt in to the Codex Responses WebSocket v2 transport (ChatGPT/Codex backend only; falls back to HTTP if the socket can't be opened). |
| `tool_search` | `true` \| `false` (default) | Opt in to the native client-executed `tool_search` protocol for Claude Code's tool search (stock OpenAI / ChatGPT-Codex flavors on GPT-5.4+ models; otherwise the text-based shim is kept). See [Codex → Tool search](/guides/codex/#native-protocol-opt-in). |

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
