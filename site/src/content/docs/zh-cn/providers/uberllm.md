---
title: UberLLM
description: 用 UBERLLM_API_KEY 将映射的模型路由到 UberLLM 的 Messages 端点。
---

**UberLLM** 是一个同时提供 OpenAI 兼容端点和 **Anthropic Messages** 端点的 LLM 网关。一个 API
密钥即可使用多家实验室的模型,包括 Claude 模型。由于它提供 `/v1/messages`,shunt 原样转发
Claude Code 的 Messages 请求并注入密钥。内置的 `uberllm` 预设提供 `kind = "anthropic"`、
`base_url = "https://api.uberllm.dev"`,以及来自 `UBERLLM_API_KEY` 的 API 密钥认证。

## 快速开始

让一个编码代理替你接线 —— `shunt add` 打印内嵌的安装蓝图(离线且只读;代理编辑配置,命令本身从不改):

```bash
shunt add upstream uberllm --print | claude
```

或按以下手动步骤操作。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

[[upstreams]]
name = "uberllm"
provider = "uberllm"

[[routes]]
model = "deepseek/deepseek-v4.1-flash"
provider = "uberllm"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方，因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

## 凭据

```bash
export UBERLLM_API_KEY='...'
```

请在 [uberllm.dev](https://uberllm.dev/signup) 创建密钥。绝不要把密钥写进配置。
`shunt check` 校验配置的结构，但不会读取密钥的值 —— 如果 `UBERLLM_API_KEY` 未设置，
第一个被路由到 `uberllm` 的请求会返回认证错误。

## 模型

模型 ID 采用 `vendor/model` 形式。公开列表位于 `https://api.uberllm.dev/v1/models`(无需密钥),
价格见 [uberllm.dev/models](https://uberllm.dev/models);部分示例 ID:

| 模型 ID | 说明 |
| :-- | :-- |
| `deepseek/deepseek-v4.1-flash` | DeepSeek 模型 |
| `anthropic/claude-sonnet-5` | Anthropic 模型 |
| `openai/gpt-5.4-mini` | OpenAI 模型 |

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
  -d '{"model":"deepseek/deepseek-v4.1-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `uberllm`,然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。
