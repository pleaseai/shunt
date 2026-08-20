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
| `GET` | `/routes` | shunt 原生路由发现 —— 逐字返回配置的 `[[routes]]` 表(model → provider/upstream_model/effort 映射,包括 claude 前缀的发现别名);区别于 `/v1/models`,后者提供更窄的 Anthropic 协议发现响应(`id`、`display_name` 以及上游模型元数据) |
| `POST` | `/v1/messages` | 推理 —— 按请求的 `model` id 路由 |
| `POST` | `/v1/messages/count_tokens` | [Token 计数](/zh-cn/guides/effort-and-context/#token-counting-count_tokens) |
| `GET` | `/managed/settings` | 按网关 JWT 提供的 Claude Code managed settings;支持 `ETag`、`If-None-Match` 与 `304 Not Modified` |
| `GET` | `/v1/organizations/spend_limits` | 使用方向游标分页列出已存储的支出限制 |
| `POST` | `/v1/organizations/spend_limits` | 为一个 `(scope, period)` 创建或替换支出限制 |
| `GET` | `/v1/organizations/spend_limits/{id}` | 获取一个已存储的支出限制 |
| `DELETE` | `/v1/organizations/spend_limits/{id}` | 删除一个已存储的支出限制 |
| `POST` | `/v1/metrics` | 来自托管 Claude Code 客户端的入站 OTLP/HTTP 指标 —— verbatim 中继到 opt-in 的网关遥测目标 |
| `POST` | `/v1/logs` | 入站 OTLP/HTTP log record —— 只中继到 `logs = true` 的目标 |
| `POST` | `/v1/traces` | 入站 OTLP/HTTP span —— 只中继到 `traces = true` 的目标 |
| `GET` | `/admin` | 管理仪表盘(HTML);未登录时重定向到 `/admin/login` |
| `GET`, `POST` | `/admin/login` | 管理员 token 登录表单与浏览器会话创建 |
| `POST` | `/admin/logout` | 清除浏览器会话 |
| `GET` | `/admin/accounts` | Claude 账户存储元数据:名称、类型、过期时间和 UUID;绝不返回 token 材料 |
| `GET` | `/admin/accounts/codex` | Codex 账户存储元数据:名称、过期时间和 ChatGPT 账户 ID;绝不返回 token 材料 |
| `GET` | `/admin/pool` | `claude_oauth` / `chatgpt_oauth` / `kimi_oauth` provider 的池状态;每个 account 对象可能包含可选的 `plan` 字符串;Codex 不发送配额 header,因此使用率字段为空 |
| `POST` | `/admin/accounts/claude` | 用 `{name, mode}` 开始 Claude 浏览器预配;`mode` 为 `oauth` 或 `setup_token`,省略时默认为 `setup_token`;返回 `{authorize_url}` |
| `POST` | `/admin/accounts/claude/{name}/complete` | 用包含 `<code>#<state>` 的 `{code}` 完成 Claude 预配;存储账户并报告其是否生效 |
| `DELETE` | `/admin/accounts/claude/{name}` | 删除指定 Claude 账户的存储文件 |
| `POST` | `/admin/accounts/codex` | 用 `{name}` 开始 ChatGPT OAuth;返回 `{authorize_url}` |
| `POST` | `/admin/accounts/codex/{name}/complete` | 用包含完整 localhost redirect URL 或 `<code>#<state>` 的 `{code}` 完成 Codex 预配 |
| `DELETE` | `/admin/accounts/codex/{name}` | 删除指定 Codex 账户的存储文件 |
| `POST` | `/backend-api/codex/responses` | 入站 Codex CLI 透传 —— 镜像真实 ChatGPT 后端路径 |
| `POST` | `/responses` | 入站 Codex CLI 透传 —— 裸 `base_url` 形式 |
| `POST` | `/v1/responses` | 入站 Codex CLI 透传 —— 带 `/v1` 后缀的 `base_url` 形式 |
| `POST` | `/backend-api/codex/analytics-events/events` | Codex CLI 分析 sink —— 接收后丢弃，仅记录净化后的事件名称计数器 |
| `POST` | `/codex/analytics-events/events` | Codex CLI 分析 sink —— 根路径式 `chatgpt_base_url` 形式 |

`/admin*` 路由仅在配置了 [`[server.admin]`](/zh-cn/reference/configuration/#serveradmin可选) 时存在;没有该表时,它们一个都不会注册。管理员凭据可通过配置的头部或 `x-api-key` 提交,`read_keys` 凭据可以通过上面的所有 GET,但在所有修改操作上会被 `403` 拒绝,在 `POST /admin/login` 上会被 `401` 拒绝。

spend-limit 路由仅在启动时配置了 [`[server.spend]`](/zh-cn/reference/configuration/#serverspend可选) 的情况下存在;它们使用 [`[server.admin]`](/zh-cn/reference/configuration/#serveradmin可选) 凭据认证,因此与 `[server.gateway]` 无关。请通过配置的管理员头部(默认 `x-shunt-admin-token`)或 `x-api-key` 发送该凭据 —— 两个槽位都被接受。write 凭据(`write_keys` 条目,或 `tokens_env`/`tokens_file` 对)可使用全部操作;`read_keys` 凭据只能使用 GET,在修改操作上会收到 `403`。`POST` 接受 `user` 和 `organization` scope、`daily`/`weekly`/`monthly` period、user scope 中 1–256 字节的 `user_id`，以及 1–19 位非负 USD 美分整数字符串或 `null` 的 `amount`，并按 `(scope, period)` 执行 upsert。列表分页接受 `limit`（1–1000，默认 20）、`after_id`、`before_id` 和 `scope_type`；两个游标不能同时使用。每个响应都包含 `request-id`，错误采用 Anthropic 错误形状。限制与修改审计记录一起保存到所配置的带版本 JSON 状态文件中,每次修改归属于 `admin-key:<id>` 或 `admin-token:<name>` —— 当两个槽位携带同一层级的不同凭据时，归属于配置的管理员头部那一个。stage 1 不公开 `/effective` 或 `/audit`，也不对推理请求实施限制。

`GET /managed/settings` 与 `POST /v1/{metrics,logs,traces}` 遥测接收路由仅在启动时启用了 `[server.gateway]` 的情况下存在,二者要求相同的网关 bearer JWT。接收路由接受托管 Claude Code 客户端 export 的 OTLP/HTTP 载荷([`[server.gateway.telemetry]`](/zh-cn/reference/configuration/) 将这些 exporter 指向网关),并把请求字节原样中继到所有 opt-in 该 signal 的目标。入站的 `content-type` 与 `content-encoding` 会被保留,目标配置的 headers 应用在其上(配置的键会替换转发值,而不是重复该 header)。客户端的 `Authorization` 头永远不会被转发,中继也不跟随重定向。目标按 signal opt-in(`metrics` 默认开启,`logs`/`traces` 默认关闭),没有任何目标 opt-in 的 signal 会被接收后丢弃。中继是分离执行的,因此无论目标状态如何,响应始终是立即的 `200`,成功 body 依照 OTLP/HTTP 镜像请求协议(`application/json` 得到 `{}`,其余得到空的 `application/x-protobuf` body)。超过 32 MiB 入站上限的 body 返回 `413`。

入站 Codex Responses 和分析路由仅在配置了 [`[server.codex_endpoint]`](/zh-cn/reference/configuration/) 时存在。Responses 路由逐字中继 OpenAI Responses 请求和响应。两个分析路由采用相同的入站认证策略，不转发或保留客户端 payload，并在认证后对无效 JSON 或超大正文也返回 `200 {}`。只有净化后的事件名称会记录到 `shunt.codex_client_events`；未配置指标 sink 时，它们是纯丢弃 sink。

即使启用了 [`[server.auth]`](/zh-cn/guides/shared-gateway/),`GET /` 和 `GET /health` 也保持开放(健康检查工具通常无法附带 token),并且不暴露任何敏感信息 —— 只有状态、版本以及已经公开的端点列表。

## 网关协议

shunt 实现官方的 [Claude Code LLM 网关协议](https://code.claude.com/docs/en/llm-gateway-protocol):正确的头部和正文字段转发、特性透传以及系统提示归属处理。网关自身产生的错误以 Anthropic 错误形状返回,上游上下文溢出错误被重写为 Anthropic 的 `prompt is too long` 措辞,以便触发 Claude Code 的 [压缩并重试](/zh-cn/guides/effort-and-context/#context-overflow-recovery),而流式响应无缓冲地中继(带可选的 [keepalive ping](/zh-cn/guides/shared-gateway/#sse-keepalive-pings))。
