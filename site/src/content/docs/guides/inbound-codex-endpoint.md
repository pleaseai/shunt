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

Startup validation rejects an unknown `provider` or one that doesn't use `auth = "chatgpt_oauth"` — the endpoint injects the operator's Codex bearer, so only a `chatgpt_oauth` provider qualifies. See the [configuration reference](/reference/configuration/#servercodex_endpoint-optional) for every key and default, and [HTTP Endpoints](/reference/endpoints/) for the three registered routes.

## Point the Codex CLI at shunt

The Codex CLI always appends `/responses` to whatever base URL it uses, so either `~/.codex/config.toml` shape works:

**Mirror the ChatGPT backend's base URL:**

```toml
chatgpt_base_url = "http://127.0.0.1:3001/backend-api/codex"
```

**Or a custom model provider:**

```toml
[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
```

Either way, the Codex CLI's own local `~/.codex/auth.json` login becomes irrelevant once pointed at shunt — the account comes from shunt's pool on every request, not from the CLI.

## Client authentication

If shunt has [`[server.auth]`](/guides/shared-gateway/) configured — recommended for anything beyond loopback — add the client token as a header the CLI sends. On the custom-provider form:

```toml
[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
http_headers = { "x-shunt-token" = "<token>" }
```

Without `[server.auth]`, the endpoint is open to anyone who can reach it — acceptable for loopback or personal use, not for a shared gateway. The client's own `Authorization` header (whatever the Codex CLI happens to send) is never forwarded upstream, and the shunt client-token header is stripped so it never leaks either. Because the inbound client is a real Codex CLI, the passthrough forwards its request headers verbatim (`version`, `originator`, `OpenAI-Beta`, `x-codex-*`, …) and swaps in **only** the selected pool account's `Authorization` bearer + `chatgpt-account-id`.

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

With no `[[providers.codex.accounts]]` configured, the endpoint falls back to the single default `~/.codex/auth.json` credential — no pooling, no failover — so a single Codex login works the moment `[server.codex_endpoint]` is set.

## What's different from `/v1/messages`

- **No translation.** The inbound Responses body is forwarded upstream byte-for-byte, and the upstream response — SSE or JSON, success or error — is relayed back verbatim (status and `content-type` preserved). There is no Anthropic Messages ⇄ Responses translation step at all.
- **No model-based routing.** Every request goes to the one provider named in `[server.codex_endpoint]`; the body's `model` field forwards through as-is and never selects a provider.
- **Exhaustion relays verbatim.** If every pooled account is tried and at least one upstream response came back, shunt relays that last response unchanged rather than re-shaping it into an Anthropic-style error, since a Responses client expects the raw shape it would have gotten from the real ChatGPT backend.
- **HTTP/SSE only.** Even when the target provider has `websocket = true`, this endpoint always uses the HTTP transport.

## Security

- Gate this endpoint with `[server.auth]` on anything beyond loopback — the provider injects a real Codex bearer on every request.
- Nothing about the client's own credential reaches the Codex backend; the passthrough forwards the Codex CLI's own request headers verbatim and swaps in only the selected pool account's bearer + `chatgpt-account-id` (the shunt client-token header is stripped, never forwarded).
- The route set is decided once at boot. Toggling `[server.codex_endpoint]` on or off at runtime logs a warning that a restart is required; a reload can still change which provider it targets.
