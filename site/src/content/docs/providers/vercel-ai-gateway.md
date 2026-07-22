---
title: Vercel AI Gateway
description: Route mapped models through Vercel's AI Gateway Anthropic-compatible endpoint with an AI_GATEWAY_API_KEY.
---

**Vercel AI Gateway** fronts many model vendors behind one key and exposes an
**Anthropic-compatible** endpoint — shunt forwards Claude Code's Messages request as-is and
injects the gateway key. There is no built-in preset, so the upstream declares `kind` and
`base_url` explicitly.

## Quick start

Let a coding agent wire it up for you — for a provider without a named blueprint, `shunt add`
injects the documentation URL into its generic research guide (offline and read-only; the agent
edits the config, the command never does):

```bash
shunt add upstream https://vercel.com/docs/ai-gateway --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "vercel"
kind = "anthropic"
base_url = "https://ai-gateway.vercel.sh"
auth = { mode = "api_key", env = "AI_GATEWAY_API_KEY" }

[[routes]]
model = "anthropic/claude-opus-4.8"
provider = "vercel"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

The gateway accepts both bearer auth (the default) and Anthropic's `x-api-key` header — add
`header = "x_api_key"` to the auth map if you prefer the latter. The legacy `[providers.vercel]`
table form remains supported — but do not mix `[[upstreams]]` and `[providers.*]` in one file.

## Credentials

```bash
export AI_GATEWAY_API_KEY='...'
```

Never write the key into the config. `shunt check` validates the config's structure but does not
read the key's value — if `AI_GATEWAY_API_KEY` is unset, the first request routed to `vercel`
returns an authentication error.

## Models

AI Gateway model ids are `vendor/model` slugs (e.g. `anthropic/claude-opus-4.8`) — see the
[AI Gateway model catalog](https://vercel.com/ai-gateway/models) and add one `[[routes]]` entry
per slug you want reachable. Select a routed id in Claude Code via `ANTHROPIC_MODEL`,
`ANTHROPIC_CUSTOM_MODEL_OPTION`, or a subagent's `model:` frontmatter. To surface an entry in
the `/model` picker instead, advertise a `claude`-prefixed alias with a
`[models.upstream_model]` map — see [Model Discovery](/guides/model-discovery/).

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"anthropic/claude-opus-4.8","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `vercel`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
