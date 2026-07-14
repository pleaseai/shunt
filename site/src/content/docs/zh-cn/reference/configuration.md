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

指定的环境变量必须包含至少一个凭据,例如 `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`。若此表存在但该变量未设置、为空或格式错误,启动会安全失败(fail closed)。被门控的路由(映射的 `/v1/messages` 推理和 `GET /v1/models` 发现)接受 token 出现在配置的头部、`Authorization: Bearer` 或 `x-api-key` 中 —— 当多个槽位携带有效 token 时,专用头部优先。

## `[server.admin]`(可选)

存在此表即启用管理 Web 界面,用于浏览器账户预配与账户池健康状况([详情](/zh-cn/guides/admin-remote-provisioning/))。此表不存在时,任何 `/admin*` 路由都不会注册。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | API/curl 调用中携带管理员 token 的头部 |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | 保存逗号分隔的 `name:token` 对的环境变量 |
| `session_ttl_secs` | `3600` | 登录后浏览器会话的生命周期,单位秒 |
| `pending_ttl_secs` | `600` | 允许完成一个已开始的预配流程的时间,单位秒 |

指定的环境变量必须包含至少一个凭据,例如 `SHUNT_ADMIN_TOKENS="ops:<token>"`。若此表存在但该变量未设置、为空或格式错误,启动会安全失败(fail closed)。

管理员 token 与 `[server.auth]` 下配置的客户端 token 是相互独立的凭据;不要在两个界面上复用同一个凭据。

## `[server.pool]`(可选)

面向 Claude(Anthropic)账户池的配额感知负载均衡调优([详情](/zh-cn/guides/anthropic-multi-account/#调优选择serverpool))。此表不存在时,选择逻辑使用单一的内置 `0.98` 阈值,与该表出现之前的行为完全一致。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | 每个配额窗口的安全兜底;达到或超过它的账户在可用账户中始终排在最后 |
| `default_threshold` | 未设置 | 任何没有更具体取值的窗口的软默认阈值 |
| `default_threshold_5h` | 未设置 | 5 小时窗口的软默认值 |
| `default_threshold_7d` | 未设置 | 共享周(`7d`)窗口的软默认值 |
| `default_threshold_fable` | 未设置 | 仅 fable 的周(`7d_oi`)窗口的软默认值 |
| `burn_rate_avoidance` | `false` | 同时避开按预测会在窗口重置之前耗尽其软阈值的账户 |

对每个窗口 `X`,生效的软阈值按以下顺序解析:账户 `threshold_X` → 账户 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`,并以 `hard_threshold` 为上限。所有阈值都是 `[0.0, 1.0]` 范围内的使用率分数;超出范围会导致启动失败。配额头部只存在于 Anthropic 后端,因此这些旋钮对 Codex/ChatGPT 池不起作用 —— 按账户的 `priority` 和 `disabled` 键(见[账户字段](/zh-cn/guides/anthropic-multi-account/#账户字段))在那里仍然适用。

## `[providers.<name>]`

每个提供方都是一个以你自选名称命名的表。内置项(`anthropic`、`openai`、`codex`、`xai`、`grok`、`cursor`)可被部分覆盖 —— 配置映射深度合并。

| 键 | 取值 | 含义 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` | 上游协议 / 适配器。`anthropic` = Messages API(透传,可选择重新设置密钥);`responses` = Anthropic Messages 转换为 OpenAI Responses API;`cursor` = 原生 Cursor ConnectRPC/protobuf AgentService 适配器。 |
| `base_url` | URL | 上游 base;shunt 追加端点路径。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough` 转发客户端自己的 credential;`api_key` 从 `api_key_env` 注入一个密钥;`chatgpt_oauth` 复用 `~/.codex/auth.json`;`claude_oauth` 从显式 Anthropic 账户中选择;`xai_oauth` 复用来自 `shunt login xai` 的 `~/.shunt/xai-auth.json`(仅经由 HTTPS 发送到 x.ai/grok.com 主机);`cursor_oauth` 复用 `~/.shunt/cursor-auth.json`(`shunt login cursor`)。 |
| `api_key_env` | 环境变量名 | 当 `auth = "api_key"` 时,从何处读取密钥。 |
| `api_key_header` | `bearer`(默认) \| `x_api_key` | 注入的密钥在哪个头部中发送。 |
| `effort` | `low` … `max` | 可选的默认推理力度(`responses` 提供方)。 |
| `count_tokens` | `tiktoken`(默认) \| `estimate` | `responses` 与 `cursor` provider:本地 tiktoken 计数 vs. `501 not_supported` 回退([详情](/zh-cn/guides/effort-and-context/#token-counting-count_tokens))。 |

只带名称的条目读取 `~/.shunt/accounts/claude/<name>.json`,该文件由 `shunt login claude --name <name> --mode oauth|import|setup-token` 创建。交互式 CLI 会提示选择这三种 mode,并推荐可刷新的 OAuth。`--long-lived` 保留为 `--mode setup-token` 的 deprecated alias。`SHUNT_CLAUDE_ACCOUNTS_DIR` 可覆盖存储目录。可刷新的 OAuth/import 文件会在 provider 轮换 refresh token 时原地更新,因此每个文件只能有一个正在运行的 owner。不要在多个 shunt 进程之间共享或独立复制该文件。请为每个进程分别预配,或在适合时使用静态 setup token。

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

## `[sentry]`(可选)

可选启用的错误上报,发送到你自己的 Sentry 项目。未设置 `dsn` 时关闭;与 `[otel]` 相互独立。只上报网关自身的诊断信息 — 致命的网关启动/服务错误、panic 和 `error` 级日志事件(`warn`/`info` 作为 breadcrumb,仅含消息);请求/响应正文、头部和凭证永远不会发送。指标和 tracing 各自是进一步的独立可选项。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `dsn` | — | Sentry 项目 DSN。留空则关闭;无效 DSN 为启动错误。 |
| `environment` | — | 上报事件上的可选 environment 标签 |
| `metrics` | `false` | 同时发送用量指标 — `shunt.requests` / `shunt.latency` 序列(仅聚合值) |
| `traces_sample_rate` | `0.0` | 同时发送性能 trace:每个请求的 span 成为一个 Sentry 事务,按 `[0.0, 1.0]` 范围内的该比率做头部采样。`0.0` 完全不发送 span;超出范围为启动错误。 |
| `include_session_id` | `false` | 在发送给 Sentry 的请求 span 上附加客户端会话 id |

## `[otel]`(可选)

可选启用的 OpenTelemetry(OTLP/HTTP)导出,将 trace、指标与日志发送到你自己的 collector([详情](/zh-cn/guides/opentelemetry/))。未设置 `endpoint` 时关闭;与 Sentry 相互独立。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `endpoint` | — | OTLP/HTTP 基础 URL(例如 `http://localhost:4318`);shunt 会追加 `/v1/{traces,metrics,logs}`。留空则关闭;非 `http(s)` 的 URL 为启动错误。 |
| `service_name` | `shunt` | `service.name` 资源属性(优先于 `OTEL_SERVICE_NAME`) |
| `environment` | — | 可选:`deployment.environment.name` |
| `sample_ratio` | `1.0` | `[0.0, 1.0]` 范围内基于 head 的 trace 采样;超出范围为启动错误 |
| `traces` | `true` | 导出每次请求的 `proxy_request` span |
| `metrics` | `true` | 导出 `shunt.requests` / `shunt.latency` 序列 |
| `logs` | `true` | 导出 `tracing` 日志事件(stderr 日志不受影响) |
| `include_session_id` | `false` | 将客户端 session id 附加到请求 span |

## `[otel.headers]`(可选)

附加到每个 OTLP 请求的 header(例如托管 collector 的令牌)。会合并到标准 `OTEL_EXPORTER_OTLP_HEADERS` 之下。

| 键 | 含义 |
| :-- | :-- |
| 任意 | header 名称 → 值,例如 `authorization = "Bearer <token>"` |

## 路由优先级

精确 `[[routes]]` 匹配 → `[[route_prefixes]]` 前缀匹配 → `server.default_provider`。
