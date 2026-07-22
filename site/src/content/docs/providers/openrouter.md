---
title: OpenRouter
description: Route mapped models to OpenRouter's Anthropic-compatible endpoint — one API key, hundreds of models.
---

**OpenRouter** aggregates many model vendors behind one API key, and exposes an
**Anthropic-compatible** endpoint — shunt forwards Claude Code's Messages request as-is and
injects the OpenRouter key. There is no built-in preset, so the upstream declares `kind` and
`base_url` explicitly.

## Quick start

Let a coding agent wire it up for you — for a provider without a named blueprint, `shunt add`
injects the documentation URL into its generic research guide (offline and read-only; the agent
edits the config, the command never does):

```bash
shunt add upstream https://openrouter.ai/docs --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "openrouter"
kind = "anthropic"
base_url = "https://openrouter.ai/api"
auth = { mode = "api_key", env = "OPENROUTER_API_KEY" }

[[routes]]
model = "anthropic/claude-opus-4.8"
provider = "openrouter"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

The legacy `[providers.openrouter]` table form remains supported — but do not mix
`[[upstreams]]` and `[providers.*]` in one file.

## Credentials

```bash
export OPENROUTER_API_KEY='...'
```

Never write the key into the config. `shunt check` validates the config's structure but does not
read the key's value — if `OPENROUTER_API_KEY` is unset, the first request routed to `openrouter`
returns an authentication error.

## Models

OpenRouter model ids are `vendor/model` slugs (e.g. `anthropic/claude-opus-4.8`) — browse the
[OpenRouter model catalog](https://openrouter.ai/models) and add one `[[routes]]` entry per slug
you want reachable. Select a routed id in Claude Code via `ANTHROPIC_MODEL`,
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

Confirm the response's `x-gateway-upstream` header names `openrouter`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
