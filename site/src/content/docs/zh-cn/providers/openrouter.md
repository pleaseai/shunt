---
title: OpenRouter
description: 将映射的模型路由到 OpenRouter 的兼容 Anthropic 端点 —— 一个 API 密钥,数百个模型。
---

**OpenRouter** 把众多模型厂商聚合在一个 API 密钥之后,并暴露一个
**兼容 Anthropic** 的端点 —— shunt 注入 OpenRouter 密钥并转发
Claude Code 的 Messages 请求。对于非 Anthropic 的 slug(`stealth/ox-alpha`……),
它会剥离 OpenRouter 的这层皮否则会以 HTTP 400 拒绝的延迟工具字段
(`defer_loading`、`tool_search_tool_*`);Anthropic 的 id(`claude*`、
`anthropic/*`)则保留这些字段。没有内置的预设,因此上游需
显式声明 `kind` 和 `base_url`。

## 快速开始

让编码 agent 为你完成接入 —— 对于没有具名蓝图的提供方,`shunt add`
会把文档 URL 注入其通用调研指南(离线且只读;配置由 agent
编辑,该命令绝不会修改配置):

```bash
shunt add upstream https://openrouter.ai/docs --print | claude
```

或者按照下面的手动步骤操作。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

[[upstreams]]
name = "openrouter"
kind = "anthropic"
base_url = "https://openrouter.ai/api"
auth = { mode = "api_key", env = "OPENROUTER_API_KEY" }

[[routes]]
model = "anthropic/claude-opus-4.8"
provider = "openrouter"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方,因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

旧的 `[providers.openrouter]` 表形式仍然受支持 —— 但不要在同一个文件中混用
`[[upstreams]]` 和 `[providers.*]`。

## 凭据

```bash
export OPENROUTER_API_KEY='...'
```

绝不要把密钥写进配置。`shunt check` 校验配置的结构,但不会
读取密钥的值 —— 如果 `OPENROUTER_API_KEY` 未设置,第一个被路由到 `openrouter` 的请求
会返回一个认证错误。

## 模型

OpenRouter 的模型 id 是 `vendor/model` 形式的 slug(例如 `anthropic/claude-opus-4.8`)—— 浏览
[OpenRouter 模型目录](https://openrouter.ai/models),并为每个你希望可达的 slug 添加一条
`[[routes]]` 条目。在 Claude Code 中通过 `ANTHROPIC_MODEL`、
`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子 agent 的 `model:` frontmatter 选择一个已路由的 id。
若想改为在 `/model` 选择器中呈现一个条目,请用
`[models.upstream_model]` 映射声明一个以 `claude` 为前缀的别名 —— 见 [模型发现](/zh-cn/guides/model-discovery/)。

## 校验

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"anthropic/claude-opus-4.8","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `openrouter`,然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。
