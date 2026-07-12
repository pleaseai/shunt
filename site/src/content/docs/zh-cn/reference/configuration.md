---
title: 配置参考
description: 每一个 shunt.toml 键 —— server、providers、routes、models。
---

关于文件位置、优先级以及带注释的示例,见 [配置](/zh-cn/guides/configuration/)。完整模板:[`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)。

## `[server]`

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt 监听的地址 |
| `default_provider` | `anthropic` | 面向任何无匹配路由的模型的提供方 |
| `sse_keepalive_seconds` | `30` | 注入 SSE `ping` 前的闲置秒数;`0` 禁用([详情](/zh-cn/guides/shared-gateway/#sse-keepalive-pings)) |

## `[server.auth]`(可选)

存在此表即启用入站客户端 token 认证([详情](/zh-cn/guides/shared-gateway/)):

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | 携带客户端 token 的头部 |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | 保存逗号分隔的 `name:token` 对的环境变量 |

## `[providers.<name>]`

每个提供方都是一个以你自选名称命名的表。内置项(`anthropic`、`openai`、`codex`)可被部分覆盖 —— 配置映射深度合并。

| 键 | 取值 | 含义 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` | 上游协议 / 适配器。`anthropic` = Messages API(透传,可选择重新设置密钥);`responses` = Anthropic Messages 转换为 OpenAI Responses API。 |
| `base_url` | URL | 上游 base;shunt 追加端点路径。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` | `passthrough` 转发客户端自己的凭据;`api_key` 从 `api_key_env` 注入一个密钥;`chatgpt_oauth` 复用 `~/.codex/auth.json`。 |
| `api_key_env` | 环境变量名 | 当 `auth = "api_key"` 时,从何处读取密钥。 |
| `api_key_header` | `bearer`(默认) \| `x_api_key` | 注入的密钥在哪个头部中发送。 |
| `effort` | `low` … `max` | 可选的默认推理力度(`responses` 提供方)。 |
| `count_tokens` | `tiktoken`(默认) \| `estimate` | 仅 `responses` 提供方:本地 tiktoken 计数 vs. 404 回退([详情](/zh-cn/guides/effort-and-context/#token-counting-count_tokens))。 |

## `[[routes]]`

精确匹配的路由条目 —— 最先检查:

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `model` | ✅ | Claude Code 发送的精确 `model` id |
| `provider` | ✅ | 某个 `[providers.<name>]` 表的名称 |
| `upstream_model` | — | 重写转发给上游的模型 id |
| `effort` | — | 按路由的推理力度覆盖 |

## `[[route_prefixes]]`

前缀匹配的路由条目 —— 在精确路由之后检查:

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `prefix` | ✅ | 模型 id 前缀,如 `gpt-` |
| `provider` | ✅ | 某个 `[providers.<name>]` 表的名称 |

## `[[models]]`

由 `GET /v1/models` 为 [模型发现](/zh-cn/guides/model-discovery/) 返回的条目。id 必须以 `claude` 或 `anthropic` 开头,否则 Claude Code 会忽略它们。

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `id` | ✅ | 暴露给 Claude Code 的模型 id |
| `display_name` | — | 在 `/model` 选择器中显示的标签 |

## 路由优先级

精确 `[[routes]]` 匹配 → `[[route_prefixes]]` 前缀匹配 → `server.default_provider`。
