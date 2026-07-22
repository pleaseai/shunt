---
title: Z.ai (GLM)
description: Route mapped models to Z.ai's Anthropic-compatible GLM endpoint with a ZAI_API_KEY.
---

**Z.ai** serves its **GLM** models over an **Anthropic-compatible** endpoint — shunt forwards
Claude Code's Messages request as-is and injects the Z.ai API key. There is no built-in preset,
so the upstream declares `kind` and `base_url` explicitly.

## Quick start

Let a coding agent wire it up for you — for a provider without a named blueprint, `shunt add`
injects the documentation URL into its generic research guide (offline and read-only; the agent
edits the config, the command never does):

```bash
shunt add upstream https://docs.z.ai/ --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "zai"
kind = "anthropic"
base_url = "https://api.z.ai/api/anthropic"
auth = { mode = "api_key", env = "ZAI_API_KEY" }

[[routes]]
model = "glm-5.2"
provider = "zai"

[[routes]]
model = "glm-4.7"
provider = "zai"
```

The legacy `[providers.zai]` table form remains supported — but do not mix `[[upstreams]]` and
`[providers.*]` in one file.

## Credentials

```bash
export ZAI_API_KEY='...'
```

Never write the key into the config. `shunt check` fails with a clear error if the variable is
missing.

## Models

| Model id | Notes |
| :-- | :-- |
| `glm-5.2` | frontier tier |
| `glm-4.7` | previous generation |

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
  -d '{"model":"glm-5.2","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `zai`, then
[point Claude Code at shunt](/guides/connect-claude-code/).

## Subagent plugin

The [`shunt-zai` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-zai) ships
one ready-made Claude Code subagent per model above:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-zai@shunt
```
