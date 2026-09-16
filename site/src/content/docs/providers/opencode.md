---
title: OpenCode Zen
description: Route mapped models to OpenCode Zen's Anthropic-compatible endpoint with an OPENCODE_API_KEY.
---

**OpenCode Zen** is the OpenCode team's curated catalog — tested models from Anthropic, OpenAI, Google, xAI, Z.ai and others behind one endpoint, billed pay-as-you-go against a Zen API key. Zen speaks the **Anthropic Messages** wire natively (including `stream: true` SSE), so shunt forwards Claude Code's Messages request as-is and injects the Zen key. The `opencode` preset is built in, so configuration is one upstream entry plus routes.

## Quick start

Let a coding agent wire it up for you — `shunt add` prints an embedded setup blueprint (offline and read-only; the agent edits the config, the command never does):

```bash
shunt add upstream opencode --print | claude
```

Or follow the manual steps below.

## Configure the upstream

The `opencode` preset supplies `kind = "anthropic"`, `base_url = "https://opencode.ai/zen"`, API-key auth from `OPENCODE_API_KEY`, and the `x-api-key` header the zen endpoint reads:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "opencode"
provider = "opencode"

[[routes]]
model = "claude-fable-5-1-via-zen"
provider = "opencode"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config that routes to `opencode` must also declare the `anthropic` default it still points at (`server.default_provider` defaults to `anthropic`); drop the `anthropic` entry only if you also set `default_provider` to a declared upstream.

The legacy `[providers.opencode]` table form remains supported, but the preset does not fill it — a legacy table must spell `kind`, `base_url`, `auth = "api_key"`, `api_key_env`, and `api_key_header = "x_api_key"` itself. Do not mix `[[upstreams]]` and `[providers.*]` in one file.

## Credentials

Create an API key in the [opencode console](https://opencode.ai/console) (or reuse the key an `opencode` CLI login stored at `~/.local/share/opencode/auth.json`), then export it in the environment that launches shunt:

```bash
export OPENCODE_API_KEY='...'
```

Never write the key into the config. `shunt check` validates the config's structure but does not read the key's value — if `OPENCODE_API_KEY` is unset, the first request routed to `opencode` returns an authentication error.

Zen reads the credential from `x-api-key` only — an `Authorization: Bearer` header is rejected as a missing key. The preset already sends the right header, so there is nothing to configure; an explicit `auth = { mode = "api_key", header = "bearer" }` map is how you would override it (not what zen wants).

## Models

Zen serves a cross-vendor catalog — Claude, GPT, Gemini, Grok, GLM and more. The live list is public at `https://opencode.ai/zen/v1/models`; some example ids:

| Model id | Notes |
| :-- | :-- |
| `claude-fable-5-1` | Anthropic frontier tier on zen |
| `claude-opus-5` | Anthropic flagship tier on zen |
| `gpt-6-astra` | OpenAI tier on zen |

Model availability changes as the catalog rotates — check the live list before routing a new id, and verify a model id against your plan before advertising it.

Select a routed id in Claude Code via `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a subagent's `model:` frontmatter. To surface an entry in the `/model` picker instead, advertise a `claude`-prefixed alias with a `[models.upstream_model]` map — see [Model Discovery](/guides/model-discovery/).

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-fable-5-1-via-zen","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `opencode`, then [point Claude Code at shunt](/guides/connect-claude-code/).

## Notes

- Zen's `/v1/messages/count_tokens` does not exist: a client calling it gets zen's HTML 404 page rather than an Anthropic error shape. This is a zen-side gap; shunt's `count_tokens` passthrough reaches it unchanged. Claude Code's `/context` falls back on its own counting when a count endpoint fails, so the practical impact is a slower `/context`, not a broken one.
- A 402 `Payment Required` or similar billing error is zen's own response — the credential worked, the account balance or plan is what needs attention.
