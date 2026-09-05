---
title: Z.ai (GLM)
description: 用 ZAI_API_KEY 将映射的模型路由到 Z.ai 的兼容 Anthropic GLM 端点。
---

**Z.ai** 通过一个**兼容 Anthropic** 的端点提供其 **GLM** 模型 —— shunt 原样转发
Claude Code 的 Messages 请求并注入 Z.ai API 密钥。没有内置的预设,
因此上游需显式声明 `kind` 和 `base_url`。

## 快速开始

让编码 agent 为你完成接入 —— 对于没有具名蓝图的提供方,`shunt add`
会把文档 URL 注入其通用调研指南(离线且只读;配置由 agent
编辑,该命令绝不会修改配置):

```bash
shunt add upstream https://docs.z.ai/ --print | claude
```

或者按照下面的手动步骤操作。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

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

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方,因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

旧的 `[providers.zai]` 表形式仍然受支持 —— 但不要在同一个文件中混用 `[[upstreams]]` 和
`[providers.*]`。

## 凭据

```bash
export ZAI_API_KEY='...'
```

绝不要把密钥写进配置。`shunt check` 校验配置的结构,但不会
读取密钥的值 —— 如果 `ZAI_API_KEY` 未设置,第一个被路由到 `zai` 的请求会返回一个
认证错误。

## 模型

| 模型 id | 说明 |
| :-- | :-- |
| `glm-5.2` | 前沿等级 |
| `glm-4.7` | 上一代 |

在 Claude Code 中通过 `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子 agent 的
`model:` frontmatter 选择一个已路由的 id。若想改为在 `/model` 选择器中呈现一个条目,
请用 `[models.upstream_model]` 映射声明一个以 `claude` 为前缀的别名 —— 见
[模型发现](/zh-cn/guides/model-discovery/)。

## 校验

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"glm-5.2","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `zai`,然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。

## 子 agent 插件

[`shunt-zai` 插件](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-zai)
为上面每个模型各提供一个现成的 Claude Code 子 agent:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-zai@shunt
```
