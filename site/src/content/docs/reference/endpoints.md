---
title: HTTP Endpoints
description: The endpoints shunt serves as a Claude Code LLM gateway.
---

| Method | Path | Purpose |
| :-- | :-- | :-- |
| `HEAD` | `/` | Liveness probe |
| `GET` | `/` | Human-readable landing (version + endpoint list) |
| `GET` | `/health` | Healthcheck — `{"status":"ok","version":"x.y.z"}` |
| `GET` | `/v1/models` | [Model discovery](/guides/model-discovery/) — returns your `[[models]]` entries |
| `GET` | `/routes` | shunt-native route discovery — returns the configured `[[routes]]` table verbatim (model → provider/upstream_model/effort mapping, including claude-prefixed discovery aliases); distinct from `/v1/models`, which serves the narrower Anthropic-protocol discovery response (`id`/`display_name` only) |
| `POST` | `/v1/messages` | Inference — routed per the request's `model` id |
| `POST` | `/v1/messages/count_tokens` | [Token counting](/guides/effort-and-context/#token-counting-count_tokens) |
| `GET` | `/admin` | Admin dashboard (HTML); redirects to `/admin/login` when not signed in |
| `GET`, `POST` | `/admin/login` | Admin-token login form and browser-session creation |
| `POST` | `/admin/logout` | Clear the browser session |
| `GET` | `/admin/accounts` | Claude account-store metadata: name, kind, expiry, and UUID; never token material |
| `GET` | `/admin/accounts/codex` | Codex account-store metadata: name, expiry, and ChatGPT account ID; never token material |
| `GET` | `/admin/pool` | Per-`claude_oauth`/`chatgpt_oauth`-provider pool state; Codex rows include reported 5h/7d usage (`7d_oi` has no Codex analog) |
| `POST` | `/admin/accounts/claude` | Start Claude browser provisioning with `{name, mode}` where `mode` is `oauth` or `setup_token` (omitted defaults to `setup_token`); returns `{authorize_url}` |
| `POST` | `/admin/accounts/claude/{name}/complete` | Complete Claude provisioning with `{code}` containing `<code>#<state>`; stores the account and reports whether it is live |
| `DELETE` | `/admin/accounts/claude/{name}` | Remove the named Claude account's store file |
| `POST` | `/admin/accounts/codex` | Start ChatGPT OAuth with `{name}`; returns `{authorize_url}` |
| `POST` | `/admin/accounts/codex/{name}/complete` | Complete Codex provisioning with `{code}` containing the full localhost redirect URL or `<code>#<state>`; stores the account and reports whether it is live |
| `DELETE` | `/admin/accounts/codex/{name}` | Remove the named Codex account's store file |
| `POST` | `/backend-api/codex/responses` | Inbound Codex CLI passthrough — mirrors the real ChatGPT backend path |
| `POST` | `/responses` | Inbound Codex CLI passthrough — bare `base_url` form |
| `POST` | `/v1/responses` | Inbound Codex CLI passthrough — `/v1`-suffixed `base_url` form |

The `/admin*` routes exist only when [`[server.admin]`](/reference/configuration/#serveradmin-optional) is configured; without that table, none of them are registered.

The `/backend-api/codex/responses`, `/responses`, and `/v1/responses` routes exist only when [`[server.codex_endpoint]`](/reference/configuration/#servercodex_endpoint-optional) is configured; without that table, none of them are registered. All three map to the same handler and relay a raw OpenAI Responses request/response, unlike the Anthropic-Messages-translating `/v1/messages` above — see the [inbound Codex endpoint guide](/guides/inbound-codex-endpoint/).

`GET /` and `GET /health` stay open even when [`[server.auth]`](/guides/shared-gateway/) is enabled (healthcheck tools usually cannot attach tokens) and expose nothing sensitive — only status, version, and the already-public endpoint list. With `[server.auth]` enabled, `GET /v1/models` requires a valid client token in the configured header, `x-api-key`, or `Authorization: Bearer`; it stays open when inbound auth is not configured. `GET /routes` remains open as shunt-native routing metadata.

## Gateway protocol

shunt implements the official [Claude Code LLM gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol): correct header and body-field forwarding, feature pass-through, and system-prompt attribution handling. Gateway-owned errors are returned in the Anthropic error shape, upstream context-overflow errors are rewritten to Anthropic's `prompt is too long` wording so Claude Code's [compact-and-retry](/guides/effort-and-context/#context-overflow-recovery) fires, and streaming responses are relayed without buffering (with optional [keepalive pings](/guides/shared-gateway/#sse-keepalive-pings)).
