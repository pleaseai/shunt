---
title: Standard Compute
description: Route mapped models to the Standard Compute Messages endpoint with a STANDARDCOMPUTE_API_KEY.
---

**Standard Compute** is an LLM gateway with an OpenAI-compatible API at
`https://api.stdcmpt.com/v1`. It also serves an **Anthropic Messages** endpoint at `/v1/messages`,
so shunt forwards Claude Code's Messages request as-is and injects the key. The built-in
`standardcompute` preset supplies `kind = "anthropic"`, `base_url = "https://api.stdcmpt.com"`,
and API-key auth from `STANDARDCOMPUTE_API_KEY`.

## Quick start

Let a coding agent wire it up for you. `shunt add` prints an embedded setup blueprint (offline and read-only; the agent edits the config, the command never does):

```bash
shunt add upstream standardcompute --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "standardcompute"
provider = "standardcompute"

[[routes]]
model = "StandardCompute"
provider = "standardcompute"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

## Credentials

```bash
export STANDARDCOMPUTE_API_KEY='...'
```

Create a key at [standardcompute.com](https://standardcompute.com). Never write the key into the
config. `shunt check` validates the config's structure but does not read the key's value. If
`STANDARDCOMPUTE_API_KEY` is unset, the first request routed to `standardcompute` returns an
authentication error.

## Models

| Model id | Notes |
| :-- | :-- |
| `StandardCompute` | Routed: sends each request to the cheapest enabled model that can handle it, with automatic fallback |

Select a routed id in Claude Code via `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a
subagent's `model:` frontmatter. To surface an entry in the `/model` picker instead, advertise a
`claude`-prefixed alias with a `[models.upstream_model]` map. See
[Model Discovery](/guides/model-discovery/).

## Verify

```bash
shunt check    # -> config ok
shunt run &    # or run it in a second terminal
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"StandardCompute","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `standardcompute`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
