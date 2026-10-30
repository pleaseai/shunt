---
title: Cheaper Inference
description: 用 CHEAPER_INFERENCE_API_KEY 将映射的模型路由到 Cheaper Inference 的 Messages 端点。
---

**Cheaper Inference** 是一个兼容 OpenAI 的 LLM 网关。一个 API 密钥即可使用多家实验室的模型。它还在
`/v1/messages` 提供 **Anthropic Messages** 端点,所以 shunt 原样转发 Claude Code 的 Messages
请求并注入密钥。内置的 `cheaperinference` 预设提供 `kind = "anthropic"`、
`base_url = "https://api.cheaperinference.com"`,以及来自 `CHEAPER_INFERENCE_API_KEY` 的 API
密钥认证。

## 快速开始

让一个编码代理替你接线 —— `shunt add` 打印内嵌的安装蓝图(离线且只读;代理编辑配置,命令本身从不改):

```bash
shunt add upstream cheaperinference --print | claude
```

或按以下手动步骤操作。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

[[upstreams]]
name = "cheaperinference"
provider = "cheaperinference"

[[routes]]
model = "gpt-5.4"
provider = "cheaperinference"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方，因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

## 凭据

```bash
export CHEAPER_INFERENCE_API_KEY='...'
```

请在 [cheaperinference.com](https://cheaperinference.com) 创建密钥。绝不要把密钥写进配置。
`shunt check` 校验配置的结构，但不会读取密钥的值 —— 如果 `CHEAPER_INFERENCE_API_KEY` 未设置，
第一个被路由到 `cheaperinference` 的请求会返回认证错误。

## 模型

模型 ID 不带厂商前缀。公开列表位于 `https://api.cheaperinference.com/public/models`;部分示例 ID:

| 模型 ID | 说明 |
| :-- | :-- |
| `gpt-5.4` | OpenAI 模型 |
| `deepseek-v4-flash` | DeepSeek 模型 |
| `glm-5.3` | Z.ai 模型 |

路由新 ID 前请先查看最新列表。在 Claude Code 中通过 `ANTHROPIC_MODEL`、
`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子 agent 的 `model:` frontmatter 选择一个已路由的 ID。要在
`/model` 选择器中展示条目,请用 `[models.upstream_model]` 映射声明一个以 `claude` 为前缀的别名
—— 见 [模型发现](/zh-cn/guides/model-discovery/)。

## 校验

```bash
shunt check    # -> config ok
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5.4","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `cheaperinference`,然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。
