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
| `GET` | `/managed/settings` | Per-user Claude Code managed settings for a gateway JWT; supports `ETag`, `If-None-Match`, and `304 Not Modified` |
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
| `GET` | `/usage` | Client-facing sanitized pool usage — per-window remaining headroom and reset for the shared account pool; never account identity or capacity |

The `/managed/settings` route exists only when [`[server.gateway]`](/reference/configuration/#servergateway-optional) was enabled at boot. A valid gateway bearer JWT is required; static `[server.auth]` tokens do not authenticate this endpoint. When `[[server.gateway.policies]]` is configured, the response is:

```json
{
  "uuid": "sha256:<stable-user-hash>",
  "checksum": "sha256:<settings-hash>",
  "settings": { "availableModels": ["claude-opus-4-8"] }
}
```

`ETag` is the quoted checksum (`"sha256:<settings-hash>"`). Send it back in `If-None-Match` to receive `304 Not Modified` with an empty body when settings have not changed; comma-separated validator lists, weak validators, `*`, and legacy unquoted checksum values are accepted. No configured `policies` returns `404`; a policy that resolves to an empty document returns `200` with `settings: {}`.

The `/admin*` routes exist only when [`[server.admin]`](/reference/configuration/#serveradmin-optional) is configured; without that table, none of them are registered.

The `/backend-api/codex/responses`, `/responses`, and `/v1/responses` routes exist only when [`[server.codex_endpoint]`](/reference/configuration/#servercodex_endpoint-optional) is configured; without that table, none of them are registered. All three map to the same handler and relay a raw OpenAI Responses request/response, unlike the Anthropic-Messages-translating `/v1/messages` above — see the [inbound Codex endpoint guide](/guides/inbound-codex-endpoint/).

The `/usage` route exists only when [`[server.usage]`](/reference/configuration/#serverusage-optional) is configured, which itself requires [`[server.auth]`](/guides/shared-gateway/). It authenticates the same client token as `GET /v1/messages` (configured header, `x-api-key`, or `Authorization: Bearer`) and returns a **sanitized, aggregated** view of the shared account pool — per-window remaining headroom and reset time plus a coarse `ok`/`degraded`/`exhausted` status — so a non-admin caller can anticipate throttling. It never exposes account names, counts, priorities, `disabled` flags, thresholds, or per-account numbers; the full per-account detail stays behind admin-only `GET /admin/pool`. The Codex backend publishes no quota headers, so its windows report `null`. Response shape:

```json
{
  "pool": {
    "status": "ok",
    "windows": {
      "5h":    { "remaining": 0.42, "resets_at": 1752000000 },
      "7d":    { "remaining": 0.61, "resets_at": 1752500000 },
      "fable": { "remaining": null, "resets_at": null }
    }
  }
}
```

`GET /` and `GET /health` stay open even when [`[server.auth]`](/guides/shared-gateway/) is enabled (healthcheck tools usually cannot attach tokens) and expose nothing sensitive — only status, version, and the already-public endpoint list. With `[server.auth]` enabled, `GET /v1/models` requires a valid client token in the configured header, `x-api-key`, or `Authorization: Bearer`; it stays open when inbound auth is not configured. `GET /routes` remains open as shunt-native routing metadata.

## Gateway protocol

shunt implements the official [Claude Code LLM gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol): correct header and body-field forwarding, feature pass-through, and system-prompt attribution handling. Gateway-owned errors are returned in the Anthropic error shape, upstream context-overflow errors are rewritten to Anthropic's `prompt is too long` wording so Claude Code's [compact-and-retry](/guides/effort-and-context/#context-overflow-recovery) fires, and streaming responses are relayed without buffering (with optional [keepalive pings](/guides/shared-gateway/#sse-keepalive-pings)).
