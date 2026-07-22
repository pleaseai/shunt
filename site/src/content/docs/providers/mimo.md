---
title: Mimo (Xiaomi)
description: Route mimo-v2.5-pro to Xiaomi's Anthropic-compatible Mimo endpoint with a MIMO_API_KEY.
---

**Mimo** is Xiaomi's model family, served over an **Anthropic-compatible** endpoint — shunt
forwards Claude Code's Messages request as-is and injects the Mimo API key. There is no built-in
preset, so the upstream declares `kind` and `base_url` explicitly.

## Quick start

Let a coding agent wire it up for you — for a provider without a named blueprint, `shunt add`
injects the documentation URL into its generic research guide (offline and read-only; the agent
edits the config, the command never does):

```bash
shunt add upstream https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "mimo"
kind = "anthropic"
base_url = "https://api.xiaomimimo.com/anthropic"
auth = { mode = "api_key", env = "MIMO_API_KEY" }

[[routes]]
model = "mimo-v2.5-pro"
provider = "mimo"
```

:::note[base_url depends on your plan]
`https://api.xiaomimimo.com/anthropic` is the pay-as-you-go host. On a **Token Plan**, use
`https://token-plan-cn.xiaomimimo.com/anthropic` instead — set `base_url` to whichever your
account requires. Hosts are from Xiaomi's current
[Claude Code integration docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode).
:::

The legacy `[providers.mimo]` table form remains supported — but do not mix `[[upstreams]]` and
`[providers.*]` in one file.

## Credentials

```bash
export MIMO_API_KEY='...'
```

Never write the key into the config. `shunt check` fails with a clear error if the variable is
missing.

## Models

| Model id | Notes |
| :-- | :-- |
| `mimo-v2.5-pro` | standard context |
| `mimo-v2.5-pro[1m]` | 1M-context variant — route this literal slug too if you want extended context |

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
  -d '{"model":"mimo-v2.5-pro","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `mimo`, then
[point Claude Code at shunt](/guides/connect-claude-code/).

## Subagent plugin

The [`shunt-mimo` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-mimo) ships
a ready-made Claude Code subagent for the model above:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-mimo@shunt
```
