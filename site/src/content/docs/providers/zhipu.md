---
title: Zhipu (GLM China)
description: Route GLM Coding Plan models to Zhipu's Anthropic-compatible BigModel endpoint with a ZHIPUAI_API_KEY.
---

**Zhipu** serves **GLM** models from its China BigModel platform over an
**Anthropic-compatible** endpoint. The built-in `zhipu` preset supplies
`kind = "anthropic"`, `base_url = "https://open.bigmodel.cn/api/anthropic"`, and API-key auth
from `ZHIPUAI_API_KEY`.

For the international Z.ai endpoint, see [Z.ai (GLM)](/providers/zai/). The credentials and
hosts are separate.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "zhipu"
provider = "zhipu"

[[routes]]
model = "glm-5.3"
provider = "zhipu"

[[routes]]
model = "glm-5.3-flash"
provider = "zhipu"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

## Credentials

```bash
export ZHIPUAI_API_KEY='...'
```

Never write the key into the config. `shunt check` validates the config's structure but does not
read the key's value — if `ZHIPUAI_API_KEY` is unset, the first request routed to `zhipu` returns
an authentication error.

## Models

| Model id | Notes |
| :-- | :-- |
| `glm-5.3` | GLM Coding Plan flagship text model |
| `glm-5.3-flash` | faster multimodal tier; clients may append `[1m]`, which shunt strips before route matching |

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
  -d '{"model":"glm-5.3-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `zhipu`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
