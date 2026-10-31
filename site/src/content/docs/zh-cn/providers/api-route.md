---
title: API Route
description: 使用自己的 API 密钥通过 API Route 路由模型。
---

[API Route](https://api-route.com) 是一个 LLM 网关。此预设使用其 `/v1/messages` Anthropic Messages 端点，通过 `API_ROUTE_API_KEY` 提供 Bearer 认证。API Route 用量与 Claude 订阅单独计费。

## 配置

```bash
shunt add upstream api-route --print | claude
```

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "api-route"
provider = "api-route"

[[routes]]
model = "gpt-6.1-sol"
provider = "api-route"
```

保留 Anthropic upstream 作为未匹配模型的默认目标。不要将有序 upstream 与旧版 provider 表混用。

## 凭据

```bash
export API_ROUTE_API_KEY='...'
```

在 [API Route](https://api-route.com) 创建自己的密钥，不要写入配置文件或版本控制。`shunt check` 仅检查配置结构，不验证凭据。

## 模型和验证

使用密钥查询模型列表。可用模型取决于账户；选择其他模型前应验证其 Messages 支持。

```bash
curl -sS https://global.api-route.com/v1/models \
  -H "Authorization: Bearer $API_ROUTE_API_KEY"
```

```bash
shunt check
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-6.1-sol","max_tokens":32,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应包含 `x-gateway-upstream: api-route`。客户端配置和模型别名请参阅 [连接 Claude Code](/zh-cn/guides/connect-claude-code/) 和 [模型发现](/zh-cn/guides/model-discovery/)。
