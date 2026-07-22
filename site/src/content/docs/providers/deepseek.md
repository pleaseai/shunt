---
title: DeepSeek
description: Route mapped models to DeepSeek's Anthropic-compatible endpoint with a DEEPSEEK_API_KEY.
---

**DeepSeek** serves its models over an **Anthropic-compatible** endpoint — shunt forwards
Claude Code's Messages request as-is and injects the DeepSeek API key. There is no built-in
preset, so the upstream declares `kind` and `base_url` explicitly.

## Quick start

Let a coding agent wire it up for you — for a provider without a named blueprint, `shunt add`
injects the documentation URL into its generic research guide (offline and read-only; the agent
edits the config, the command never does):

```bash
shunt add upstream https://api-docs.deepseek.com/guides/anthropic_api --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "deepseek"
kind = "anthropic"
base_url = "https://api.deepseek.com/anthropic"
auth = { mode = "api_key", env = "DEEPSEEK_API_KEY" }

[[routes]]
model = "deepseek-v4-pro"
provider = "deepseek"

[[routes]]
model = "deepseek-v4-flash"
provider = "deepseek"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

The legacy `[providers.deepseek]` table form remains supported — but do not mix `[[upstreams]]`
and `[providers.*]` in one file.

## Credentials

```bash
export DEEPSEEK_API_KEY='...'
```

Never write the key into the config. `shunt check` validates the config's structure but does not
read the key's value — if `DEEPSEEK_API_KEY` is unset, the first request routed to `deepseek`
returns an authentication error.

## Models

| Model id | Notes |
| :-- | :-- |
| `deepseek-v4-pro` | frontier tier |
| `deepseek-v4-flash` | fast, lighter tier |

Select a routed id in Claude Code via `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a
subagent's `model:` frontmatter. To surface an entry in the `/model` picker instead, advertise a
`claude`-prefixed alias with a `[models.upstream_model]` map — see
[Model Discovery](/guides/model-discovery/).

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek-v4-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `deepseek`, then
[point Claude Code at shunt](/guides/connect-claude-code/).

## Subagent plugin

The [`shunt-deepseek` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-deepseek)
ships one ready-made Claude Code subagent per model above:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-deepseek@shunt
```
