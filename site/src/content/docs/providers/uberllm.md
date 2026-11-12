---
title: UberLLM
description: Route mapped models to the UberLLM Messages endpoint with an UBERLLM_API_KEY.
---

**UberLLM** is an LLM gateway with OpenAI-compatible and **Anthropic Messages** endpoints. One API
key gives access to models from many labs, Claude models included. Because it serves
`/v1/messages`, shunt forwards Claude Code's Messages request as-is and injects the key. The
built-in `uberllm` preset supplies `kind = "anthropic"`, `base_url = "https://api.uberllm.dev"`,
and API-key auth from `UBERLLM_API_KEY`.

## Quick start

Let a coding agent wire it up for you — `shunt add` prints an embedded setup blueprint (offline and read-only; the agent edits the config, the command never does):

```bash
shunt add upstream uberllm --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "uberllm"
provider = "uberllm"

[[routes]]
model = "deepseek/deepseek-v4.1-flash"
provider = "uberllm"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

## Credentials

```bash
export UBERLLM_API_KEY='...'
```

Create a key at [uberllm.dev](https://uberllm.dev/signup). Never write the key into the config.
`shunt check` validates the config's structure but does not read the key's value — if
`UBERLLM_API_KEY` is unset, the first request routed to `uberllm` returns an authentication
error.

## Models

Model ids use the `vendor/model` form. The public list is at `https://api.uberllm.dev/v1/models`
(no key needed), with prices at [uberllm.dev/models](https://uberllm.dev/models); some example ids:

| Model id | Notes |
| :-- | :-- |
| `deepseek/deepseek-v4.1-flash` | DeepSeek model |
| `anthropic/claude-sonnet-5` | Anthropic model |
| `openai/gpt-5.4-mini` | OpenAI model |

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
  -d '{"model":"deepseek/deepseek-v4.1-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `uberllm`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
