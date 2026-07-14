---
title: Connect the Codex CLI
description: Point the OpenAI Codex CLI at shunt and have it load-balanced across a ChatGPT/Codex OAuth account pool.
---

Every other connection guide routes **Claude Code** to a backend. This one runs the opposite
direction: point the OpenAI **Codex CLI** at shunt and let shunt load-balance it across a pool of
ChatGPT/Codex OAuth accounts. shunt relays the Codex CLI's OpenAI Responses traffic **verbatim** —
no Anthropic translation — so the CLI talks the same wire protocol to shunt that it would talk
directly to `chatgpt.com`.

This is the [Inbound Codex Endpoint](/guides/inbound-codex-endpoint/) in practice; that page is the
behavior spec (failover table, exhaustion semantics, reload rules). The steps below are the
end-to-end connection walkthrough. Codex CLI config keys quoted here come from OpenAI's
[configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference) and
[authentication](https://learn.chatgpt.com/docs/auth) docs.

## 1. Enable the endpoint on shunt

The inbound endpoint is **opt-in**. Add the table and point it at a `chatgpt_oauth` provider (the
default provider name is `codex`):

```toml
# shunt config.toml
[server.codex_endpoint]
provider = "codex"        # must be a chatgpt_oauth provider; "codex" is the default

[[providers]]
name = "codex"
auth = "chatgpt_oauth"
```

```bash
shunt check                # validates the endpoint's provider exists + is chatgpt_oauth
shunt run
```

When `[server.codex_endpoint]` is absent, none of the routes are registered and shunt's default
HTTP surface is unchanged. When present, startup validation **rejects** an unknown `provider` or one
whose `auth` isn't `chatgpt_oauth` — the endpoint injects the operator's Codex bearer, so only a
`chatgpt_oauth` provider qualifies. See the
[configuration reference](/reference/configuration/#servercodex_endpoint-optional) for every key.

## 2. Point the Codex CLI at shunt

The Codex CLI appends `/responses` to whatever base URL it uses (Codex speaks the OpenAI Responses
wire protocol — `wire_api = "responses"` is the only value it supports), so shunt registers three
routes and any client shape below lands on one:

| Codex CLI `~/.codex/config.toml` | shunt route it hits |
| :-- | :-- |
| custom provider `base_url = ".../v1"` | `POST /v1/responses` |
| `openai_base_url = ".../v1"` | `POST /v1/responses` |
| `chatgpt_base_url = ".../backend-api/codex"` | `POST /backend-api/codex/responses` |

**Recommended — a custom model provider.** It is the **only** shape that can attach a shunt client
token (step 3), and with `requires_openai_auth = false` it is unauthenticated from the CLI's view,
so it needs **no local `codex login`** at all — shunt supplies the account:

```toml
# ~/.codex/config.toml
model_provider = "shunt"          # select it as the active provider
model = "gpt-5.6-sol"             # an entitled slug — see step 5

[model_providers.shunt]
name = "shunt"
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"            # the only supported value; also the default
requires_openai_auth = false      # shunt handles auth; the CLI needs no ChatGPT/API login here
```

**Loopback shortcuts (no `[server.auth]` only).** If shunt has no client-token gate, you can skip
the provider block and just override a base URL. Neither shape can attach a custom header, and both
keep the CLI in its own auth mode (so they still need a local `codex login` whose credential shunt
ignores):

```toml
# ~/.codex/config.toml — either one, not both
openai_base_url  = "http://127.0.0.1:3001/v1"                 # built-in openai provider
chatgpt_base_url = "http://127.0.0.1:3001/backend-api/codex"  # ChatGPT auth mode
```

Either way, the CLI's own local `~/.codex/auth.json` login is **irrelevant to which account
answers** — every request draws an account from shunt's pool. A loopback `base_url` may stay plain
`http://`; use `https://` for anything remote. Do **not** set `supports_websockets = true` on the
shunt provider — this endpoint is HTTP/SSE-only (see below).

:::tip[Isolate it in a profile]
Keep your normal Codex setup untouched by putting the shunt block in a profile file
`~/.codex/shunt.config.toml` and running `codex --profile shunt`. Profile files use the same
top-level keys and only need the values that differ from your base config.
:::

## 3. Present the shunt client token (when `[server.auth]` is set)

If shunt has [`[server.auth]`](/guides/shared-gateway/) configured — recommended for anything beyond
loopback — the CLI must present the shunt client token through the configured header (default
`x-shunt-token`). shunt authenticates on that **header**, *not* on an `Authorization: Bearer`, so
use the provider's `http_headers` / `env_http_headers` — **not** `env_key` (which Codex turns into a
Bearer that shunt neither reads nor forwards). This is the other reason the custom provider is the
only workable shape here.

Keep the secret out of `config.toml` with `env_http_headers`, which reads the value from an
environment variable:

```toml
# ~/.codex/config.toml
[model_providers.shunt]
name = "shunt"
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
requires_openai_auth = false
env_http_headers = { "x-shunt-token" = "SHUNT_TOKEN" }   # reads $SHUNT_TOKEN
```

```bash
export SHUNT_TOKEN="<token>"
```

Or hardcode it with `http_headers = { "x-shunt-token" = "<token>" }` if you accept the secret living
in the file. Without a valid token the endpoint returns `401 authentication_error`; without
`[server.auth]` at all it is open to anyone who can reach it — fine for loopback, not for a shared
gateway.

:::note[The client's own credential never leaves — but its identity headers do]
Whatever `Authorization: Bearer` the Codex CLI happens to send is **never** used as the inbound
credential and **never** forwarded upstream, and the `x-shunt-token` header is stripped so it never
leaks either. Everything else the CLI sends is forwarded **verbatim** — shunt swaps in only the pool
account's `Authorization` bearer + `chatgpt-account-id`. In particular your CLI's **own** `version`
reaches the backend (not a shunt-fixed one), so `minimal_client_version` model gating (step 5)
behaves exactly as it would against `chatgpt.com`.
:::

## 4. Provision the account pool on shunt

The endpoint reuses the [Codex Multi-Account](/guides/codex-multi-account/#configure-the-pool) pool
unchanged — provision accounts on **shunt's** host, not the CLI's:

```bash
codex login                       # sign in to a ChatGPT account (browser flow)
shunt login codex --name main     # capture it into shunt's store
```

```toml
# shunt config.toml
[[providers.codex.accounts]]
name = "main"

[[providers.codex.accounts]]
name = "backup"
```

Selection is **session-sticky**: the Codex CLI's own `session-id` request header keys the account,
so one conversation stays on one account for as long as it stays healthy, then fails over (429 →
rotate, 401 → refresh + retry, 5xx → cool down + rotate). A successful pooled response carries an
`x-shunt-account: <name>` header.

With **no** `[[providers.codex.accounts]]` configured and an empty store, the endpoint falls back to
the single default `~/.codex/auth.json` credential — no pool, no failover — so one Codex login works
the moment `[server.codex_endpoint]` is set.

:::note[Headless provisioning]
`shunt login codex` captures the **file-based** `~/.codex/auth.json`, so on the shunt host keep
`cli_auth_credentials_store = "file"` (the default) rather than `keyring`. On a server with no
browser, sign the account in with Codex's device-code flow (`codex login --device-auth`) or complete
`codex login` on a laptop and copy `~/.codex/auth.json` over to the shunt host — see
[Login on headless devices](https://learn.chatgpt.com/docs/auth#login-on-headless-devices).
:::

## 5. Pick an entitled model

:::caution[Codex-suffixed slugs are rejected]
The inbound body's `model` is forwarded upstream **verbatim** and the ChatGPT-account backend only
accepts the slugs your account is **currently entitled** to — it **rejects** `gpt-*-codex` slugs
(e.g. `gpt-5.2-codex`) with a `400`. If your Codex CLI defaults to a `-codex` model, override it to
an entitled slug, e.g. in `~/.codex/config.toml`:

```toml
model = "gpt-5.6-sol"   # entitled slug, not a *-codex one
```

Current entitled slugs are `gpt-5.6-sol` / `-terra` / `-luna` and `gpt-5.5` / `gpt-5.4` /
`gpt-5.4-mini` / `gpt-5.2` — older accounts may only have the earlier ones. See
[ChatGPT / Codex → Route a model](/guides/codex/#3-route-a-model-to-codex) for the authoritative
list. A request naming a model your subscription lacks fails exactly as it would talking to
`chatgpt.com` directly.
:::

## 6. Verify

A raw Responses request against any of the three routes should relay verbatim and (when pooled)
return an `x-shunt-account` header:

```bash
curl -N -i -X POST http://127.0.0.1:3001/v1/responses \
  -H "content-type: application/json" \
  -H "x-shunt-token: <token>" \
  -d '{"model":"gpt-5.6-sol","input":"say hi","stream":true}'
```

- A `200` with `content-type: text/event-stream` and an `x-shunt-account:` header ⇒ the pool served
  the request and the SSE is relayed unchanged.
- A `401 authentication_error` ⇒ missing/invalid `x-shunt-token` (step 3).
- A `400` naming the model ⇒ the account isn't entitled to that slug (step 5).

Then run the real Codex CLI a turn — it round-trips through the pool with no code change on the CLI
side beyond the base URL. See [Inbound Codex Endpoint](/guides/inbound-codex-endpoint/) for how this
differs from the `/v1/messages` path and [HTTP Endpoints](/reference/endpoints/) for the registered
routes.

## HTTP/SSE only

Even when the target provider has `websocket = true`, this endpoint always uses the HTTP transport,
so leave `supports_websockets` off (its default) on the Codex CLI's shunt provider. The experimental
[Codex WebSocket v2 transport](https://github.com/pleaseai/shunt/blob/main/docs/m11-inbound-codex-endpoint.md)
is out of scope for this endpoint and tracked as a follow-up.
