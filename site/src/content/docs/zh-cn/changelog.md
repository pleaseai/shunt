---
title: 更新日志
description: shunt 的每一项重要变更，按日期记录，并标注破坏性变更。
---

精选的发布说明，由新到旧。shunt 尚未发布 1.0，因此次版本号递增（`0.44` → `0.45`）可能包含破坏性变更；
修订版本号递增通常不会，但例外都会标注为 **破坏性变更**，请留意这类条目。下面每一条破坏性条目都写明了
受影响的对象、需要采取的措施，以及落地的版本。

- **订阅：**[GitHub 发布源](https://github.com/pleaseai/shunt/releases.atom)
- **完整记录**：由每一次提交生成的 [`CHANGELOG.md`](https://github.com/pleaseai/shunt/blob/main/CHANGELOG.md)
- **升级：**[安装](/zh-cn/getting-started/installation/)

本页涵盖 0.35.0 及之后的版本。更早的发布只存在于生成的更新日志中。

## 0.45.1 — 2026-09-14

### 为 `gpt-6-astra` 启用原生工具搜索

**修复** — `gpt-6-astra` 现在会协商上游自身的工具搜索能力，而不是回退到翻译形式。参见
[推理强度与上下文](/zh-cn/guides/effort-and-context/)。

## 0.45.0 — 2026-09-14

### 管理 JSON 与写操作路由迁移至 `/admin/api/*`

**变更 · 破坏性变更，自 0.45.0 起** — 影响所有以脚本调用管理 API 的地方。全部 13 条 JSON 与写操作路由在
`/admin` 之后多了一个 `/api` 段，且旧路径被移除而非保留为别名，因此每个调用方都必须更新。`/admin`、
`/admin/login` 和 `/admin/oidc/callback` 不受影响；完整的前后对照表见
[管理路径迁移](/zh-cn/reference/endpoints/#管理路径迁移)。

### `GET /admin` 返回 SPA 外壳而不再重定向

**变更 · 破坏性变更，自 0.45.0 起** — 影响那些把指向 `/admin/login` 的 `303` 当作「未登录」的脚本。未认证的
`GET /admin` 现在返回 `200` 和内嵌外壳，该外壳是对所有访问者都相同的单个静态文件，不携带任何运维数据；
重定向并未消失，而是移入了包内，它会在 `GET /admin/api/session` 返回 `401` 后触发。请改用该引导端点判断
是否已登录。参见 [HTTP 端点](/zh-cn/reference/endpoints/#管理-spa-包--features-ui)。

### 只读密钥可以开启浏览器会话

**变更 · 破坏性变更，自 0.45.0 起** — 影响那些把「只读密钥无法开启浏览器会话」当作吊销特性的部署。对于
`[server.admin] read_keys` 凭据，`POST /admin/login` 现在返回 `303` 和一个只读层级的会话 Cookie，而此前返回
`401`。所有写操作仍返回 `403`，因此该 Cookie 并未赋予密钥本就没有的权限，但它有自己的生命周期。浏览器会话
仅依据内存中的会话存储校验，因此轮换一个已泄露的只读密钥只能在下次重新加载时终止其请求头凭据，而它的
Cookie 会一直读取到 `session_ttl_secs`（默认 1 小时）到期为止 — 请通过重启而非重新加载来清除它。

### `--features ui` 背后的内嵌管理 SPA

**新增** — 管理面板现在是在构建期内嵌、由 `/admin` 提供的已编译 SPA 包，取代了原先服务端渲染的字符串字面量。
未启用 `--features ui` 构建的二进制在外壳路由上返回 `404`，并指明所需的 feature。参见
[管理与远程预配](/zh-cn/guides/admin-remote-provisioning/)。

### 优雅停机排空有了上限且可配置

**新增** — 首次收到 SIGTERM/SIGINT 后，进行中的 HTTP/SSE/WebSocket 工作会排空 `shutdown_timeout_seconds`
（默认 `30`，范围 `1`–`3600`），其余部分随后被取消。修改该值需要重启。参见
[`[server]`](/zh-cn/reference/configuration/#server)。

### 更新 `h2` 以修复空帧拒绝服务

**安全** — 已更新 `h2` 依赖，修复通过向连接灌入空帧即可触发的拒绝服务。请升级到 0.45.0 或更高版本，无需
修改配置。

### 可选的 function 参数在翻译后得以保留

**修复** — OpenAI 后端会把没有显式 `strict` 的 function 工具向严格模式归一化，而在该模式下，封闭的参数对象会
把每个属性都当作必填，于是模型会填入调用方从未设置的值。现在 shunt 会在转发的 function 工具上固定
`strict:false`，让可选属性保持可选。参见 [故障排查](/zh-cn/reference/troubleshooting/)。

### Codex 目录失败改用协商得到的错误形态

**修复** — 针对 Codex 目录的模型发现失败，现在返回入站端点协商得到的错误形态，而不再一律使用 Anthropic 的
形态，这样 OpenAI 协议的客户端就能走自己的错误路径解析。参见
[模型发现](/zh-cn/guides/model-discovery/)。

### 管理登录与预配修复

**修复** — 登录页 CSP 不再发送 `script-src` 和 `connect-src`；SPA 外壳在 `/admin/` 与 `/admin` 上都可访问；
同一个待处理登录的并发完成请求被串行化；被拒绝的启动会说明它关闭了授权步骤；添加账户表单不再错误地报告
登录成功。

## 0.44.0 — 2026-09-09

### 入站 Responses 请求可翻译到 Anthropic 与 Chat Completions 上游

**新增** — 到达入站 Codex 端点的请求现在可以路由到 Anthropic 类或 Chat Completions 上游，翻译在两个方向上
都会处理。参见 [入站 Codex 端点](/zh-cn/guides/inbound-codex-endpoint/)。

### WebSocket 传输会记录流内的 `codex.rate_limits`

**修复** — WebSocket 传输现在会记录流内的 `codex.rate_limits` 事件，因此配额窗口在复用连接上也能保持最新，
而不只是在新的 HTTP 轮次上。这会反映到 [`GET /usage`](/zh-cn/reference/endpoints/)。

### 移除 OpenAI 校验器无法编译的工具 schema 正则

**修复** — OpenAI 后端会用 Python 的 `re` 编译工具 schema 中的每一个 `pattern`，因而拒绝仅 JavaScript 支持的
正则（`\p{Cc}`、`(?<name>…)`、`\u{…}`）。现在 shunt 会从转发的 schema 中剥离这些 pattern；在严格模式之外
`pattern` 只是建议性的，因此丢失的只是提示。参见 [故障排查](/zh-cn/reference/troubleshooting/)。

## 0.43.0 — 2026-09-08

### `GET /usage` 提供按提供方的细分

**新增** — 除池级汇总外，`GET /usage` 现在在 `providers` 下按每个已池化的提供方给出同样经过脱敏的数值，
因此只路由到某一个提供方的客户端可以读取该提供方的余量，而不是混合后的平均值。认证方式非池化的提供方会被
省略。参见 [HTTP 端点](/zh-cn/reference/endpoints/)。

### `GET /usage` 报告池平均余量，而非利用率最低的账户

**变更** — 每个窗口现在报告的是所有报告该窗口且未被禁用的账户上的 `mean(1 - utilization)`，即池中合计容量
尚未使用的比例，而不再是利用率最低的那一个账户。此前把该字段读作单个账户余量的客户端，现在看到的是池级
数值。

## 0.42.0 — 2026-09-07

### 入站 Responses 端点支持按模型路由的第三方上游

**新增** — 入站 Responses 端点现在遵循按模型路由到第三方上游的配置，因此 Codex CLI 客户端可以凭模型 id 访问
OpenAI 之外的厂商。参见 [入站 Codex 端点](/zh-cn/guides/inbound-codex-endpoint/)。

## 0.41.3 — 2026-09-07

### 没有 `usagePercent` 的 Grok 产品不再清空配额行

**修复** — 此前报告中缺少 `usagePercent` 的产品不会被跳过，而是清空了整行配额。参见
[xAI / Grok](/zh-cn/guides/xai/)。

## 0.41.2 — 2026-09-07

### 设备页的 SSO 表单可以重定向到身份提供方

**修复** — 设备页的 CSP `form-action` 拒绝了指向所配置身份提供方的重定向，导致无法在该页面完成 SSO。参见
[网关登录](/zh-cn/guides/gateway-login/)。

## 0.41.1 — 2026-09-07

### 即使 `Origin` 为 null 也接受设备页自身的表单 POST

**修复** — 在 `Referrer-Policy: no-referrer` 下，同页表单 POST 也会带着 `Origin: null` 到达，而 CSRF 防护会
拒绝它。现在该防护先依据 `Sec-Fetch-Site: same-origin` 判定。

### 未送达的 `agy` 交接不再让已完成的轮次失败

**修复** — 已经完成的轮次不会再因为一次无法送达本地 `agy` 子进程的交接而失败。参见
[Antigravity](/zh-cn/providers/antigravity/)。

## 0.41.0 — 2026-09-05

### 智谱与 MiniMax 国内版预设

**新增** — `zhipu` 与 `minimax-cn` 作为内置的 Anthropic 兼容预设提供，因此接入中国大陆端点只需要凭据，无需
手写提供方表。参见 [智谱](/zh-cn/providers/zhipu/) 与 [MiniMax 国内版](/zh-cn/providers/minimax-cn/)。

### 遇到未知模型时重新发现 Antigravity 的 effort 矩阵

**修复** — 指定了缓存 effort 矩阵中不存在的模型的轮次，现在会触发重新发现而不是失败。参见
[Antigravity](/zh-cn/providers/antigravity/)。

### Cursor composer 快速模式与内置工具调用

**修复** — composer 快速模式作为模型元数据发送，而不再编码进模型 id；包含内置工具调用的轮次会被呈现而不是
丢弃。参见 [Cursor](/zh-cn/providers/cursor/)。

## 0.40.2 — 2026-09-05

### `antigravity-cli` 拒绝调用方提供的工具，而不是忽略它们

**变更 · 破坏性变更，自 0.40.2 起** — 影响向 `antigravity-cli` 提供方发送非空 `tools` 数组（`tool_choice`
为 `none` 时除外），或 `any`、`tool` 形式 `tool_choice` 的调用方。该提供方运行本地 `agy` 二进制，而 `agy` 自行解析工具调用，从不返回
`tool_use` 块；因此这类请求过去会得到一个悄悄忽略工具的纯文本 `200`，现在则以 `400
invalid_request_error` 拒绝。请将任务作为普通提示发送，或把该模型路由到会转发工具的原生 `antigravity`
或 `gemini` 提供方。没有工具且 `tool_choice` 为 `auto` 时不受影响。
参见 [Antigravity](/zh-cn/providers/antigravity/)。

### 为 `gpt-6-astra` 将 Codex 客户端标识升到 0.153.3

**修复** — 对外声明的 Codex 客户端标识已升至 0.153.3，这是后端提供 `gpt-6-astra` 所要求的版本。参见
[ChatGPT / Codex](/zh-cn/guides/codex/)。

### 将流内的 `rate_limit_exceeded` 归类为 429

**修复** — 流内的 `rate_limit_exceeded` 事件现在被归类为 429，misalignment steer 会转发给客户端而不是被
丢弃。

### Gemini 元组式数组 schema

**修复** — Gemini 所要求的 `items` schema 现在由元组式的数组定义推导得出，而不再以后端拒绝的形态发送。

## 0.40.1 — 2026-09-03

### Antigravity 从实时目录解析模型 id

**修复** — 模型 id 会对照账户的实时目录解析，并且被固定到生产主机的 `base_url` 会被重定向；两者共同消除了
让该提供方无法使用的虚假 429「quota」拒绝。Antigravity 的目录因账户而异且会无预告变动，因此不再硬编码
id。参见 [Antigravity](/zh-cn/providers/antigravity/)。

## 0.40.0 — 2026-09-02

### 通过 `wham` 用量端点获取 Codex 账户配额

**新增** — shunt 会轮询 `wham` 用量端点获取 Codex 账户配额，因此无需等待流量返回 `x-codex-*` 响应头即可填充
池状态。参见 [Codex 多账户](/zh-cn/guides/codex-multi-account/)。

### Antigravity 以 agent envelope 访问 daily 后端

**修复** — 请求现在会携带完整的 agent envelope 和带 effort 后缀的模型 id 访问 daily 后端，这正是该服务实际
接受的组合。参见 [Antigravity](/zh-cn/providers/antigravity/)。

### 合并相邻的 Gemini 用户轮次

**修复** — 相邻的用户轮次会被合并，使工具配对能够跨越对话中途的系统消息保持成立。

## 0.39.2 — 2026-09-01

### 从未被选中的账户也保留需要重新登录的判定

**修复** — 从未被任何提供方表选中的账户，现在会连同 `has_state: false` 一起报告 `needs_relogin`，因为管理
刷新探测是按存储名记录其判定的。参见 [HTTP 端点](/zh-cn/reference/endpoints/)。

## 0.39.1 — 2026-08-31

### 按凭据类型报告 Claude 账户状态

**修复** — 账户状态由凭据类型推导，而不是原始过期时间，因此已失效的凭据会被报告为需要重新登录，而不只是
已过期。参见 [Anthropic 多账户](/zh-cn/guides/anthropic-multi-account/)。

## 0.39.0 — 2026-08-29

### 择机重新探测接近配额的陈旧账户

**新增** — 因接近配额而暂停的账户会在有机会时被重新探测，因此窗口一重置就能回到轮换，而不必等满固定的
冷却时间。参见 [Anthropic 多账户](/zh-cn/guides/anthropic-multi-account/)。

### 池中的套餐按账户身份建索引

**修复** — 套餐按账户身份建索引，且底层文件读取采用 single-flight，因此并发读取不会再把某个账户的套餐算到
另一个账户头上。

### 在非 Anthropic 的 Messages 模型上剥离 deferred 工具

**修复** — 在转发给不理解 deferred 工具块的非 Anthropic Messages 上游之前，会先将其移除。参见
[模型别名](/zh-cn/guides/model-aliases/)。

### 限制无重置时间的配额标记的存活期

**修复** — 没有携带重置时间戳的配额标记现在会自行过期，而不会无限期地暂停该账户。

## 0.38.0 — 2026-08-25

### `kind = "antigravity"` 现在指原生 HTTP 上游

**变更 · 破坏性变更，自 0.38.0 起** — 影响所有配置了 `antigravity` 提供方的配置文件。该名称现在表示原生
HTTP 上游，本地 `agy` 子进程传输迁移到了 `kind = "antigravity_cli"`（内置提供方 `antigravity-cli`）。仍
沿用旧含义的配置会按名称被拒绝，而不是被改指到别处；被路由到但没有凭据的 `antigravity` 提供方会拒绝启动。
若要保留子进程传输，请把表名改为 `antigravity_cli`；若要改用 HTTP 传输，请补上凭据 — 参见
[Antigravity](/zh-cn/providers/antigravity/)。

### `kind = "antigravity_cli"` 已弃用

**弃用** — 本地 `agy` 子进程传输已被原生 HTTP 上游（`kind = "antigravity"`）取代而弃用。它仍可工作，请在
方便时迁移。参见 [Antigravity](/zh-cn/providers/antigravity/)。

### 含有 shunt 凭据的共享槽位会被整体移除

**变更 · 破坏性变更，自 0.38.0 起** — 仅影响在单个请求中多次发送 `authorization` 或 `x-api-key` 的调用方。
当 shunt 自己的凭据与真正的上游凭据共用一个槽位时，现在整个槽位都会被移除，上游凭据也随之丢失。请用单独
的槽位发送上游凭据。参见 [共享网关](/zh-cn/guides/shared-gateway/)。

### 读/写管理密钥，以及迁移到 `[server.spend]` 的支出面

**新增** — `[server.admin]` 新增了 `read_keys` 和 `write_keys`：只读密钥可通过所有管理 GET，在所有写操作上
被 `403` 拒绝。支出上限相关配置迁移到了独立的
[`[server.spend]`](/zh-cn/reference/configuration/#serverspend可选) 表。参见
[`[server.admin]`](/zh-cn/reference/configuration/#serveradmin可选)。

### `shunt gateway` 登录、令牌助手与 Claude Code 启动器

**新增** — `shunt gateway login`、`shunt gateway token`、`shunt gateway claude` 和 `shunt gateway logout`
让客户端可以通过设备流向共享网关认证，并据此启动 Claude Code。参见
[网关登录](/zh-cn/guides/gateway-login/) 与 [CLI](/zh-cn/reference/cli/)。

### 作为一等订阅上游的 Kimi Code OAuth

**新增** — 除 Moonshot API 密钥外，现在也可以通过 OAuth 直接使用 Kimi Code 订阅。参见
[Kimi](/zh-cn/providers/kimi/)。

### `[server.gateway.session]` JWT 配置

**新增** — 网关会话的 JWT 参数可在
[`[server.gateway.session]`](/zh-cn/reference/configuration/#servergatewaysession可选) 下配置。

### 配置中的 `${VAR}` 与 `${file:}` 引用，以及密文脱敏

**新增** — 配置值会解析 `${VAR}` 环境变量引用和 `${file:…}` 路径引用，且密文字段在调试输出中会被脱敏，因此
导出的配置不会泄露凭据。参见 [配置参考](/zh-cn/reference/configuration/)。

### 支出上限管理 API

**新增** — 支出上限管理 API 的第一阶段已落地于
[`[server.spend]`](/zh-cn/reference/configuration/#serverspend可选) 之下。

### 在池状态中暴露账户套餐

**新增** — 池中的账户对象可以带一个可选的 `plan` 字符串，并在可能时通过档案查询精化为更准确的值。参见
[HTTP 端点](/zh-cn/reference/endpoints/)。

### Codex 客户端面同步到 `openai/codex` 0.148.0

**变更** — Codex 的客户端标识与请求面已同步到上游 0.148.0。参见 [ChatGPT / Codex](/zh-cn/guides/codex/)。

### 网关 JWT 与客户端令牌绝不会到达上游

**安全** — 网关 JWT 现在按形态剥离，而不只是在它通过认证时才剥离，并且不会在任一凭据槽位中被转发；静态
`[server.auth]` 令牌按值剥离；入站的 `x-api-key` 在 Codex 透传上被剥离。所有凭据槽位的转发点都走同一处共享
剥离逻辑，因此接受规则与剥离规则不会彼此偏离。参见 [共享网关](/zh-cn/guides/shared-gateway/)。

### `shunt check` 会执行被路由的 Antigravity 凭据校验

**修复** — `shunt check` 现在应用与启动相同的凭据校验，因此被路由但缺少凭据的 `antigravity` 提供方会在服务
启动前就被报告出来。参见 [CLI](/zh-cn/reference/cli/)。

## 0.37.0 — 2026-08-13

### 可选启用的上游 Statuspage 轮询

**新增** — `[server.status]` 会轮询所配置的提供方 Statuspage 源，并暴露最近一次观测到的指标、描述与事件，
仅供观测。路由与故障转移绝不会参考它。配置键见
[配置参考](/reference/configuration/#serverstatus-optional)（英文）。

### `grok-4.6` 与刷新后的 Grok 模型面

**新增** — 新增 `grok-4.6` 并刷新了 Grok 的模型面。参见 [xAI / Grok](/zh-cn/guides/xai/)。

## 0.36.0 — 2026-08-11

### `agy` 以带流式输出与沙箱的智能体模式运行

**新增** — Antigravity CLI 传输现在以智能体模式运行，具备流式输出、沙箱以及发现得到的 effort 矩阵。参见
[Antigravity](/zh-cn/providers/antigravity/)。

## 0.35.0 — 2026-08-10

### 入站正文上限默认为 32 MiB

**变更 · 破坏性变更，自 0.35.0 起** — 影响发送 32 至 64 MiB 之间请求正文的场景，通常是较大的文件或图片
请求。该上限的默认值从此前硬编码的 64 MiB 变为 32 MiB，落在该区间的请求会返回 `413 request_too_large`。
若要恢复原来的上限，请调高 `[server.limits]` 的 `max_request_bytes` — 参见
[配置参考](/zh-cn/reference/configuration/)。

### HTTP 调优配置面

**新增** — `[server.limits]`、`[server.timeouts]` 及相关表暴露了此前硬编码的正文、请求头、URL 与超时调优项。
参见 [配置参考](/zh-cn/reference/configuration/)。
