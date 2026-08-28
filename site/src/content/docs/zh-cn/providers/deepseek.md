---
title: DeepSeek
description: 用 DEEPSEEK_API_KEY 将映射的模型路由到 DeepSeek 的兼容 Anthropic 端点。
---

**DeepSeek** 通过一个**兼容 Anthropic** 的端点提供其模型 —— shunt 原样转发
Claude Code 的 Messages 请求并注入 DeepSeek API 密钥。没有内置的
预设,因此上游需显式声明 `kind` 和 `base_url`。

## 快速开始

让编码 agent 为你完成接入 —— 对于没有具名蓝图的提供方,`shunt add`
会把文档 URL 注入其通用调研指南(离线且只读;配置由 agent
编辑,该命令绝不会修改配置):

```bash
shunt add upstream https://api-docs.deepseek.com/guides/anthropic_api --print | claude
```

或者按照下面的手动步骤操作。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "deepseek"
kind = "anthropic"
base_url = "https://api.deepseek.com/anthropic"
auth = { mode = "api_key", env = "DEEPSEEK_API_KEY" }

[[routes]]
model = "deepseek-v4-pro"
provider = "deepseek"

[[routes]]
model = "deepseek-v4-flash"
provider = "deepseek"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方,因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

旧的 `[providers.deepseek]` 表形式仍然受支持 —— 但不要在同一个文件中混用 `[[upstreams]]`
和 `[providers.*]`。

## 凭据

```bash
export DEEPSEEK_API_KEY='...'
```

绝不要把密钥写进配置。`shunt check` 校验配置的结构,但不会
读取密钥的值 —— 如果 `DEEPSEEK_API_KEY` 未设置,第一个被路由到 `deepseek` 的请求
会返回一个认证错误。

## 模型

| 模型 id | 说明 |
| :-- | :-- |
| `deepseek-v4-pro` | 前沿等级 |
| `deepseek-v4-flash` | 快速、更轻量的等级 |

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
  -d '{"model":"deepseek-v4-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `deepseek`,然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。

## 子 agent 插件

[`shunt-deepseek` 插件](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-deepseek)
为上面每个模型各提供一个现成的 Claude Code 子 agent:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-deepseek@shunt
```
