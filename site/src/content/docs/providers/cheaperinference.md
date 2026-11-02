---
title: Cheaper Inference
description: Route mapped models to the Cheaper Inference Messages endpoint with a CHEAPER_INFERENCE_API_KEY.
---

**Cheaper Inference** is an OpenAI-compatible LLM gateway. One API key gives access to models
from several labs. It also serves an **Anthropic Messages** endpoint at `/v1/messages`, so shunt
forwards Claude Code's Messages request as-is and injects the key. The built-in
`cheaperinference` preset supplies `kind = "anthropic"`,
`base_url = "https://api.cheaperinference.com"`, and API-key auth from
`CHEAPER_INFERENCE_API_KEY`.

## Quick start

Let a coding agent wire it up for you — `shunt add` prints an embedded setup blueprint (offline and read-only; the agent edits the config, the command never does):

```bash
shunt add upstream cheaperinference --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "cheaperinference"
provider = "cheaperinference"

[[routes]]
model = "gpt-5.4"
provider = "cheaperinference"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

## Credentials

```bash
export CHEAPER_INFERENCE_API_KEY='...'
```

Create a key at [cheaperinference.com](https://cheaperinference.com). Never write the key into the
config. `shunt check` validates the config's structure but does not read the key's value — if
`CHEAPER_INFERENCE_API_KEY` is unset, the first request routed to `cheaperinference` returns an
authentication error.

## Models

Model ids are bare (no vendor prefix). The public list is at
`https://api.cheaperinference.com/public/models`; some example ids:

| Model id | Notes |
| :-- | :-- |
| `gpt-5.4` | OpenAI model |
| `deepseek-v4-flash` | DeepSeek model |
| `glm-5.3` | Z.ai model |

Check the live list before routing a new id. Select a routed id in Claude Code via
`ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a subagent's `model:` frontmatter. To
surface an entry in the `/model` picker instead, advertise a `claude`-prefixed alias with a
`[models.upstream_model]` map — see [Model Discovery](/guides/model-discovery/).

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5.4","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `cheaperinference`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
