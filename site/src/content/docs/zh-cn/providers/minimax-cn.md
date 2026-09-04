---
title: MiniMax China
description: 用 MINIMAX_API_KEY 将 MiniMax-M3 路由到 MiniMax 国内兼容 Anthropic 的端点。
---

**MiniMax 中国版**通过**兼容 Anthropic** 的端点提供 **MiniMax-M3**。内置的 `minimax-cn`
预设提供 `kind = "anthropic"`、`base_url = "https://api.minimax.cn/anthropic"`，以及来自
`MINIMAX_API_KEY` 的 API 密钥认证。

国际版端点见 [MiniMax](/zh-cn/providers/minimax/)。两者的主机和凭据互不相同。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

[[upstreams]]
name = "minimax-cn"
provider = "minimax-cn"

[[routes]]
model = "MiniMax-M3"
provider = "minimax-cn"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方，因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

## 凭据

```bash
export MINIMAX_API_KEY='...'
```

请使用中国 MiniMax 开放平台的密钥。绝不要把密钥写进配置。`shunt check` 校验配置的结构，但不会
读取密钥的值 —— 如果 `MINIMAX_API_KEY` 未设置，第一个被路由到 `minimax-cn` 的请求会返回认证错误。

## 模型

| 模型 ID | 说明 |
| :-- | :-- |
| `MiniMax-M3` | 1M token 上下文；客户端可以添加 Claude Code 的 `[1m]` 标记，shunt 会在匹配前移除它，因此请路由不带后缀的 ID |

在 Claude Code 中通过 `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子 agent 的
`model:` frontmatter 选择一个已路由的 ID。

## 校验

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `minimax-cn`，然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。
