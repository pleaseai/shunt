---
title: Kimi (Moonshot)
description: Route mapped models to Moonshot's Anthropic-compatible Kimi endpoint with a MOONSHOT_API_KEY.
---

**Kimi** is Moonshot AI's model family, served over an **Anthropic-compatible** endpoint —
shunt forwards Claude Code's Messages request as-is and injects the Moonshot API key. The
`kimi` preset is built in, so configuration is one upstream entry plus routes.

## Quick start

Let a coding agent wire it up for you — `shunt add` prints an embedded setup blueprint
(offline and read-only; the agent edits the config, the command never does):

```bash
shunt add upstream kimi --print | claude
```

Or follow the manual steps below.

## Configure the upstream

The `kimi` preset supplies `kind = "anthropic"`, `base_url = "https://api.moonshot.ai/anthropic"`,
and API-key auth from `MOONSHOT_API_KEY`:

```toml
[[upstreams]]
name = "kimi"
provider = "kimi"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

The legacy `[providers.kimi]` table form remains supported (older examples used
`api_key_env = "KIMI_API_KEY"`, which still works when set explicitly) — but do not mix
`[[upstreams]]` and `[providers.*]` in one file.

## Credentials

```bash
export MOONSHOT_API_KEY='...'
```

Never write the key into the config. `shunt check` fails with a clear error if the variable is
missing.

## Models

| Model id | Notes |
| :-- | :-- |
| `kimi-k3[1m]` | frontier tier; `[1m]` is Claude Code's 1M-context marker — route the literal slug |
| `kimi-k2.7-code` | coding-focused tier |

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
  -d '{"model":"kimi-k2.7-code","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `kimi`, then
[point Claude Code at shunt](/guides/connect-claude-code/).

## Subagent plugin

The [`shunt-kimi` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-kimi) ships
one ready-made Claude Code subagent per model above:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-kimi@shunt
```
