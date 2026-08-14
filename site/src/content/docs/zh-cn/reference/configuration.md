---
title: 配置参考
description: 每一个 shunt.toml 键 —— server、providers、routes、models。
---

关于文件位置、优先级以及带注释的示例,见 [配置](/zh-cn/guides/configuration/)。完整模板:[`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)。

## Secret 引用

配置文件中的字符串值可以写成 `${VAR}` 或 `${file:/绝对/路径}` 而不是字面量。`${VAR}` 会替换为环境变量 `VAR` 的值,可以嵌入更长的字符串中,例如 `"Bearer ${TOKEN}"`(若变量未定义,配置加载会失败)。`${file:/绝对/路径}` 会替换为该文件的内容(已 trim),路径必须是绝对路径,且引用必须是该字段的整个值 —— 不能嵌入更长的字符串中(若文件不可读、路径是相对路径,或引用嵌入在更长的字符串中,配置加载会失败)。`$${` 会转义为字面量 `${`。解析不是递归的 —— 解析出的值不会被再次扫描。此替换仅适用于配置文件,`SHUNT_*` 环境变量覆盖会按原样使用。它会在每次配置加载时重新执行,包括启动、`shunt check` 以及[热重载](https://github.com/pleaseai/shunt/blob/main/docs/config-reload.md)(SIGHUP 与文件监视),因此以 `${file:}` 引用的 secret 可以通过重写所引用的文件并触发一次重载来轮换,无需重启 shunt。但轮换后的值是否生效取决于该字段自身的重载行为:`[sentry]` 和 `[otel]` 只在启动时初始化一次,因此轮换这两个部分中的 secret 只会更新配置,需要重启才能生效。

`[sentry] dsn`、`[otel.headers]` 的值,以及 `[server.gateway.telemetry] forward_to[].headers` 的值这三个字段被标记为 redacting secret 类型,在诊断输出中显示为 `[redacted]`。在这些字段中写入字面量值的行为与之前完全相同;若某个 secret 类型字段持有字面量值,shunt 会在启动时记录一次仅列出相关字段路径(绝不包含值)的建议性警告,提示改用 `${VAR}` / `${file:...}`。

现有的 `tokens_env`、`jwt_secret_env`、`client_secret_env`、`api_key_env`、`users_env`、`token_env`、`tokens_file` 字段不受此变更影响,仍然指向一个环境变量(对 `tokens_file` 而言是文件路径)。

## `[server]`

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt 监听的地址 |
| `default_provider` | `anthropic` | 面向任何无匹配路由的模型的提供方 |
| `max_concurrent_requests` | `1024` | 入站并发请求上限，请求会一直计数到响应正文结束。超出上限的请求不会排队，而是立即以 `503` 和 `Retry-After: 1` 拒绝。`0` 表示禁用限制，`/` 和 `/health` 不受限制。更改此键后需要重启 |
| `sse_keepalive_seconds` | `30` | 注入 SSE `ping` 前的闲置秒数;`0` 禁用([详情](/zh-cn/guides/shared-gateway/#sse-keepalive-pings)) |

## HTTP 调优表

`[server.access_control]` 提供 `allow_cidrs = []`、`deny_cidrs = []` 和 `trust_forwarded_for = false`。deny 规则优先，并且也适用于 `/` 和 `/health`。非空 allow 列表会启用默认拒绝，但这两个健康检查路径只免除 allow 检查。仅在可信代理会覆盖客户端提供的转发头时才信任这些头。更改后需要重启。

此 `trust_forwarded_for` 设置与 `[server.gateway] trust_forwarded_for` 相互独立。access-control 设置仅影响 CIDR 允许/拒绝规则，gateway 设置仅影响设备流速率限制器。如果两个表面都在可信反向代理后运行，请同时启用这两个设置。只设置其中一个时，另一个表面仍会使用套接字对端地址。

`[server.limits]` 的 `max_request_bytes` 适用于 Anthropic Messages 和入站 Codex Responses 请求正文，默认为 `33554432`（32 MiB），超出时返回 `413`。其他网关、管理、遥测和分析路由仍使用各端点自身的正文限制。`max_request_header_bytes` 和 `max_url_length` 默认未设置，分别返回 `431` 和 `414`。头部大小是所有已解析头部名称长度与值长度之和。正文限制可热重载，头部和 URL 限制需要重启。

`[server.timeouts] upstream_ttfb_ms` 默认为 `120000`，设为 `0` 可禁用。它只限制等待推理上游 HTTP 响应头的时间，因此不会对响应正文和长时间 SSE 流施加总时限。覆盖 Anthropic Messages、OpenAI Responses HTTP（包括 WebSocket 回退）、Gemini HTTP 和入站 Codex Responses 透传；不覆盖 Codex WebSocket、Cursor、Antigravity 或辅助 HTTP 请求。

`[server.rate_limits.device_authorization]` 默认为 `max = 30`、`window_seconds = 600`，`[server.rate_limits.device_verify]` 默认为 `max = 10`、`window_seconds = 600`。两个 per-IP 限制彼此独立；未配置 `[server.gateway]` 时不生效。更改后需要重启。

## `[server.auth]`(可选)

存在此表即启用入站客户端 token 认证([详情](/zh-cn/guides/shared-gateway/)):

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | 携带客户端 token 的头部 |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | 保存逗号分隔的 `name:token` 对的环境变量 |

指定的环境变量必须包含至少一个凭据,例如 `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`。若此表存在但该变量未设置、为空或格式错误,启动会安全失败(fail closed)。被门控的路由(映射的 `/v1/messages` 推理和 `GET /v1/models` 发现)接受 token 出现在配置的头部、`Authorization: Bearer` 或 `x-api-key` 中 —— 当多个槽位携带有效 token 时,专用头部优先。

`tokens_env` 自身的值也和其他配置文件字符串一样,可以写成 `${VAR}` / `${file:...}`(见 [Secret 引用](#secret-引用))——它仍然指向 shunt 用来读取 token 的环境变量。

### `[[server.auth.jwt]]`(可选)

一个表数组,增加第二种入站凭据:由外部 identity provider 签发的 JWT,shunt **只验证、从不签发** —— 没有登录流程、没有会话存储、没有签名 secret。客户端的配置方式与静态 token 完全相同(base URL 加 bearer),因此不同于 `[server.gateway]` 登录,它不会让 Claude Code 进入 gateway provider 模式,也不会失去该模式禁用的客户端功能。参见[行为规范](https://github.com/pleaseai/shunt/blob/main/docs/inbound-jwt-auth.md)。

```toml
[server.auth]
tokens_env = "SHUNT_CLIENT_TOKENS"   # 配置了 JWT issuer 后即为可选

[[server.auth.jwt]]
issuer = "https://accounts.google.com"
audience = ["<专供 shunt 使用的 client id>"]
email_domains = ["example.com"]
```

| 键 | 默认值 | 含义 |
| :-- | :-- | :-- |
| `issuer` | 必填 | `iss` 精确匹配。必须使用 HTTPS,仅回环地址允许 HTTP;可以带路径 |
| `audience` | 必填 | 接受的 `aud` 值,字符串或数组 |
| `email_domains` | `[]` | 不区分大小写的域名,与最后一个 `@` 之后的部分**精确**匹配,不是后缀匹配 |
| `allowed_emails` | `[]` | 不区分大小写的完整邮箱地址 |
| `algorithms` | `["RS256"]` | 接受的签名算法,仅限非对称算法 |
| `authorized_parties` | `audience` | `azp` claim 存在时接受的值 |
| `clock_skew_seconds` | `0` | 应用于 `exp` 和 `nbf` 的容差 |
| `max_token_age_seconds` | `3600` | `exp - iat` 超过该值时拒绝 |
| `jwks_url` | discovery | 为不提供 discovery 文档的 issuer 显式指定 JWKS 端点 |

每个条目都需要 `issuer`、`audience`,以及至少一个 `email_domains` 或 `allowed_emails` 值。仅有 `audience` 不构成授权规则 —— 对某些 issuer 来说,调用方可以自行选择 audience。如果 `algorithms` 含有对称算法或未知值,或者 `issuer` 与 `[server.gateway]` 的 `public_url` 相同,启动同样会 fail closed:两个验证器读取同一个 bearer 槽位,issuer 相同会让 token 的归属取决于求值顺序。

只要存在至少一个条目,`tokens_env` 就可以解析为空 —— 完全通过 IdP 认证的部署没有静态 token 需要设置。既没有可解析的 token 列表也没有条目时,`[server.auth]` 仍然启动失败。

JWT **只**通过 `Authorization: Bearer` 接受 —— 不接受配置的 `header`,也不接受 `x-api-key`。这正是 Claude Code 已经发送 `ANTHROPIC_AUTH_TOKEN` 的槽位。在受控路由上先检查静态 token,该路由接受时再检查 gateway JWT,然后按 `iss` 检查 JWT 条目。验证会从 config 固定算法(token header 里的 `alg` 永远不参与选择)、要求 `kid`,并检查 `exp`/`nbf`、`aud`、`azp`、`exp - iat`、`email_verified = true` 和邮箱允许列表。验证通过的邮箱成为日志和用量归属中的调用方身份,上限 256 字节。

key set 按 issuer 在首次使用时惰性拉取,因此 IdP 不可达不会阻塞启动,并在进程生命周期内缓存 —— config reload 只重新解析条目,不会丢弃密钥。未知的 `kid` 在每个 issuer 的 60 秒窗口内最多触发一次重新拉取。没有任何条目能验证的 token 返回 `401`;完全拉不到 key set 的 issuer 返回 **`503`**,这样 IdP 故障就不会被报告成凭据错误。

shunt 不保存吊销状态,所以有效的 token 会一直可用到过期:限制这一范围的是 `max_token_age_seconds`,issuer 应当签发分钟级寿命的 token。从允许列表中移除域名或地址会在下一次请求时生效。

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

和 `[server.auth]` 的 `tokens_env` 一样,这个 `tokens_env` 的值也可以写成 `${VAR}` / `${file:...}`(见 [Secret 引用](#secret-引用))。

## `[server.gateway]`(可选)

存在此表即启用 Claude Code managed `forceLoginMethod: "gateway"` 使用的 [OAuth device-flow gateway 登录](/zh-cn/guides/gateway-login/)。此表不存在时,shunt 不会注册 `/.well-known/oauth-authorization-server`、`/oauth/device_authorization`、`/oauth/token`、`/device` 或 `/managed/settings`。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `public_url` | 必需 | 对外可达的 HTTPS origin,用作 JWT issuer 和 OAuth endpoint 基址;仅 loopback 允许 `http` |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | 保存至少 32 bytes 的 HS256 signing secret 的 env 变量 |
| `users_env` | `SHUNT_GATEWAY_USERS` | 保存逗号分隔的 `email:secret` approval user 的 env 变量 |
| `token_ttl_seconds` | `3600` | access token 生命周期,以 `expires_in` 返回 |
| `trust_forwarded_for` | `false` | 将 `X-Forwarded-For`/`X-Real-IP` 信任为 `/device` rate-limit identity;只能在会替换 client 所提供值的 trusted proxy 后启用 |

如果 URL 不是不带路径等内容的 HTTPS origin(仅 loopback 允许 `http`)、TTL 为 0、secret 缺失或少于 32 bytes,或者 user list 为空或格式错误,启动会 fail closed。secret 可以包含 `:`,只有第一个 colon 用于分隔 email 与 secret。`jwt_secret_env` 和 `users_env` 的值也和其他配置文件字符串一样,可以写成 `${VAR}` / `${file:...}`(见 [Secret 引用](#secret-引用))。env-backed secret 和 user 的变更会在 config reload 时生效;由于 route tree 在 boot 时固定,添加或移除此表需要 restart。

颁发的 bearer 会在所选 provider 注入 server-side credential 时认证 `/v1/models`、`/v1/messages` 和 `/v1/messages/count_tokens`。passthrough provider 仍保持 open。如果还存在 `[server.auth]`,任一 credential 都能授权访问。device grant 和 rotating refresh token 是 process-lifetime in-memory state:config reload 会保留它们,但 restart 会使其失效。

### `[[server.gateway.policies]]`(可选)

存在 `[server.gateway]` 即会注册经过认证的 `GET /managed/settings`;有序且非空的 policy list 为其提供 managed document。每条 policy 都有可选的 `[server.gateway.policies.match]` 和必需的 open-schema `[server.gateway.policies.cli]` object。省略 `match`、使用 `match = {}` 或不设置 `emails` 都表示 catch-all。显式空 `emails` list 或空白 entry 会导致启动错误。

所有 catch-all policy 按顺序 merge,然后在其上 merge 第一个 email 精确匹配(case-sensitive)的 policy。object 递归 merge;array 通常替换,但 key 包含 `deny` 的 array 会做无重复 union。已知 key 会在启动和 hot reload 时验证:`availableModels` 必须是仅包含 string 的 array;`env` 必须是仅包含 string、number 或 boolean scalar value 的 table。未知 key 保持 open-schema,但所有 value 都必须可用 JSON 表示;非有限 float 会被拒绝。

没有 `policies` 时 endpoint 返回 `404`。已配置 policy 但没有匹配的 user-specific 或 catch-all settings 时,若 telemetry 已启用则返回带有仅 telemetry `settings.env` 的 `200`,否则返回带有 `settings: {}` 的 `200`。response 包含 `uuid`、`checksum` 和保存 checksum 的 quoted `ETag`;匹配的 `If-None-Match` 返回 `304`。

解析后的 `cli.availableModels` 会应用于 gateway JWT request 的 `/v1/messages` 和 `/v1/messages/count_tokens`。比较前会从 top-level `model` 移除一个末尾 Claude Code context-window hint（`[1m]` 或 `[1M]`）;若剩余 model 不在 list 中,则返回 `400 invalid_request_error`。static `[server.auth]` credential 无法标识 gateway policy user,因此不受此限制。

### `[server.gateway.telemetry]`(可选)

`forward_to` 是 destination array,每项具有必需的 base OTLP/HTTP `url`、可选的 string `headers` map,以及每个 signal 的 opt-in boolean(`metrics` 默认 `true`,`logs`/`traces` 默认 `false`)。至少 opt-in 一个 signal 的 list 会向 managed `settings.env` 注入 6 个值:`CLAUDE_CODE_ENABLE_TELEMETRY=1`、每个 `OTEL_METRICS_EXPORTER`/`OTEL_LOGS_EXPORTER`/`OTEL_TRACES_EXPORTER` 在有 destination opt-in 该 signal 时为 `otlp`,否则为 `none`、`OTEL_EXPORTER_OTLP_ENDPOINT=public_url`、`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`。若没有任何 signal 被 opt-in,则不注入任何值。发生冲突时 policy env value 优先。同一 list 也驱动 inbound ingest(M-C,#189):只要存在 `[server.gateway]` 就会注册的 `POST /v1/{metrics,logs,traces}` route 接收客户端 export 的 OTLP payload,并将其 verbatim relay 到所有 opt-in 该 signal 的 destination;没有任何 destination opt-in 的 signal 会被接收后丢弃。`logs`/`traces` 默认关闭,因为 Claude Code 的 log record 和 span 可能携带 command line、prompt 和文件路径。`headers` 的每个值都是 redacting secret 类型,在诊断输出中显示为 `[redacted]`(见 [Secret 引用](#secret-引用))。

```toml
[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
headers = { "x-api-key" = "..." }
```

默认情况下,`/device` 忽略 forwarding header 并按 socket peer 做 rate limit。只有在 shunt 仅能通过会删除 client 所提供 forwarding header 并设置自身值的 trusted reverse proxy 访问时,才设置 `trust_forwarded_for = true`。不要在直接暴露的 gateway 上启用。

## `[server.pool]`(可选)

面向账户池的配额感知负载均衡调优 —— Claude(Anthropic)([详情](/zh-cn/guides/anthropic-multi-account/#调优选择serverpool)),以及自 issue #195 起的 Codex/ChatGPT([详情](/zh-cn/guides/codex-multi-account/))。此表不存在时,选择逻辑使用单一的内置 `0.98` 阈值,与该表出现之前的行为完全一致。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | 每个配额窗口的安全兜底;达到或超过它的账户在可用账户中始终排在最后 |
| `default_threshold` | 未设置 | 任何没有更具体取值的窗口的软默认阈值 |
| `default_threshold_5h` | 未设置 | 5 小时窗口的软默认值 |
| `default_threshold_7d` | 未设置 | 共享周(`7d`)窗口的软默认值 |
| `default_threshold_fable` | 未设置 | 仅 fable 的周(`7d_oi`)窗口的软默认值 |
| `burn_rate_avoidance` | `false` | 同时避开按预测会在窗口重置之前耗尽其软阈值的账户 |
| `usage_refresh_seconds` | 禁用(`0`/未设置) | `GET /api/oauth/usage` 的轮询间隔(秒);低于 60 的正值会向上取到 60 秒下限 |
| `state_path` | 未设置 | 用于持久化池中按账户配额状态的文件;重启时从最后观测到的使用率热启动,而非从空池开始。未设置则禁用持久化(默认) |
| `ramp_initial_concurrency` | 禁用(`0`/未设置) | 风暴控制:对刚开始承接流量的账户身份的初始并发准入额度。`0` 或未设置则禁用准入门控 |

对每个窗口 `X`,生效的软阈值按以下顺序解析:账户 `threshold_X` → 账户 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`,并以 `hard_threshold` 为上限。所有阈值都是 `[0.0, 1.0]` 范围内的使用率分数;超出范围会导致启动失败。阈值与 burn-rate 旋钮对两个池家族都生效:Anthropic 池取自其 `anthropic-ratelimit-unified-*` 头部,Codex/ChatGPT 池取自其 `x-codex-*` 5 小时/周窗口(Codex 没有 Fable 范围的 `7d_oi` 窗口,因此 `default_threshold_fable` 在那里不起作用)。`usage_refresh_seconds` 仅限 Anthropic —— Codex 没有带外 usage API。

正的 `usage_refresh_seconds` 还会启动一个后台轮询器,把 Claude 账户池的配额状态与 Anthropic OAuth usage API 对账校正;未设置或为 `0` 时禁用(默认)。只有 imported(可刷新)的 `claude_oauth` 账户会被轮询 —— 长期 `claude setup-token` 或 `token_env` 账户会被跳过,因为 usage 端点会拒绝不可刷新的令牌。轮询器会把基于头部的 5h/周/Fable(`7d_oi`)配额状态,与包含 shunt 之外同一账户消耗在内的权威用量对账。间隔在启动时固定;配置重载不会启动、停止或重新调整轮询器。

`state_path` 会把池的配额状态(所有 provider 账户的按窗口使用率与重置)写入磁盘。不设置时,重启会从空池开始:每个账户在重启后首个响应之前都显示为未观测,这会禁用 burn-rate 规避,并使 `GET /usage` 在流量重新填充池之前返回空值。该文件是尽力而为的缓存,而非权威来源 —— 配额无论如何都会从上游响应重新导出,因此文件缺失、陈旧或损坏只会导致冷启动,绝不会导致启动失败。写入使用私有 temp 文件(Unix 上为 `0600`)并将其原子重命名覆盖目标,且仅在配额发生变化时按后台定时器进行。写入失败时会在下一个 tick 重试。冷却不会被持久化(重启即失效),恢复的窗口中重置已过期的会在恢复后的首次选择或 snapshot 时延迟丢弃。路径在启动时固定;配置重载不会启动、停止或改变持久化路径。

正的 `ramp_initial_concurrency` 会在每个账户池上启用**风暴控制(storm control)**:一次故障转移切换之后,在途的并发请求本会全部同时落到刚选中的账户上。开启该门控后,刚开始承接流量的身份(全新、刚从冷却回来,或空闲 60 秒)最多准入所配置数量的并发请求;每次成功响应把额度翻倍(slow start),一次达到故障转移条件的失败会重启该 ramp,被拒绝的请求则顺延到选择顺序中的下一个账户。无论门控如何,最后一个候选始终会被尝试,因此门控只能推迟、而绝不会失败一个未门控的池本会服务的请求。这也意味着,若池中所有账户都解析到同一个上游身份,则该池实际上不受门控:唯一的候选同时也是最后一个候选,因此该设置仅在存在两个及以上不同账户身份时才生效。

## `[[upstreams]]`（有序故障转移）

`[[upstreams]]` 是命名上游的有序数组。声明顺序就是全局故障转移顺序；模型的 `[models.upstream_model]` 映射选择哪些条目参与。映射中的书写顺序不影响路由。

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "kimi-overflow"
provider = "kimi"

[[upstreams]]
name = "codex-fallback"
provider = "codex"

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
kimi-overflow = "kimi-k2"
codex-fallback = "gpt-5.2"
```

此示例依次尝试 `anthropic-primary`、`kimi-overflow`、`codex-fallback`。模型映射中未列出的上游不会参与。

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `name` | 是 | 非空且唯一的上游名称。路由、模型映射、`server.default_provider`、指标和管理界面都使用它。 |
| `provider` | 未设置 `kind` + `base_url` 时 | 内置 preset。提供 `kind`、`base_url` 和默认 auth。显式字段覆盖 preset 值。 |
| `kind` | 无 preset 时 | `anthropic`、`responses`、`cursor`、`gemini` 或 `antigravity`。后两者在下方 preset 表中没有条目(同名的内置 `[providers.*]` 表是另一套遗留机制,并非 preset),因此有序 upstream 必须显式设置 `kind`。 |
| `base_url` | 无 preset 时 | 上游 base URL。对于 `kind = "cursor"`，它仅用于登录/令牌刷新接口；推理使用固定的代理主机 `https://agentn.global.api5.cursor.sh`，且只能通过 `SHUNT_CURSOR_AGENT_BASE_URL` 覆盖。 |
| `auth` | 否 | auth mode 字符串或特定于 mode 的映射。默认采用 preset 的 auth；没有 preset 时为 `passthrough`。 |
| `effort`, `count_tokens`, `websocket`, `tool_search`, `request_compression`, `retry` | 否 | 与旧式 provider 相同的按上游设置。preset 不会覆盖 `count_tokens`。Cursor 上游的 `retry` 也会被标准化，但不适用于 Cursor 流式推理请求。 |

可用 preset 如下：

| Preset | Kind | Base URL | 默认 auth |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `https://api.anthropic.com` | `passthrough` |
| `codex` | `responses` | `https://chatgpt.com/backend-api` | `chatgpt_oauth` |
| `openai` | `responses` | `https://api.openai.com/v1` | `api_key`, env `OPENAI_API_KEY` |
| `xai` | `responses` | `https://api.x.ai/v1` | `api_key`, env `XAI_API_KEY` |
| `grok` | `responses` | `https://cli-chat-proxy.grok.com/v1` | `xai_oauth` |
| `kimi` | `anthropic` | `https://api.moonshot.ai/anthropic` | `api_key`, env `MOONSHOT_API_KEY` |
| `cursor` | `cursor` | `https://api2.cursor.sh` | `cursor_oauth` |

`auth = "claude_oauth"` 这样的字符串是 `auth = { mode = "claude_oauth" }` 的简写。`api_key` 映射接受 `env`（除非 preset 已提供，否则必需）和 `header`（默认为 `bearer`，也可设为 `x_api_key`）。`claude_oauth` 与 `chatgpt_oauth` 映射可用 `account = "name"` 或 `accounts = [...]` 缩小范围，但不能同时设置两者。`accounts` 接受存储条目名称字符串和完整账户表；显式的 `accounts = []` 会被拒绝，而省略两个范围字段则扫描整个存储。若 ChatGPT 存储为空，`chatgpt_oauth` 仍会回退到 `~/.codex/auth.json`。`passthrough`、`xai_oauth`、`cursor_oauth` 映射只接受 `mode`；特定 mode 下的未知键会报错。

不要在配置文件中同时声明 `[[upstreams]]` 与 `[providers.*]`：文件层同时存在这两种声明形式时，启动会失败。无论采用哪种形式，环境变量都可按标准化后的上游/provider 名称通过 `SHUNT_PROVIDERS__<name>__<field>` 覆盖单个字段。有序的 `[[upstreams]]` 数组本身应在配置文件中声明，不要试图用单个环境变量合成整个数组。旧式 `[providers.<name>]` 仍受支持，并会标准化为按名称排序的隐式上游。由于这种形式没有声明故障转移顺序，模型映射只能有零个或一个条目；向模型映射添加多个条目前，请迁移到 `[[upstreams]]`。

### 故障转移行为

对于多条目的模型映射，shunt 从声明的上游序列中筛出映射内的名称来构建链。当上游状态为 `429`、`401`、`403`、`404`、任意 `5xx`，或者在收到上游响应头之前失败时，会前进到下一条目。auth 配置错误、适配器自身的校验或头部构建错误等不代表上游尝试的网关本地错误会立即返回，使错误配置不会被故障转移掩盖。返回 `2xx` 响应头之后不再故障转移，即使后续流式正文失败也是如此。

链耗尽时，shunt 按 `429` → `401`/`403` → `404` → 其他 `5xx` 的优先级返回最佳的已中继失败。响应头之前的失败不会被记为最佳失败。若没有记住任何已中继响应，则返回消息为 `all upstreams failed (N attempted)` 的 `502 api_error`。

对于 `passthrough` 上游，客户端自己的 `authorization` / `x-api-key` 仅在故障转移尝试中当**主**路由自身为 `passthrough` 且该尝试的目标来源(origin)与该主路由一致时才转发。此时该凭据是客户端自己的上游凭据、对主路由而言是来源专属的，因此对**不同**来源的 `passthrough` 故障转移尝试会将其剥离并快速失败(fail closed)，而不会把主机专属令牌重放到另一个来源；同一来源的回退(例如同一主机上的两个 passthrough 条目)仍会携带该凭据。当主路由改为注入自己的凭据时，客户端头部是网关/客户端密钥而非上游凭据，因此每个 `passthrough` 回退无论来源如何都会将其剥离。`api_key`/OAuth 上游无论位置如何都会注入自己的服务端凭据。

每个代理成功响应或最终失败都带有 `x-gateway-upstream`（所选上游名称）、`x-gateway-model`（客户端请求的 id）和 `x-gateway-upstream-model`（映射后的后端 id）。`count_tokens` 只使用链中第一个条目，且不会故障转移。`[server.codex_endpoint]` 仍固定到所配置的单一上游，不参与此链。

### 迁移现有配置

现有配置**无需更改**。旧式 provider 会保留原有路由及按名称排序的选择行为。升级时有以下三项新增或有意的行为变化：

1. 解析到同一物理 OAuth 账户的旧式 provider 现在会共享配额窗口、health、cooldown、refresh lock 和 in-flight admission 状态。池持久化键的 schema 已提升版本，因此现有 `state_path` 缓存会被忽略一次，池会经历一次冷启动。
2. 每个代理响应都会新增上述三个 `x-gateway-*` metadata 头部。
3. 在 Anthropic Messages 路由（`/v1/messages`）上，无论 Claude 或 Codex OAuth 池的规模如何，若所有尝试都在响应头之前失败，现在都会返回 `all upstreams failed (N attempted)`，而不是该池专用的 `all Claude OAuth accounts failed before receiving an upstream response` 或 `all Codex OAuth accounts failed before receiving an upstream response`。单独的 `[server.codex_endpoint]` 入站路径不受影响，并保留 Codex 专用消息。

要采用有序故障转移，请把每个 `[providers.<name>]` 表改写为同名 `[[upstreams]]` 条目，把 `api_key_env`、`api_key_header` 和 OAuth `accounts` 折入 `auth` 映射，按偏好顺序排列条目，然后把每个参与名称加入模型的 `upstream_model` 映射。

`kimi` preset 读取 `MOONSHOT_API_KEY`。显式使用 `api_key_env = "KIMI_API_KEY"` 的旧示例在旧式形式中仍然有效；在上游形式中也可用 `auth = { mode = "api_key", env = "KIMI_API_KEY" }` 保留该名称。只有依赖 preset 默认值的用户才需要 export `MOONSHOT_API_KEY`。

## `[providers.<name>]`（旧式）

每个提供方都是一个以你自选名称命名的表。内置项(`anthropic`、`openai`、`codex`、`xai`、`grok`、`cursor`、`gemini`、`antigravity`)可被部分覆盖 —— 配置映射深度合并。

| 键 | 取值 | 含义 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` \| `gemini` \| `antigravity` | 上游协议 / 适配器。`anthropic` = Messages API(透传,可选择重新设置密钥);`responses` = Anthropic Messages 转换为 OpenAI Responses API;`cursor` = 原生 Cursor ConnectRPC/protobuf AgentService 适配器;`gemini` = Anthropic Messages 转换为 Google Code Assist 后端的 Gemini `generateContent`/`streamGenerateContent`;`antigravity` = 没有任何上游,以子进程方式运行本地 Antigravity CLI 二进制(`agy`)。 |
| `base_url` | URL | 上游 base；shunt 追加端点路径。对于 `kind = "cursor"`，它仅用于登录/令牌刷新接口，不会选择代理/推理主机。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` \| `google_oauth` \| `none` | `passthrough` 转发客户端自己的 credential;`api_key` 从 `api_key_env` 注入一个密钥;`chatgpt_oauth` 复用 `~/.codex/auth.json`;`claude_oauth` 从显式 Anthropic 账户中选择;`xai_oauth` 复用来自 `shunt login xai` 的 `~/.shunt/xai-auth.json`(仅经由 HTTPS 发送到 x.ai/grok.com 主机);`cursor_oauth` 复用 `~/.shunt/cursor-auth.json`(`shunt login cursor`);`google_oauth` 复用 gemini CLI 登录的 `~/.gemini/oauth_creds.json`,仅在 `kind = "gemini"` 下有效;`none` 完全不发送 credential,用于没有上游需要认证的适配器(`kind = "antigravity"`)。 |
| `api_key_env` | 环境变量名 | 当 `auth = "api_key"` 时,从何处读取密钥。该值自身也可以写成 `${VAR}` / `${file:...}`(见 [Secret 引用](#secret-引用))。 |
| `api_key_header` | `bearer`(默认) \| `x_api_key` | 注入的密钥在哪个头部中发送。 |
| `effort` | `low` … `max` | 可选的默认推理力度(`responses` 提供方)。 |
| `count_tokens` | `tiktoken`(默认) \| `estimate` | `responses` 与 `cursor` provider:本地 tiktoken 计数 vs. `501 not_supported` 回退([详情](/zh-cn/guides/effort-and-context/#token-counting-count_tokens))。 |
| `tool_search` | 未设置("auto",默认) \| `true` \| `false` | 在模型为 GPT-5.4+ 且风格不是 xAI/Grok 时,为 Claude Code 的工具搜索使用原生的客户端执行 `tool_search` 协议。未设置时仅对已验证支持的主机 —— ChatGPT/Codex 后端与 `api.openai.com` —— 默认使用原生协议,LiteLLM、vLLM、OpenRouter、自托管代理等其他所有 OpenAI 兼容端点都保留文本 shim。设为 `true` 可让已验证的自定义端点选择加入原生协议;设为 `false` 则始终强制使用 shim。见 [Codex → 工具搜索](/zh-cn/guides/codex/#原生协议)。 |

只带名称的条目读取 `~/.shunt/accounts/claude/<name>.json`,该文件由 `shunt login claude --name <name> --mode oauth|import|setup-token` 创建。交互式 CLI 会提示选择这三种 mode,并推荐可刷新的 OAuth。`--long-lived` 保留为 `--mode setup-token` 的 deprecated alias。`SHUNT_CLAUDE_ACCOUNTS_DIR` 可覆盖存储目录。可刷新的 OAuth/import 文件会在 provider 轮换 refresh token 时原地更新,因此每个文件只能有一个正在运行的 owner。不要在多个 shunt 进程之间共享或独立复制该文件。请为每个进程分别预配,或在适合时使用静态 setup token。

## `[[routes]]`

旧式的精确匹配路由条目 —— 在匹配的 `[models.upstream_model]` 条目之后检查:

> **旧式:** 对于精确模型 id,建议使用 `[[models]]` 条目和 `[models.upstream_model]`;它能以单一事实来源同时路由并公开该 id。`[[routes]]` 将继续获得支持,但不再是推荐的精确路由形式。

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `model` | ✅ | Claude Code 发送的精确 `model` id |
| `provider` | ✅ | 已配置的上游名称 |
| `upstream_model` | — | 重写转发给上游的模型 id |
| `effort` | — | 按路由的推理力度覆盖 |

## `[[route_prefixes]]`

前缀匹配的路由条目 —— 在精确路由之后检查:

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `prefix` | ✅ | 模型 id 前缀,如 `gpt-` |
| `provider` | ✅ | 已配置的上游名称 |

## `[[models]]`

由 `GET /v1/models` 为 [模型发现](/zh-cn/guides/model-discovery/) 返回的条目。id 必须以 `claude` 或 `anthropic` 开头,否则 Claude Code 会忽略它们。

顶层 `auto_include_builtin_models` 键默认为 `true`。启用后,shunt 会先返回管理员维护的 `[[models]]` 条目,再追加它自行发现的模型。对于 id 完全相同的条目,会保留管理员维护的条目并去重。若只想公开 `[[models]]` 列表,请将其设为 `false` —— 这也会一并关闭下述的上游调用。

发现的模型在 shunt 能取到实际上游列表时来自该列表。仅当 `server.default_provider` 为 Anthropic 类型时,它才会对该上游发起 `GET /v1/models`,并按其认证模式选择凭据。`auth = "passthrough"` 时使用调用方转发的凭据,因此每个调用方看到的都是该凭据有权使用的列表。如果 shunt 已将该凭据用作 `[server.auth]` 客户端 token 或 gateway-login bearer,则不会将其重放到上游。此时,发现会记录没有剩余上游凭据的原因并回退到快照。`api_key` 时使用配置的密钥。`claude_oauth` 时使用推理路径所用的同一有效账户集合中第一个可解析且未禁用的账户。该集合包含从存储中扫描到的账户,并遵循 `account_scope` 顺序。发现不会进行账户池选择、冷却或配额记账。因此,后两种使用网关自有凭据的模式下,所有调用方共享由该凭据范围决定的目录。shunt 不做缓存。若 `server.default_provider` 不是 Anthropic 类型、没有凭据,或调用失败、超时(上限 2 秒),则回退到内置 Claude 目录快照。无论哪种情况,这些 id 都不需要专门的 `[[routes]]` 条目;它们按常规路由规则解析,当 `[[routes]]` 与 `[[route_prefixes]]` 均未匹配时回退到 `server.default_provider`。

在维护的条目中添加 `[models.upstream_model]`，即可通过同一声明公开 id、进行路由并转换为上游 id。对于精确 id 路由，建议使用此形式而不是 `[[routes]]`。使用有序 `[[upstreams]]` 时，映射可包含一个或多个 `upstream = "backend-id"` 键值对，并按 `[[upstreams]]` 声明顺序解析为故障转移链。旧式 `[providers.*]` 没有声明顺序，因此只能包含一个键值对。对于这个 id，该映射优先于 `[[routes]]`、`[[route_prefixes]]` 和 `server.default_provider`；每个上游的默认 `effort` 会应用到相应链条目。空映射、空或仅含空白字符的上游名称或后端 id、未知上游、同 id 的 `[[routes]]` 条目、以 `[1m]` 或 `[1M]` 结尾的带映射 id，以及至少有一项带映射的重复 `[[models]]` id 都会导致启动错误。client 会在匹配前移除 context-window hint，因此在带映射 id 中包含该 suffix 会使该条目无法命中。仅由不带映射条目组成的重复 id 保持原有行为。

```toml
[[models]]
id = "claude-opus-4-8"
display_name = "Claude Opus 4.8"

[models.upstream_model]
codex = "gpt-5.2"
```

| 键 | 必需 | 含义 |
| :-- | :-- | :-- |
| `id` | ✅ | 暴露给 Claude Code 的模型 id |
| `display_name` | — | 在 `/model` 选择器中显示的标签 |
| `upstream_model` | — | 从已配置上游名称到后端模型 id 的映射；有序 `[[upstreams]]` 可形成多条目故障转移链，旧式 provider 只允许一个条目 |

## `[sentry]`(可选)

可选启用的错误上报,发送到你自己的 Sentry 项目。未设置 `dsn` 时关闭;与 `[otel]` 相互独立。上报网关自身的诊断信息 — 致命的网关启动/服务错误、panic 和 `error` 级日志事件(`warn`/`info` 作为 breadcrumb,仅含消息)— 此外,只要设置了 `dsn`,每当上游提供方本身返回失败响应时都会无条件发送一个错误/警告事件:5xx 响应对应 `error`,429/529(限流/过载)对应 `warning`,并且仅附带 `model`、`provider`、`upstream_status` 三个标签。请求/响应正文、头部和凭证永远不会发送。指标和 tracing 各自是进一步的独立可选项。

| 键 | 默认 | 含义 |
| :-- | :-- | :-- |
| `dsn` | — | Sentry 项目 DSN。留空则关闭;无效 DSN 为启动错误。Redacting secret —— 在诊断输出中显示为 `[redacted]`(见 [Secret 引用](#secret-引用))。 |
| `environment` | — | 上报事件上的可选 environment 标签 |
| `metrics` | `false` | 同时发送用量指标 — OpenTelemetry 指南中列出的 gateway 指标序列(仅聚合值) |
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
| `metrics` | `true` | 导出 OpenTelemetry 指南中列出的 gateway 指标序列 |
| `logs` | `true` | 导出 `tracing` 日志事件(stderr 日志不受影响) |
| `include_session_id` | `false` | 将客户端 session id 附加到请求 span |

## `[otel.headers]`(可选)

附加到每个 OTLP 请求的 header(例如托管 collector 的令牌)。会合并到标准 `OTEL_EXPORTER_OTLP_HEADERS` 之下。每个 header 值都是 redacting secret 类型,在诊断输出中显示为 `[redacted]`(见 [Secret 引用](#secret-引用))。

| 键 | 含义 |
| :-- | :-- |
| 任意 | header 名称 → 值,例如 `authorization = "Bearer <token>"` |

## 路由优先级

匹配的 `[models.upstream_model]` 条目 → 精确 `[[routes]]` 匹配 → `[[route_prefixes]]` 前缀匹配 → `server.default_provider`。
