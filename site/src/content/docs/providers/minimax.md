---
title: MiniMax
description: Route MiniMax-M3 (1M context) to MiniMax's Anthropic-compatible endpoint with a MINIMAX_API_KEY.
---

**MiniMax** serves its models over an **Anthropic-compatible** endpoint — shunt forwards Claude
Code's Messages request as-is and injects the MiniMax API key. There is no built-in preset, so
the upstream declares `kind` and `base_url` explicitly.

## Quick start

Let a coding agent wire it up for you — for a provider without a named blueprint, `shunt add`
injects the documentation URL into its generic research guide (offline and read-only; the agent
edits the config, the command never does):

```bash
shunt add upstream https://platform.minimax.io/docs/token-plan/claude-code --print | claude
```

Or follow the manual steps below.

## Configure the upstream

```toml
[[upstreams]]
name = "minimax"
kind = "anthropic"
base_url = "https://api.minimax.io/anthropic"
auth = { mode = "api_key", env = "MINIMAX_API_KEY" }

[[routes]]
model = "MiniMax-M3[1m]"
provider = "minimax"
```

The legacy `[providers.minimax]` table form remains supported — but do not mix `[[upstreams]]`
and `[providers.*]` in one file.

## Credentials

```bash
export MINIMAX_API_KEY='...'
```

Never write the key into the config. `shunt check` fails with a clear error if the variable is
missing.

## Models

| Model id | Notes |
| :-- | :-- |
| `MiniMax-M3[1m]` | 1M-token context; `[1m]` is Claude Code's context marker — MiniMax's own [Claude Code integration](https://platform.minimax.io/docs/token-plan/claude-code) documents this exact slug, so route the literal id |

Select the routed id in Claude Code via `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a
subagent's `model:` frontmatter. To surface an entry in the `/model` picker instead, advertise a
`claude`-prefixed alias with a `[models.upstream_model]` map — see
[Model Discovery](/guides/model-discovery/). Note a mapped id must **not** end in `[1m]`
(clients strip the hint before matching), so keep the literal `MiniMax-M3[1m]` on the
`[[routes]]` form.

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `minimax`, then
[point Claude Code at shunt](/guides/connect-claude-code/).

## Subagent plugin

The [`shunt-minimax` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-minimax)
ships a ready-made Claude Code subagent for the model above:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-minimax@shunt
```
