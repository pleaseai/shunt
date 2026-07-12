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
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough` forwards the client's own credential; `api_key` injects a key from `api_key_env`; `chatgpt_oauth` reuses `~/.codex/auth.json`; `xai_oauth` reuses `~/.shunt/xai-auth.json` from `shunt login xai` (only sent to x.ai/grok.com hosts over HTTPS); `cursor_oauth` reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`). |
| `api_key_env` | env var name | Where the key is read from, when `auth = "api_key"`. |
| `api_key_header` | `bearer` (default) \| `x_api_key` | Header the injected key is sent in. |
| `effort` | `low` … `max` | Optional default reasoning effort (`responses` providers). |
| `count_tokens` | `tiktoken` (default) \| `estimate` | `responses` providers only: local tiktoken count vs. 404 fallback ([details](/guides/effort-and-context/#token-counting-count_tokens)). |

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
