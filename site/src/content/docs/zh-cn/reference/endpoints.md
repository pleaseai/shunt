---
title: HTTP 端点
description: shunt 作为 Claude Code LLM 网关所提供的端点。
---

| 方法 | 路径 | 用途 |
| :-- | :-- | :-- |
| `HEAD` | `/` | 存活探测 |
| `GET` | `/` | 人类可读的落地页(版本 + 端点列表) |
| `GET` | `/health` | 健康检查 —— `{"status":"ok","version":"x.y.z"}` |
| `GET` | `/v1/models` | [模型发现](/zh-cn/guides/model-discovery/) —— 返回你的 `[[models]]` 条目 |
| `GET` | `/routes` | shunt 原生路由发现 —— 逐字返回配置的 `[[routes]]` 表(model → provider/upstream_model/effort 映射,包括 claude 前缀的发现别名);区别于 `/v1/models`,后者提供更窄的 Anthropic 协议发现响应(仅 `id`/`display_name`) |
| `POST` | `/v1/messages` | 推理 —— 按请求的 `model` id 路由 |
| `POST` | `/v1/messages/count_tokens` | [Token 计数](/zh-cn/guides/effort-and-context/#token-counting-count_tokens) |
| `GET` | `/admin` | 管理仪表盘(HTML);未登录时重定向到 `/admin/login` |
| `GET`, `POST` | `/admin/login` | 管理员 token 登录表单与浏览器会话创建 |
| `POST` | `/admin/logout` | 清除浏览器会话 |
| `GET` | `/admin/accounts` | 账户存储元数据:名称、类型、过期时间和 UUID;绝不返回 token 材料 |
| `GET` | `/admin/pool` | 按 `claude_oauth` 提供方的池健康状况:配额使用率、status、冷却与可用性 |
| `POST` | `/admin/accounts/claude` | 用 `{name}` 开始浏览器 setup token 预配;返回 `{authorize_url}` |
| `POST` | `/admin/accounts/claude/{name}/complete` | 用包含 `<code>#<state>` 的 `{code}` 完成预配;存储账户并报告其是否生效 |
| `DELETE` | `/admin/accounts/claude/{name}` | 删除指定名称账户的存储文件 |

`/admin*` 路由仅在配置了 [`[server.admin]`](/zh-cn/reference/configuration/#serveradmin可选) 时存在;没有该表时,它们一个都不会注册。

即使启用了 [`[server.auth]`](/zh-cn/guides/shared-gateway/),`GET /` 和 `GET /health` 也保持开放(健康检查工具通常无法附带 token),并且不暴露任何敏感信息 —— 只有状态、版本以及已经公开的端点列表。

## 网关协议

shunt 实现官方的 [Claude Code LLM 网关协议](https://code.claude.com/docs/en/llm-gateway-protocol):正确的头部和正文字段转发、特性透传以及系统提示归属处理。网关自身产生的错误以 Anthropic 错误形状返回,上游上下文溢出错误被重写为 Anthropic 的 `prompt is too long` 措辞,以便触发 Claude Code 的 [压缩并重试](/zh-cn/guides/effort-and-context/#context-overflow-recovery),而流式响应无缓冲地中继(带可选的 [keepalive ping](/zh-cn/guides/shared-gateway/#sse-keepalive-pings))。
