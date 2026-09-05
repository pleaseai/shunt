---
title: MiniMax China
description: Route MiniMax-M3 to MiniMax's China Anthropic-compatible endpoint with a MINIMAX_API_KEY.
---

**MiniMax China** serves **MiniMax-M3** over an **Anthropic-compatible** endpoint. The built-in
`minimax-cn` preset supplies `kind = "anthropic"`,
`base_url = "https://api.minimax.cn/anthropic"`, and API-key auth from `MINIMAX_API_KEY`.

For the international endpoint, see [MiniMax](/providers/minimax/). The credentials and hosts are
separate.

## Configure the upstream

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "minimax-cn"
provider = "minimax-cn"

[[routes]]
model = "MiniMax-M3"
provider = "minimax-cn"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config must declare the
`anthropic` default it still falls back to (`server.default_provider` defaults to `anthropic`).

## Credentials

```bash
export MINIMAX_API_KEY='...'
```

Use a key from the China MiniMax open platform. Never write the key into the config.
`shunt check` validates the config's structure but does not read the key's value — if
`MINIMAX_API_KEY` is unset, the first request routed to `minimax-cn` returns an authentication
error.

## Models

| Model id | Notes |
| :-- | :-- |
| `MiniMax-M3` | 1M-token context; a client may append Claude Code's `[1m]` marker, which shunt strips before matching, so route the unsuffixed id |

Select the routed id in Claude Code via `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a
subagent's `model:` frontmatter. To surface an entry in the `/model` picker instead, advertise a
`claude`-prefixed alias with a `[models.upstream_model]` map — see
[Model Discovery](/guides/model-discovery/). A mapped id must **not** end in `[1m]`.

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `minimax-cn`, then
[point Claude Code at shunt](/guides/connect-claude-code/).
