---
title: Inbound Codex Endpoint
description: Point the OpenAI Codex CLI itself at shunt and load-balance it across a ChatGPT/Codex OAuth account pool.
---

Every other guide on this site routes **Claude Code** to another backend. shunt can also run the opposite direction: an opt-in raw OpenAI Responses passthrough that lets the **Codex CLI** point its own `base_url` at shunt and be load-balanced across a ChatGPT/Codex OAuth account pool. It is opt-in: when `[server.codex_endpoint]` is absent, none of those routes are registered and shunt's default HTTP surface is unchanged.

This builds on the same account pool as [Codex Multi-Account](/guides/codex-multi-account/) — selection, cooldowns, and refresh are shared unchanged. See the [M11 behavior specification](https://github.com/pleaseai/shunt/blob/main/docs/m11-inbound-codex-endpoint.md) for the full spec, including the exact failover table and reload semantics.

For the end-to-end setup walkthrough — enabling the endpoint, pointing the Codex CLI at shunt, client auth, account provisioning, and picking an entitled model — follow [Connect the Codex CLI](/guides/connect-codex-cli/). This page focuses on *what the endpoint does*; that guide is the *how to connect* checklist.

## Enable the endpoint

```toml
[server.codex_endpoint]   # all keys optional; default shown
provider = "codex"        # must be a chatgpt_oauth provider
```

```bash
shunt check
shunt run
```

Startup validation rejects an unknown `provider` or one that doesn't use `auth = "chatgpt_oauth"` — the endpoint injects the operator's Codex bearer, so only a `chatgpt_oauth` provider qualifies. See the [configuration reference](/reference/configuration/#servercodex_endpoint-optional) for every key and default, and [HTTP Endpoints](/reference/endpoints/) for the registered routes.

The opt-in also makes Codex CLI model discovery parseable. `GET /models` and `GET /backend-api/codex/models` return the valid fallback `{"models":[]}`. On the shared `GET /v1/models` path, a `client_version` query field selects that Codex shape even when Anthropic-looking headers are present; without it, the existing Anthropic discovery response is unchanged. These requests pass the normal model-discovery auth gate, and shunt deliberately avoids fabricating incomplete Codex model rows.

## Client analytics sink

The Codex CLI also posts product analytics to the base URL. shunt accepts both paths the CLI can produce:

- `POST /backend-api/codex/analytics-events/events`
- `POST /codex/analytics-events/events`

These routes use the same `[server.auth]` policy as the Responses routes but never forward telemetry upstream, because choosing one pooled account would misattribute the client event to that account. They always return `200 {}` after authentication, including for malformed, unreadable, or oversized bodies.

The payload and event properties are neither logged nor exported. shunt records only the sanitized `event_type` as the `event` attribute on the opt-in `shunt.codex_client_events` counter: names may contain lowercase ASCII letters, digits, `.`, `_`, and `-`, up to 64 bytes; invalid names become `other`, and unrecognized batches become `unparsed`. Without Sentry or OpenTelemetry metrics enabled, this is a pure discard sink.

## Point the Codex CLI at shunt

The Codex CLI always appends `/responses` to whatever base URL it uses, so either `~/.codex/config.toml` shape works:

**Mirror the ChatGPT backend's base URL:**

```toml
chatgpt_base_url = "http://127.0.0.1:3001/backend-api/codex"
```

**Or a custom model provider** (the top-level `model_provider` must select it, or the CLI keeps its built-in provider):

```toml
model_provider = "shunt"

[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
```

With the custom provider (add `requires_openai_auth = false` so the CLI needs no local login), the Codex CLI's own `~/.codex/auth.json` becomes irrelevant once pointed at shunt — the account comes from shunt's pool on every request. The `chatgpt_base_url` shape instead keeps the CLI in ChatGPT-login mode, so it still needs its local login and works only against an **ungated** endpoint: its ChatGPT bearer is not the configured shunt token, so `[server.auth]` rejects it.

## Client authentication

If shunt has [`[server.auth]`](/guides/shared-gateway/) configured — recommended for anything beyond loopback — present the client token **either** as an OpenAI-style Bearer key (`OPENAI_API_KEY` / a custom provider's `env_key`, the LiteLLM/llmgateway idiom) **or** as the `x-shunt-token` header:

```toml
# A. Bearer — built-in openai provider. Set the base URL in ~/.codex/config.toml,
#    NOT via the OPENAI_BASE_URL env var: the env var leaves the CLI's Responses
#    WebSocket pointed at wss://api.openai.com, so it bypasses shunt. See
#    "Point the Codex CLI at shunt" in the connect guide.
openai_base_url = "http://127.0.0.1:3001/v1"
```

```bash
export OPENAI_API_KEY="<shunt-token>"      # sent as Authorization: Bearer
```

```toml
# B. Header — a custom provider carries it (use env_http_headers to keep it out of the file):
[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
http_headers = { "x-shunt-token" = "<token>" }
```

Without `[server.auth]`, the endpoint is open to anyone who can reach it — acceptable for loopback or personal use, not for a shared gateway. The client's presented credential is used **only** to authenticate to shunt: it (and any `Authorization` the CLI happens to send) is stripped and never forwarded upstream. The `[server.admin]` credential header — `x-shunt-admin-token` by default, or whatever `[server.admin] header` names — is stripped too, since the admin surface authenticates on that slot and an admin credential can provision upstream accounts. So is the whole `cookie` header, because the admin surface also accepts a write-tier session cookie there; shunt keeps no cookie jar, so nothing upstream depends on it. `x-api-key` is stripped unconditionally too — even when `[server.auth]` is not configured — since the target provider is validated `chatgpt_oauth`-only at boot, so no inbound `x-api-key` value can ever be a valid upstream credential; a client whose `apiKeyHelper` sets both `Authorization` and `x-api-key` to the same key (as Claude Code's does) does not leak that key through the second slot. Because the inbound client is a real Codex CLI, the passthrough forwards its request headers verbatim (`version`, `originator`, `OpenAI-Beta`, `x-codex-*`, …) and swaps in **only** the selected pool account's `Authorization` bearer + `chatgpt-account-id`. See [Connect the Codex CLI](/guides/connect-codex-cli/#3-present-the-shunt-client-token-when-serverauth-is-set) for the full auth walkthrough.

## Account provisioning

Reuses the same pool as [Codex Multi-Account](/guides/codex-multi-account/#configure-the-pool):

```bash
codex login
shunt login codex --name main
```

```toml
[[providers.codex.accounts]]
name = "main"
```

With no `[[providers.codex.accounts]]` configured **and an empty shunt account store**, the endpoint falls back to the single default `~/.codex/auth.json` credential — no pooling, no failover — so a single Codex login works the moment `[server.codex_endpoint]` is set. (The handler first scans the account store and pools any accounts it discovers, so imported store accounts still enable pooling.)

## Route models to other upstreams

By default every request goes to the one provider named in `[server.codex_endpoint]`. An optional `[[server.codex_endpoint.routes]]` table lets the Codex CLI pick a **different** Responses-compatible upstream by model id; every model with no route keeps the fixed-provider behavior.

Several vendors document a native Responses endpoint for the Codex CLI: Z.ai GLM (`https://api.z.ai/api/v1`), DeepSeek (`https://api.deepseek.com`), Kimi Code (`https://api.kimi.com/coding/v1`), MiniMax (`https://api.minimax.io/v1`), Mimo (`https://api.xiaomimimo.com/v1`), OpenRouter (`https://openrouter.ai/api/v1`), Vercel AI Gateway (`https://ai-gateway.vercel.sh/codex/v1`), and stock OpenAI. shunt appends `/responses` to a provider's `base_url`, so give it the same base URL the vendor documents for Codex. The upstream must implement the Responses API natively — there is no Responses → Chat Completions adapter.

```toml
[providers.glm]
kind = "responses"
auth = "api_key"
api_key_env = "GLM_API_KEY"
base_url = "https://api.z.ai/api/v1"

[providers.deepseek]
kind = "responses"
auth = "api_key"
api_key_env = "DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"

[server.codex_endpoint]
provider = "codex"

[[server.codex_endpoint.routes]]
model = "glm-5.3"
provider = "glm"

[[server.codex_endpoint.routes]]
model = "deepseek-v4-flash"
provider = "deepseek"
```

`upstream_model` is optional and defaults to `model`; set it when the id the CLI types differs from the id the vendor serves. The routed provider must carry a real credential — a credential-free auth mode (`passthrough` or `none`) is rejected at boot, since the client's own `Authorization` is always stripped. shunt's built-in `kimi` preset is `kind = "anthropic"`, so a Codex route to Kimi Code needs a separate `kind = "responses"` provider — routing a Codex model at the Anthropic-kind preset is rejected at boot.

On the CLI side, point Codex at **shunt** and select the route by `model`:

```toml
# ~/.codex/config.toml
model = "glm-5.3"
model_provider = "shunt"
model_catalog_json = "~/.codex/models.json"

[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
env_key = "SHUNT_TOKEN"
```

shunt serves no Codex model catalog — its `GET /v1/models` discovery list is Anthropic-shaped and does not advertise Codex routes. The CLI gets slug metadata from a `~/.codex/models.json` catalog referenced by `model_catalog_json`, exactly as these vendors document; only the `model` value selects the shunt route.

What changes for a routed request to a **non-ChatGPT** upstream:

- **Header allowlist.** Only `content-type` and `accept` are taken from the client, plus the resolved credential and whatever identity the routed upstream itself requires — `OpenAI-Beta: responses=experimental` (skipped for xAI/Grok), and the Grok-CLI identity headers for an `xai_oauth` route. No `authorization`, `x-api-key`, `chatgpt-account-id`, `originator`, `version`, `user-agent`, `session-id`, `x-codex-*`, or `x-shunt-*` reaches a third party.
- **Body `model` rewrite.** When `upstream_model` differs from the requested model, shunt rewrites the top-level `model` and leaves every other field intact. A body that is not a JSON object is rejected with a `400` rather than sent on.
- **Identity encoding.** A zstd request body is decoded first — a stock Responses API does not accept that encoding — and `content-encoding` is not forwarded.
- **One credential, no failover.** There is no pool behind a routed third party, so a 429 or 5xx relays verbatim with its `retry-after` instead of triggering rotation.

Matching is exact and case-sensitive with no charset restriction, so vendor slugs like `MiniMax-M3`, `openai/gpt-5.6-sol`, and `~openai/gpt-latest` route as written. A route to another `chatgpt_oauth` provider instead keeps the full pool passthrough. Routes are read from the live config, so they take effect on reload.

## What's different from `/v1/messages`

- **No translation.** The inbound Responses body is forwarded upstream byte-for-byte, and the upstream response — SSE or JSON, success or error — is relayed back verbatim (status and `content-type` preserved). There is no Anthropic Messages ⇄ Responses translation step at all.
- **Compressed request bodies pass through.** Current Codex releases zstd-compress the request body when they talk to the ChatGPT backend, which includes the `chatgpt_base_url` shape pointed at this endpoint. The bytes and their `content-encoding: zstd` header are forwarded unchanged; shunt additionally decodes a copy in-memory only to read the request's `model` for its metrics, logs, and spans. A body shunt cannot decode still relays fine — only the `model` label degrades to `unknown`, with a warning naming the reason.
- **Model-based routing is opt-in.** By default every request goes to the one provider named in `[server.codex_endpoint]` and the body's `model` forwards through as-is. With `[[server.codex_endpoint.routes]]`, an exactly matching `model` selects that entry's provider instead — see [Route models to other upstreams](#route-models-to-other-upstreams).
- **Exhaustion relays verbatim.** If every pooled account is tried and at least one upstream response came back, shunt relays that last response unchanged rather than re-shaping it into an Anthropic-style error, since a Responses client expects the raw shape it would have gotten from the real ChatGPT backend.
- **Gateway-owned errors are OpenAI-shaped.** When the failure is shunt's own — a bad or missing client token (`401`), an unresolvable pool with no upstream response (`502`), an oversized request body, or an unconfigured endpoint — shunt returns it in the OpenAI Responses error shape (`{"error":{"message":…,"type":…,"code":null}}`) with the same status code, so the Codex CLI parses it through its own error path instead of the Anthropic `{"type":"error",…}` envelope. Relayed *upstream* errors (429/4xx/5xx from the backend) still pass through verbatim.
- **HTTP/SSE only.** Even when the target provider has `websocket = true`, this endpoint always uses the HTTP transport.

## Security

- Gate this endpoint with `[server.auth]` on anything beyond loopback — the provider injects a real Codex bearer on every request.
- Nothing about the client's own credential reaches the Codex backend; the passthrough forwards the Codex CLI's own request headers verbatim and swaps in only the selected pool account's bearer + `chatgpt-account-id` (the shunt client-token header, the `[server.admin]` credential header, the whole `cookie` header, the internal `x-shunt-inbound-client` label, the client's `Authorization`/`chatgpt-account-id`, and `x-api-key` are all stripped, never forwarded).
- Only the endpoint's **HTTP route registration** is decided once at boot: toggling `[server.codex_endpoint]` on or off at runtime logs a warning that a restart is required to add or drop those paths. Everything the table *contains* hot-reloads — the target `provider` and the whole `[[server.codex_endpoint.routes]]` model table are read from the live config on every request, so adding, editing, or removing a route takes effect on reload.
