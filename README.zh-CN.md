# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · **简体中文**

> 将 Claude Code 分流到任意模型。

`shunt` 是一个符合规范的 [Claude Code LLM 网关](https://code.claude.com/docs/en/llm-gateway-protocol):一个透明代理，针对**你映射的模型**，在**推理层**将推理分流到另一个 LLM 提供方。它按请求的 `model` id 进行路由 —— 其余一切均原样透传给 Anthropic(即“分流”;回退目标可通过 `server.default_provider` 配置)。

名字即机制:电气/铁路中的 *shunt(分流)* 将流量中被选中的部分导向一条并行路径。在这里,被映射模型的推理被分流到另一个提供方,而 Claude Code 的工具和技能保持完好。

它内置了 **OpenAI**、**ChatGPT/Codex**(通过 `codex login` 复用你的订阅)、**xAI**(API 密钥)、**Grok**(通过 `shunt login xai` 复用你的 SuperGrok / X Premium+ 订阅)、**Cursor**(通过 `shunt login cursor` 复用你的订阅)、**Kimi Code**(通过 `shunt login kimi` 复用你的订阅)、**Gemini / Google Code Assist**(通过 `~/.gemini/oauth_creds.json` 复用你的订阅;shunt 会直接使用有效的访问令牌,而自行刷新需要 `SHUNT_GOOGLE_CLIENT_ID` 和 `SHUNT_GOOGLE_CLIENT_SECRET`)以及 **Anthropic** 透传 —— 而任何兼容 Anthropic-Messages 的后端(Kimi、DeepSeek、GLM、MiniMax、OpenRouter、Vercel AI Gateway……)只需一个 TOML 表或 YAML 映射即可接入,无需改动代码。

> [!NOTE]
> `shunt` 是仍在活跃开发中的 1.0 之前(pre-1.0)软件。按照 [SemVer](https://semver.org/lang/zh-CN/#spec) 惯例,`0.x` 版本可能包含对配置键、CLI 和行为的破坏性变更(breaking change) —— 升级前请查看[发布说明](https://github.com/pleaseai/shunt/releases)。

## 安装

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo —— 直接从源码仓库安装
cargo install --git https://github.com/pleaseai/shunt
```

新版本通过 Homebrew 和每个 [GitHub release](https://github.com/pleaseai/shunt/releases) 附带的预构建二进制文件(macOS/Linux,arm64/x64)分发。crates.io 软件包将停留在最后发布的版本。预构建二进制和从源码构建的说明见 [安装](https://shunt.dev/getting-started/installation/)。

### 作为服务运行 (macOS/Homebrew)

```bash
brew services start shunt
```

日志会写入 `$(brew --prefix)/var/log/shunt.log`。`brew services stop` 会发送 `SIGTERM`,
shunt 会先处理完正在进行的请求再退出;在 Unix 上,关机开始时 Antigravity 的 agent 轮次会被终止,
因此它们各自独立的进程组无法拖住这次排空。之后修改配置文件不需要重启 ——
会自动[热重载](docs/config-reload.md)。详见 [作为服务运行](docs/running.md#run-as-a-background-service-homebrew)。

## 快速开始

```toml
# shunt.toml —— 将一个 gpt-* id 路由到你的 ChatGPT 订阅
# [[routes]] 是用于精确 id 的旧式写法;建议优先使用 [models.upstream_model]。
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"        # 复用 `codex login`;使用 `openai` 则读取 OPENAI_API_KEY
```

```bash
codex login                                        # 提供方凭据
shunt run                                           # -> listening on 127.0.0.1:3001

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
claude                                              # /model -> 选择 gpt-5.6-sol
```

未映射的模型(你所有的 `claude-*` id)会完全照旧工作 —— shunt 使用你自己的凭据将它们转发给 Anthropic。完整演练见 [快速开始](https://shunt.dev/getting-started/quickstart/)。

### 起始配置

`shunt init` 会在现有目录中创建带注释的 `shunt.toml`。你可以保留默认 passthrough starter，也可以 scaffold 有序 upstream preset，而不改变未映射模型的 fallback：

```bash
shunt init
shunt init --upstream codex --upstream kimi
```

### Agent 原生设置 blueprint

`shunt add` 用于获取面向编码 agent 的内置 Markdown 实现指南。可用 `shunt add upstream` 列出可用的 upstream blueprint，也可以直接将其输送给 agent：

```bash
shunt add upstream kimi --print | claude
shunt add upstream https://provider.example/docs --print | claude
```

该命令离线且只读：它只打印指南，不会修改文件、安装任何内容或访问网络。若要为全新的 provider protocol 贡献支持，请使用 `shunt add provider <absolute-url>`。

共享部署可以用 `[server] max_concurrent_requests`(默认 `1024`)限制进行中的入站请求数量。
超出的请求会立即以 `503` 和 `Retry-After: 1` 被丢弃,而流式请求会一直占用其名额,
直到响应体结束或客户端断开连接。把该值设为 `0` 可禁用该限制;`/` 和 `/health` 始终保持可用,
以便存活探针访问。共享网关还可以在 `[server.access_control]`、`[server.limits]`、
`[server.timeouts]` 和 `[server.rate_limits]` 下配置 CIDR 允许/拒绝规则、请求/头部/URL 大小限制、
不限制流式响应体的上游响应头超时,以及独立的设备流速率限制。Anthropic Messages 和入站 Codex
Responses 的请求体默认限制为 32 MiB;更大的文件或图片请求请调高 `max_request_bytes`。
其他网关、管理、遥测和分析路由保留各自端点专用的正文大小限制。
参见[配置参考](https://shunt.dev/zh-cn/reference/configuration/#server)。

具有密钥性质的值不必以字面量形式写在配置文件里:任何字符串都可以改写成 `${VAR}`
(环境变量,例如 `"Bearer ${TOKEN}"`)或 `${file:/abs/path}`(某个文件去除首尾空白后的内容,
作为该字段的完整取值),它们会在每次加载配置时重新解析 —— 包括[热重载](docs/config-reload.md),
因此由 `${file:}` 支撑的密钥无需重启 shunt 即可轮换。新值随后是否生效,取决于该字段自身的
重载行为:`[sentry]` 和 `[otel]` 只在启动时构建一次,因此轮换这两段中的密钥仍然需要重启。
参见[配置密钥引用](docs/config-secrets.md)。

## 提供方

一个提供方可以是有序的 `[[upstreams]]` 条目，也可以是旧式 `[providers.<name>]` TOML 表（在 YAML 中，分别对应 sequence 或 mapping 中的条目）。两种适配器类型即可覆盖大多数上游：`kind = "anthropic"`（上游讲 Anthropic Messages；透传，可选择换用不同的密钥）和 `kind = "responses"`（上游讲 OpenAI Responses API；shunt 在 Anthropic Messages ⇄ Responses 之间转换，含流式传输）。第三种原生类型 `kind = "cursor"` 桥接 Cursor 的 ConnectRPC/protobuf AgentService，使 Cursor 订阅可通过同一套 Anthropic-Messages 接口访问。

有序上游支持跨提供方故障转移。声明顺序就是尝试顺序；模型的 `upstream_model` 映射选择参与的条目，并将其公开 id 映射到各后端的 id：

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic" # preset: kind, base_url, and default auth
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "codex-fallback"
provider = "codex" # defaults to chatgpt_oauth

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
codex-fallback = "gpt-5.6-sol"
```

该链先尝试 `anthropic-primary`，再尝试 `codex-fallback`。`auth` 接受 mode 字符串或映射；`claude_oauth` 与 `chatgpt_oauth` 映射可用 `account = "name"` 或 `accounts = [...]` 缩小凭据范围。旧式 `[providers.<name>]` 仍受支持，并会成为按名称排序的隐式上游。不要在配置文件中同时声明两种形式；混用 `[[upstreams]]` 与 `[providers.*]` 会导致配置错误。有关 preset、失败类别和迁移细节，请参阅[配置参考](https://shunt.dev/reference/configuration/)。

**内置:**

| 名称 | 类型 | 认证 | 后端 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | 透传或 Claude OAuth 账号池 | `api.anthropic.com` —— 默认转发调用方自己的凭据;`auth = "claude_oauth"` 可启用池化的订阅凭据 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` —— 复用 `~/.codex/auth.json`(`codex login`) |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` —— 开发者 API,按 token 计费 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` —— Grok CLI 代理;复用 `~/.shunt/xai-auth.json`(使用 SuperGrok / X Premium+ 订阅执行 `shunt login xai`) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` —— 复用 `~/.shunt/cursor-auth.json`(`shunt login cursor`) |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` —— Google Code Assist 后端,复用 `~/.gemini/oauth_creds.json` |
| `antigravity` | `antigravity` | Antigravity OAuth | `cloudcode-pa.googleapis.com` —— 通过 HTTP 访问的 Google Antigravity 后端,使用 `~/.shunt/antigravity-auth.json`(`shunt login antigravity`) |
| `antigravity-cli` | `antigravity_cli` | 无(本地 CLI) | **已弃用。** 本地 `agy` 二进制 —— 通过子进程访问同一后端,已被上面的 `antigravity` 取代 |

xAI 可能按订阅层级限制 OAuth 访问 —— 如果 `grok` 返回 403,请改用 `xai` API 密钥提供方。详见 [`docs/m6-xai-provider.md`](docs/m6-xai-provider.md)。

**Antigravity 有两种传输方式。** `antigravity` 提供方通过 HTTP 与 Google Antigravity 后端通信,使用 `shunt login antigravity` 认证 —— 这是一个使用 Antigravity 自有 OAuth 客户端与作用域的 Google 授权码流程,因此无法复用 Gemini CLI 的登录。它与 `gemini` 提供方使用相同的 Code Assist 协议,目前提供 Gemini 系列的 Antigravity 模型;Antigravity 同时提供的 Claude 模型需要尚未实现的请求改写(#368)。完整设置(登录与作用域、项目发现、模型 slug、thinking 以及适配器传递的内容)详见 [提供方 → Antigravity](https://shunt.dev/zh-cn/providers/antigravity/)。

**`antigravity-cli` 已弃用,且等同于任意代码执行。** 它以 agent 模式运行本地 `agy` 二进制:CLI 使用自己的工具完成工作,shunt 则把它的进度作为 Anthropic SSE 流式返回。因此请求中的 `tools` 数组不会被转发,也永远不会返回 `tool_use` 块。由于非交互式运行无法回答权限提示,`agy` 会带 `--dangerously-skip-permissions` 运行,所以**请把该提供方视为以运行 shunt 的用户身份执行任意代码**。有两项设置对其加以约束:`sandbox`(默认 `true`)会传入 `--sandbox`,把读写限制在工作区内,这才是真正约束该 agent 的机制;`workspace_roots` 只决定它可以从哪里*启动*,它会把取自请求系统提示(由客户端控制的文本)的 `Working directory:` 路径限制为你所列根目录下的规范化路径。请保持沙箱开启,并把监听绑定在回环地址上。建议优先使用上面的 `antigravity` 提供方,它完全不需要这些。请参阅[提供方指南](https://shunt.dev/zh-cn/guides/providers/)。

**从旧的 `antigravity` 迁移。** `kind = "antigravity"` 过去表示本地 CLI。仍按该含义编写的配置会按名称被拒绝,而不是被悄悄改指到别处;而路由到 `antigravity` 却没有凭据的提供方会拒绝启动 —— 在启动看似正常的情况下把传输方式、凭据和出口流量整个换掉,比直接失败更糟。`shunt check` 会执行同样的检查,因此 CI 和部署脚本能在上线前发现指向 `antigravity` 却没有存储凭据的路由,而不是等到启动时才发现。该检查只判断凭据是否存在 —— 它不会打开凭据,因此空的或过期的凭据仍会通过,之后在请求路径上失败。请运行 `shunt login antigravity`,或把路由指向 `antigravity-cli`。

**Anthropic 多账号。** Anthropic 提供方使用 `auth = "claude_oauth"` 时,可以从 Claude Code 凭据文件或 setup-token 环境变量加载明确指定的账号,也可以使用通过 `shunt login claude --name <name>` 创建的私有存储管理账号。Claude 登录有三种模式:`--mode oauth` 运行 shunt 自己的可刷新 OAuth 流程(终端默认模式),`--mode import` 复制当前的 Claude Code 登录,`--mode setup-token` 创建有效期一年的仅推理令牌(`--long-lived` 仍是已弃用的别名)。OAuth 首先使用自动 `127.0.0.1` 回调,失败后转为隐藏的手动粘贴;使用 `--manual` 可跳过回调。OAuth 范围的行为取决于声明形式:旧式 `[providers.*].accounts = []` 会扫描账号存储,而有序的 `[[upstreams]]` 要扫描整个存储则必须同时省略 `account` 与 `accounts`;明确写出 `accounts = []` 会被拒绝。shunt 会让健康的 `x-claude-code-session-id` 会话保持粘性,否则按提供方分别轮询;在条件允许时,它依据感知模型的 5 小时/每周配额状态,在达到上限前主动切换离配额已接近上限的粘性账号。可选的 `[server.pool]` 表可以调整此选择(issue #135):按窗口设置的软配额阈值及账号级覆盖(低阈值会将账号标记为备用账号),考虑消耗速率的排序,可选地预测并避开预计会在重置前耗尽窗口的账号,以及账号级 `priority` 和 `disabled` 设置。启用 `usage_refresh_seconds` 后,可以轮询导入的(可刷新的)账号的 Anthropic OAuth 使用量 API,核对外部产生的消耗;轮询默认关闭。设置 `state_path` 会将每个账号的配额持久化到磁盘,使重启后从上次观察到的使用率暖启动,而不是从空池开始(这是尽力而为的缓存,配额仍会独立地从上游重新推导)。没有重置时间的窗口也会由自身最后观察时间限制存续期,因此不会无限期持久化;持久化默认关闭。对于配额拒绝的 429、401 和 5xx 响应,反应式处理仍是故障转移的最低保障。风暴控制留待后续工作。请参阅[使用方法](https://shunt.dev/guides/anthropic-multi-account/)、[配置参考](https://shunt.dev/reference/configuration/)和 [M8 行为规范](docs/m8-anthropic-multi-account.md)。

**Codex 多账号。** `chatgpt_oauth` 提供方(内置的 `codex` 提供方或使用该认证模式的任意 `responses` 提供方)同样可以池化多个 ChatGPT 账号。账号可以通过导入 `codex login` 的凭据并执行 `shunt login codex --name <name>` 来配置,也可以在[管理 Web 界面](https://shunt.dev/guides/admin-remote-provisioning/)中运行 ChatGPT OAuth,或通过明确的 `credentials`/`token_env` 账号条目配置。OAuth 范围的行为取决于声明形式:旧式 `[providers.*].accounts = []` 会扫描账号存储,而有序的 `[[upstreams]]` 要扫描整个存储则必须同时省略 `account` 与 `accounts`;明确写出 `accounts = []` 会被拒绝。shunt 会记录后端的 `x-codex-*` 5 小时/7 天使用窗口,并将其纳入与 Claude 池相同的**配额感知主动选择**:接近配额上限的粘性账号会在返回 429 前让位;即使上游重置标头为空,观察时间本身也会限制标记的存续期,因此 `[server.pool]` 阈值和消耗速率排序仍然适用。基于冷却时间的反应式故障转移(429、401、5xx、凭据解析失败)仍是安全底线。额外启用可选的 `[server.pool] ramp_initial_concurrency` 慢启动门控后,还可以保护故障转移后新选中的账号,避免并发中的请求同时涌入。请参阅[使用方法](https://shunt.dev/guides/codex-multi-account/)和 [M10 行为规范](docs/m10-codex-multi-account.md)。

`chatgpt_oauth` 提供方可以把多个 ChatGPT 账户组成池,并通过 `[server.pool] reprobe_seconds` 让陈旧的近配额账户重新承接真实请求。选择时只会预留一个账户,在 admission 和凭据解析完成后,首次 HTTP 发送开始时才记录重新探测时间并增加 `shunt.pool.reprobes`;此前失败会取消预留,不会消耗间隔。启用 WebSocket 传输的提供方不会创建这个预留,outbound Responses 池会抑制重新探测,因为流内 rate-limit 事件无法安全轮换。可选的 inbound Codex HTTP 端点仍会重新探测,因此该提供方的指标只统计 inbound 探测。没有 Codex usage 轮询器时,被排除的 outbound 标记会一直保留,直到基于观测时间的窗口寿命上限到期。

**入站 Codex 端点。** shunt 也可以反向运行:可选的 `[server.codex_endpoint]` 表会注册一个原始的 OpenAI Responses 透传接口(`/responses`、`/v1/responses`、`/backend-api/codex/responses`),使 **Codex CLI 本身**可以把它的 `base_url` 指向 shunt,并在同一个 ChatGPT/Codex OAuth 账号池上做负载均衡 —— 这是逐字节的中继,而不是上面那条经 Anthropic 转换的路径。它还会把 CLI 的两个分析(analytics)路径当作保护隐私的丢弃接收端,只记录经过净化的事件名计数器,绝不把负载转发到上游。它默认关闭;没有该表时,这些路由都不会被注册。请参阅[使用方法](https://shunt.dev/zh-cn/guides/inbound-codex-endpoint/)和 [M11 行为规范](docs/m11-inbound-codex-endpoint.md)。

**有界的上游重试。** 提供方单凭据路径上的瞬时上游故障会以指数退避加随机抖动重试,且发生在任何字节抵达客户端之前(绝不在流式传输中途重试)。连接层的传输错误(连接被重置/被拒绝、超时)总是重试 —— 在它们发生时上游尚未接受任何内容。瞬时的响应*状态码*(`429`/`502`/`503`/`504`/`529`,即 Anthropic 的 "Overloaded")只在幂等的 Cursor 路径上重试;非幂等的 Anthropic Messages 和单凭据 Responses POST 会立即把它暴露出来,因为收到响应意味着上游可能已经接受了一次会计费的生成(issue #126)。其他 `4xx` 永不重试。它遵循 `Retry-After`(delta-seconds 和 HTTP-date 两种形式),对 `count_tokens` 不生效,并可在 `[providers.<name>.retry]` 下按提供方配置(默认开启,取值保守;设置 `max_retries = 0` 可禁用)。`claude_oauth`/`chatgpt_oauth` 账号池则改用它们自己的账号轮换故障转移。请参阅[配置参考](https://shunt.dev/reference/configuration/#providersnameretry)。

**可选的 Claude 应用网关登录与策略。** 配置 `[server.gateway]` 后,受管的 Claude Code 客户端可以通过 OAuth 设备流(`forceLoginMethod: "gateway"` + `forceLoginGatewayUrl`)登录,而不必分发同一个共享的静态令牌。浏览器批准可以使用基于环境变量的静态用户,或通过 `[server.gateway.oidc]` 配置的白名单 OIDC 提供方(例如 Google);两种方式也可以同时提供。shunt 提供 OAuth discovery、浏览器批准、device/refresh 授权、HS256 访问 JWT、轮换的不透明 refresh token,以及按用户的 `GET /managed/settings`(带 `ETag` 缓存、遥测环境变量推送和 `availableModels` 强制)。签发的 bearer 会保护 `/v1/models` 以及那些由所选提供方注入服务端凭据的推理路由,而 passthrough 提供方仍然开放。它可以与 `[server.auth]` 组合使用。该功能默认关闭。refresh 会话默认在重启后依然保留(issue #194):`state_path`(默认 `~/.shunt/gateway-sessions.json`)会把 refresh token 以 SHA-256 哈希的形式写入一个原子写入、仅属主可读写(Unix 上为 0600)的文件,并在启动时恢复,因此用户可以继续静默刷新,而不必重新走一遍浏览器流程。设置 `state_path = ""` 可改为仅内存会话,此时重启会使 refresh 会话失效;已签发的访问 JWT 在过期前仍然有效,过期之后用户必须重新登录。device 授权始终仅保存在内存中。客户端也可以从终端登录,而不是在 Claude Code 内部登录:`shunt gateway login <url>` 执行同样的设备流并把会话保存在本地(`~/.shunt/gateway/session.json`,仅属主可读写),`shunt gateway token` 打印可用作 `apiKeyHelper` 的访问令牌,`shunt gateway claude` 则在启动 Claude Code 时只对这一个进程应用该配置 —— 既不修改 `~/.claude/settings.json`,也不会让客户端进入已登录的网关会话,因此不必承担那道门带来的功能取舍(任何 `apiKeyHelper` 都会触发的普通凭据类型门槛依然适用)。`shunt login <provider>` 和 `shunt token` 没有变化,仍然用于让 shunt 对上游进行认证。参见[设置指南](https://shunt.dev/guides/gateway-login/)、[配置参考](https://shunt.dev/reference/configuration/#servergateway-optional)、[M-A 登录说明](docs/gateway-login.md)、[M-B managed-settings 说明](docs/gateway-managed-settings.md)和 [M-C 遥测说明](docs/gateway-telemetry.md)。

**可选的支出上限 Admin API。** 配置 `[server.spend]` 可在 `/v1/organizations/spend_limits` 下注册需要认证的 CRUD 路由,用于组织级和用户级的上限。第一阶段会存储上限和一份审计记录,但尚未对推理流量强制执行这些上限。这些路由使用 `[server.admin]` 凭据认证 —— `[server.spend]` 是不持有任何密钥材料的顶层策略段,因此启用支出上限并不需要 `[server.gateway]` 登录 —— 状态默认持久化到一个原子写入的私有 JSON 文件。设置方法、API 行为和推迟实现的功能见[第一阶段指南](docs/gateway-spend-limits.md)。

**可选的网关遥测接收。** 非空的 `[server.gateway.telemetry].forward_to` 列表会做两件事:通过 managed settings 推送遥测启用标志以及五个 `OTEL_*` 环境变量值(把每个受管客户端的 exporter 指向 shunt),并为客户端随后投递到的入站 OTLP/HTTP 路由开启逐字转发 —— `POST /v1/metrics`、`POST /v1/logs` 和 `POST /v1/traces`,它们与 `[server.gateway]` 的其余接口一起注册,并由与 `GET /managed/settings` 相同的网关 bearer 保护。负载会被**逐字**中继 —— 完全相同的请求字节,带上入站的 `content-type` 和 `content-encoding`,并且绝不带客户端的 `Authorization` 标头 —— 因此 `application/x-protobuf` 和 `application/json` 两种 exporter 都能工作,Claude Code 客户端侧的归属属性也得以保留。每个目的地按信号分别选择加入:`metrics` 默认开启,而 `logs` 和 `traces` 默认关闭,因为 Claude Code 的日志记录和 span 可能包含命令行、提示词和文件路径。没有任何目的地选择加入的信号会被接收并丢弃;中继以分离方式运行,因此即使收集器缓慢或宕机,客户端也总能立即得到 `200`。默认关闭;没有配置目的地时,这些路由只接收并丢弃。参见 [Claude Code 监控](https://code.claude.com/docs/en/monitoring-usage)、[配置参考](https://shunt.dev/reference/configuration/#servergatewaytelemetry-optional)和 [M-C 遥测说明](docs/gateway-telemetry.md)。

**可选的管理 Web 界面。** 配置 `[server.admin]` 可添加一个需要管理员认证的**账号与用量**视图,它会自动观察受支持的宿主登录,并展示可识别的脱敏身份和提供方原生的配额窗口:Claude Code(凭据文件或 macOS Keychain)、Codex CLI(从响应推导的 `x-codex-*` 窗口)、Gemini CLI(全部 Code Assist 模型桶)、Kimi Code(每周和 5 小时限额)、Grok CLI(额度/产品用量)以及 Cursor.app(计费周期、Auto + Composer 以及具名模型的用量)。观察是只读的:shunt 绝不刷新、复制或写入这些来源凭据;Cursor.app 的状态以只读方式打开,仅用于在内存中派生一个第一方 Web 会话。Claude 用量会缓存 60 秒,其他提供方读取器则在请求仪表盘数据时运行。受管池的配置功能仍保留在面向 Claude 和 Codex 账号的折叠高级区域中;那些单独存储的账号才是 shunt 拥有并负责刷新以进行负载均衡的凭据。没有运维人员介入就无法通过任何重试恢复的受管账户,会显示为 **Needs re-login** 而不是 `cooling`,这样永久失效的登录就能与配额暂停区分开,而不是每五分钟永远重试下去;`imported` 行还带有一个 **Refresh** 按钮(`POST /admin/accounts/claude/{name}/refresh`),它会按需执行该账户的 refresh 授权并报告该登录是否仍然有效。受管池的 `/admin/pool` 视图还带有可选的账号级 `plan`(订阅层级),在可获得时由文件推导,并对 Claude 账号提供有界且带缓存的实时回填 —— 它既可以补上缺失的 plan,也可以把缺少倍率细节的文件推导值细化为更精确的值;无法确定 plan 时,该键就直接不存在。该界面默认关闭 —— 没有该表时,不会注册任何 `/admin*` 路由 —— 并且使用与 `[server.auth]` 分开的凭据。要一步启用它,请运行 `shunt dashboard setup`:它会把管理员令牌生成到 `~/.shunt/admin-token`(仅属主可读写),通过 `[server.admin].tokens_file` 接入,使启动环境中不出现任何密钥,启用 `[server.oauth_usage]`,并打印仪表盘 URL —— 然后重启 shunt。管理员令牌也可以来自 `SHUNT_ADMIN_TOKENS`。有两个访问层级:`[[server.admin.write_keys]]` 条目以及 `tokens_env`/`tokens_file` 的 `name:token` 键值对拥有完整访问权限,而 `[[server.admin.read_keys]]` 条目可以通过所有 `GET`(管理界面和支出上限 API 都一样),但在所有变更操作上都会被拒绝,包括浏览器登录表单。数组形式的密钥必须通过 `${VAR}` / `${file:...}` 或 `SHUNT_*` 覆盖提供;在配置文件中写字面量会在加载时被拒绝。请参阅[使用方法](https://shunt.dev/zh-cn/guides/admin-remote-provisioning/)和 [M9 设计说明](docs/m9-admin-surface.md)。

**可选的客户端用量端点。** 配置 `[server.usage]` 可注册一个只读的 `GET /usage`,它返回共享账号池配额的**经过净化的聚合**视图 —— 每个窗口的剩余余量、重置时间,以及粗粒度的 `ok`/`degraded`/`exhausted` 状态 —— 使非管理员客户端无需管理界面也能预判限流。它与 `/v1/messages` 使用同一个 `[server.auth]` 客户端令牌进行认证(并且需要该表),并且绝不暴露账号名称、数量、优先级、`disabled` 标志或阈值 —— 完整的账号级细节仍然只保留在仅管理员可访问的 `/admin/pool` 后面。只有没有任何未禁用账户报告某个窗口时,该窗口才是 `null`。Codex 响应中的 `x-codex-*` 头部会填充 5 小时和共享每周窗口。Codex 没有 Fable 范围(`7d_oi`)的信号,但混合提供方池中的其他提供方可以提供聚合 Fable 值。本分支没有带外 Codex 轮询器,也不会调用为 #430 保留的私有、未公开文档的 ChatGPT 用量 API。默认关闭;没有该表时,该路由不会被注册。请参阅[配置参考](https://shunt.dev/reference/configuration/#serverusage-optional)和 [M12 设计说明](docs/m12-client-usage-endpoint.md)。

**可选的 Claude Code CLI 原生用量条。** 配置 `[server.oauth_usage]` 可注册 `GET /api/oauth/usage`,这正是 Claude Code CLI 自己的 `Current session`/`Current week` 用量条所请求的路径 —— 因此当 CLI 通过 `ANTHROPIC_BASE_URL` 指向 shunt 时,它未经修改的 UI 就能渲染真实的、仅限 Claude 的、按优先级分层的最坏情况池数值,而不是 404 后显示一条空的用量条。**前提条件,仅得到部分验证:** 已确认当 `ANTHROPIC_AUTH_TOKEN` 由 `claude setup-token` 或共享网关客户端令牌设置时,它**不会**请求该路径 —— 这是另外两种有文档记载的 shunt 凭据配置方式;而完整的交互式 `claude login`(订阅)会话*确实*会请求它,这一点是根据对 CLI 二进制文件的静态分析和间接的 UI 证据推定的,并未直接观察到(在侦察环境中无法安全地脚本化一次真实的订阅登录)。因此这并非对每种配置都“开箱即用”;完整的前提条件证据及其唯一未经验证的一环见 [M14 设计说明](docs/m14-oauth-usage-endpoint.md)。认证受绑定拓扑限制(回环地址上无需认证;在非回环绑定上需要有效的客户端令牌或网关 JWT —— 其判定方式与 `/v1/messages` 完全一致,而不是只看标头是否存在 —— 此时还要求已配置 `[server.auth]` 或 `[server.gateway]`)。默认关闭;没有该表时,该路由不会被注册。请参阅[配置参考](https://shunt.dev/reference/configuration/#serveroauth_usage-optional)。

**可选的上游状态轮询。** 在 `[server.status]` 中配置一个或多个 Statuspage `summary.json` 源,即可按固定间隔(默认 5 分钟)轮询各提供方的公开状态源,并把最后观察到的指示状态显示在管理仪表盘的“上游状态”条上,同时作为 `shunt.upstream.status` gauge 指标暴露。它严格只做观察 —— 它读到的任何内容都不会参与路由、故障转移或池/冷却决策。抓取失败、返回非 2xx 响应,或报告了 shunt 无法识别的指示状态的源,会被存储并报告为 `unknown`,而不是被悄悄当作运行正常。默认关闭;没有该表时,不会启动任何后台轮询。请参阅[配置参考](https://shunt.dev/reference/configuration/#serverstatus-optional)和[设计说明](docs/upstream-status.md)。

OpenAI 的 Thibault Sottiaux 已公开欢迎通过其他编码 harness 运行 Codex：

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([来源](https://x.com/thsottiaux/status/2075830097488249060))

他还[进一步演示](https://x.com/thsottiaux/status/2076119366647894371)了如何亲自将 Claude Code（“你那只橙色的螃蟹”）指向 GPT-5.6 Sol —— 这正是 `shunt` 所做的推理层替换，无需单独的应用。

话虽如此，是否从非官方客户端复用你的 ChatGPT/Codex 或 SuperGrok 订阅（或 Kimi、Cursor 等其他后端），由你自己决定 —— 公开的欢迎并不保证未来的政策或账号层面的处置。使用风险自负。

**Cursor** 的工作方式相同 —— 登录一次,然后路由一个 `cursor:*` 模型 id:

```bash
shunt login cursor                                  # OAuth -> ~/.shunt/cursor-auth.json
```

```toml
# shunt.toml —— 将一个 cursor:<id> 路由到你的 Cursor 订阅
[[routes]]
model = "cursor:default"                             # "default" 是 Auto 的 wire id;付费方案可以使用具名 id
provider = "cursor"
```

`cursor:` / `cursor-agent:` / `cursor-plan:` / `cursor-ask:` 前缀用于选择 Cursor 的 agent 模式(Agent / Plan / Ask);后缀是 Cursor 的 **wire** 模型 id(Auto 是 `default`,不是 `auto`)。该适配器会流式传输助手文本和推理内容,把你客户端的工具桥接为原生 Cursor MCP 工具调用,并转发内联图片(issue #170)。详情见 [提供方 → Cursor](https://shunt.dev/zh-cn/providers/cursor/)。

**任何兼容 Anthropic 的后端**只需一个表即可接入 —— 无需改动代码:

| 提供方 | `base_url` | 示例模型 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`、`kimi-k2.7-code` |
| Kimi Code(订阅制,OAuth) | `https://api.kimi.com/coding` | 使用你订阅提供的 ID |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`、`deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`、`glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | 见 [MiniMax 文档](https://platform.minimax.io/docs/token-plan/claude-code) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

上表中的行大多使用 `auth = "api_key"`。例外是 **Kimi Code**,它是与上一行按量计费的 Moonshot API 完全不同的订阅制 Kimi 服务 —— 主机不同,使用 OAuth 而非 API 密钥。它有专门的内置 `kimi-code` 预设(`kind = "anthropic"`、`base_url = "https://api.kimi.com/coding"`、`auth = "kimi_oauth"`),因此只需 `provider = "kimi-code"` 和一个已登录的账户,无需手动编写 `[providers.*]`/`[[upstreams]]` 表:

```bash
shunt login kimi --name <account-name>                # RFC 8628 设备流程 -> ~/.shunt/accounts/kimi/<account-name>.json
```

```toml
# shunt.toml — 路由到你的 Kimi Code 订阅
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# 声明 [[upstreams]] 会替换内置的 provider 集合,因此要在末尾保留一个 anthropic
# passthrough;否则 `shunt check` 会因无法解析默认的 server.default_provider 而失败。
# 这与 `shunt init` 追加的条目相同。
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

`kimi_oauth` 和 `claude_oauth`/`chatgpt_oauth` 一样支持账户池 —— 用 `accounts = [...]` 代替 `account`,即可在多个已保存的 Kimi 账户间分摊负载。包括 admin/`/usage` 池视图在内的完整说明,见 [Kimi → Kimi Code (OAuth subscription)](https://shunt.dev/providers/kimi/#kimi-code-oauth-subscription);设备流程、令牌存储与校验的内部细节见 [M15 设计说明](docs/m15-kimi-oauth.md)。

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

完整列表和各提供方说明见 [提供方](https://shunt.dev/guides/providers/)。

## 文档

一切都在 **[shunt.dev](https://shunt.dev)**:

- [快速开始](https://shunt.dev/getting-started/quickstart/) · [为什么选 shunt?](https://shunt.dev/getting-started/why-shunt/) · [提供方](https://shunt.dev/guides/providers/) · [配置](https://shunt.dev/guides/configuration/) · [故障排查](https://shunt.dev/reference/troubleshooting/)
- **面向 agent:** 每个页面都有一个 Markdown 孪生版本(在任意 URL 后追加 `.md`,或使用页面的 *Copy Markdown* / *Open in AI* 按钮),并且站点按 [llms.txt 规范](https://llmstxt.org/) 发布了 [`/llms.txt`](https://shunt.dev/llms.txt)、[`/llms-small.txt`](https://shunt.dev/llms-small.txt) 和 [`/llms-full.txt`](https://shunt.dev/llms-full.txt)。

设计笔记和里程碑规范位于 [`docs/`](docs/)(从 [`docs/implementation-plan.md`](docs/implementation-plan.md) 开始)。要将 Claude Code 路由到你的 ChatGPT/Codex 订阅,见 [Codex 配置参考](docs/codex-configuration.md)。

### 可观测性指标

| 序列 | 类型 | 属性 | 含义 |
| :-- | :-- | :-- | :-- |
| `shunt.failover` | Counter | `provider`、`state` | 有序上游的故障转移状态迁移:`attempted`、`advanced` 或 `exhausted`。 |

完整的指标表和导出配置见 [OpenTelemetry 指南](https://shunt.dev/zh-cn/guides/opentelemetry/)。

## 为什么

Claude Code 会把每一轮都发送到 Anthropic API。`shunt` 位于前面(通过 `ANTHROPIC_BASE_URL`),针对你映射的模型,将它们的推理分流到另一个提供方(OpenAI、Codex/ChatGPT……)。由于路由发生在 HTTP/推理层 —— 而不是把任务移交给另一个 CLI —— 会话仍在 Claude Code 的框架内运行:相同的工具循环、相同的预加载技能、相同的捆绑脚本路径解析。只有 token 生成被外包出去。

与另一种方案(把 `subagent_type` 移交给像 Codex CLI 这样的另一个运行时)相比,后者在技术栈中切得更高,会丢失人设和预加载技能。

### 按模型,而非按 agent —— 也不是全局替换

选择性由**每个请求上的 `model` id** 驱动,而 Claude Code 本来就允许你按上下文选择它:主会话的 `/model` 选择器、子 agent 定义的 `model:` frontmatter、面向所有子 agent 的 `CLAUDE_CODE_SUBAGENT_MODEL`,或用 `ANTHROPIC_CUSTOM_MODEL_OPTION` 向选择器添加一个自定义条目。因此“只分流这个 agent / 这个会话”是在 Claude Code 中决定的,而 shunt 只是遵从它收到的 model id —— 没有脆弱的按 agent 系统提示指纹识别。与全局模型替换代理不同,主会话可以留在 Claude 上,而只有你指名的模型才被分流。

## Claude Code 集成(官方接口)

Claude Code 在 `ANTHROPIC_BASE_URL` 后暴露了一个**一等公民的网关契约** —— `shunt` 实现的是这个契约,而不是早期 Claude Code 代理所依赖的脆弱的“对子 agent 系统提示做哈希”启发式方法。

- [LLM 网关协议](https://code.claude.com/docs/en/llm-gateway-protocol) —— API 契约:端点、需转发 vs 消费的头部/正文字段、特性透传以及归属信息。运行中的网关在 `GET /protocol` 提供机器可读的规范。
  - [模型发现](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) —— Claude Code 在启动时查询 `GET /v1/models?limit=1000`(通过 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` 主动启用),并将返回的模型加入 `/model` 选择器。默认情况下,`auto_include_builtin_models = true` 会把自动发现的模型追加在精选的 `[[models]]` 条目之后,并按 id 去重;设为 `false` 可得到严格精选的列表。这些模型来自对 `server.default_provider` 的一次实时 `GET /v1/models` 调用(前提是它属于 anthropic 类型),并使用该提供方的认证模式:`passthrough` 转发调用方的凭据,因此每个调用方看到的是自己有权访问的列表;`api_key` 使用配置的密钥;`claude_oauth` 则使用与推理相同的有效账号集合中第一个可解析且未禁用的账号(包括按 `account_scope` 顺序扫描到的存储账号),不做池选择、冷却或配额计算。后两种模式暴露的是一份共享的、按网关凭据划分的目录。shunt 不做任何缓存;当默认提供方不属于 anthropic 类型、没有凭据或调用失败时(2 秒上限),它会回退到内置的 Claude 目录快照。精选条目还可以包含 `[models.upstream_model]` 映射(配合有序 `[[upstreams]]` 时可有多个条目,旧式 providers 下只有一个条目),这会让被公布的 id 可以通过映射到的上游路由,并把它转换成各个映射后端的 id,而无需单独的 `[[routes]]` 条目。**约束:** `id` 不以 `claude`/`anthropic` 开头的条目会被忽略 —— 非 Claude 模型必须设置别名或手动添加。
  - **系统提示归属块** —— Claude Code 会在系统提示前添加一段客户端版本 + 会话指纹;在会话生命周期内保持稳定(v2.1.181+)。`shunt` 原样转发它(从不剥除 —— 那是开发者通过 `CLAUDE_CODE_ATTRIBUTION_HEADER=0` 决定的事)。
- [添加自定义模型选项](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) —— `ANTHROPIC_CUSTOM_MODEL_OPTION` 向 `/model` 选择器添加一个走网关路由的条目,而不替换内置别名;其 ID 跳过校验,因此任何网关接受的字符串都有效。**这是选择非 Claude 模型的主要方式**(例如 `gpt-5.6-sol`),因为发现会忽略不以 `claude`/`anthropic` 开头的 id。
- **工具搜索**(`ENABLE_TOOL_SEARCH`)—— Claude Code 会推迟 MCP/LSP 工具的 schema,并通过 `ToolSearch` 工具按需揭示它们,从而收回模型本来会花在它从不调用的工具上的上下文。由于 shunt 不是 Anthropic 第一方宿主,Claude Code 默认让该功能**保持关闭**,除非你用 `ENABLE_TOOL_SEARCH=true` 主动启用。在 Messages 路径上,推迟能否保留由上游模型而不是某个设置决定:`claude*` 和 `anthropic/*` id 会逐字节保留该协议,而非 Anthropic 的 id(OpenRouter 的隐身 slug、Kimi……)会被剥除其 `defer_loading` 标记和 `tool_search_tool_*` 条目,因为这些宿主会直接拒绝它们(`400 Deferred custom tools are only supported on Anthropic models...`)。它们的工具仍会送达,只是会连同完整 schema 一次性送达,因此在这些模型上工具搜索收不回任何上下文。在 Codex/Responses 路径上,`[providers.<name>]` 下的 `tool_search` 是一个三态设置:不设置(默认的“auto”)只对已知实现了该协议的上游 —— ChatGPT/Codex 后端和 `api.openai.com` —— 映射到 Responses API 自身原生的、由客户端执行的 `tool_search` 协议,而对其他所有 OpenAI 兼容端点(LiteLLM、vLLM、OpenRouter、自建代理……)保留 #43 的文本 shim;`tool_search = true` 会在上游的 flavor 和模型满足条件(非 xAI/Grok、gpt-5.4+)时强制使用原生方式,让你可以为已验证的自定义端点主动启用;`tool_search = false` 则始终强制使用 shim,它会把每个被揭示的工具加入缓存的 `tools` 前缀,并在每次揭示时使其失效。参见[工具搜索](https://shunt.dev/zh-cn/guides/codex/#tool-search)指南。

**设计原则:** 做一个符合规范的 Anthropic-Messages 网关(`/v1/messages`、`/v1/models`,正确的头部/归属透传),按请求的 `model` id 路由,并为映射的模型在 Anthropic Messages ⇄ OpenAI Responses API 之间转换 —— 不使用会在每次 Claude Code 提示变更时失效的提示形状启发式方法。

## 相关工作 / 现有技术

**Claude Code 专用路由器与代理**

- [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) —— 这个细分领域里最大的;以 Claude Code 为基础,决定请求如何抵达不同的模型/提供方。
- [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) —— 在 OpenAI 模型上运行 Claude Code。
- [fuergaosi233/claude-code-proxy](https://github.com/fuergaosi233/claude-code-proxy) —— Claude Code → OpenAI API 代理。
- [seifghazi/claude-code-proxy](https://github.com/seifghazi/claude-code-proxy) —— 捕获/可视化进行中的 Claude Code 请求,可选**按 agent** 路由到其他提供方(`shunt` 子 agent 路由构想的直接灵感来源)。
- [luohy15/y-router](https://github.com/luohy15/y-router) —— 一个让 Claude Code 能与 OpenRouter 协作的简单代理。
- [tingxifa/claude_proxy](https://github.com/tingxifa/claude_proxy) —— 将 Claude API 请求转换为 OpenAI 格式的 Cloudflare Workers 代理(Gemini、Groq、Ollama)。
- [badlogic/claude-bridge](https://github.com/badlogic/claude-bridge) —— 在 Claude Code 中使用任意模型提供方。
- [jimmc414/claude_n_codex_api_proxy](https://github.com/jimmc414/claude_n_codex_api_proxy) —— 跨运行时路由器:将 Anthropic **或** OpenAI API 调用代理到本地的 **Claude Code 或 Codex** CLI(当 API 密钥全为 9 时路由到本地 CLI,否则路由到真正的云端 API)。注意方向相反 —— 是把云端 API 调用路由*到*本地 CLI,而不是把 Claude Code agent 路由*出去*到云端提供方。
- [insightflo/chatgpt-codex-proxy](https://github.com/insightflo/chatgpt-codex-proxy) —— 一个兼容 Anthropic 的 `/v1/messages` 代理,从 **ChatGPT Codex 后端**提供 Claude Code 推理(使用 ChatGPT Plus/Pro 订阅而非 API 密钥)。与 `shunt` 相同的推理层替换,针对 Codex/GPT 订阅后端,同时保留 Claude Code 的 UI 和 MCP 工具。

**通用 AI 网关(相邻基础设施 —— 可作为后端)**

- [BerriAI/litellm](https://github.com/BerriAI/litellm) —— SDK + 代理/AI 网关,以 OpenAI 格式调用 100+ 个 LLM API,带成本追踪、护栏、负载均衡。
- [Portkey-AI/gateway](https://github.com/Portkey-AI/gateway) —— 快速 AI 网关,路由到 1,600+ 个 LLM,集成护栏。
- [maximhq/bifrost](https://github.com/maximhq/bifrost) —— 高性能 AI 网关,带自适应负载均衡,支持 1000+ 个模型。
- [mazori-ai/modelgate](https://github.com/mazori-ai/modelgate) —— 开源 LLM 网关 + MCP 服务器(Go):RBAC/策略强制、多提供方(OpenAI、Anthropic、Gemini、Bedrock、Azure 以及本地 Ollama)、带语义工具搜索的 MCP 网关,以及语义响应缓存。

### `shunt` 有何不同

上面大多数 Claude Code 代理把**所有**流量路由到一个替代提供方(全局模型替换)。`shunt` 的重点是由请求的 `model` id 驱动的**选择性、按模型**分流:让主会话留在 Claude 上,只把你指名的模型分流到其他提供方 —— 即配线架/跳线板的用例。由于 Claude Code 本来就允许你按上下文绑定模型(主会话、子 agent 的 `model:` frontmatter、`CLAUDE_CODE_SUBAGENT_MODEL`),同样的选择性无需 shunt 检查调用方身份即可下探到单个 agent。

## 贡献

欢迎提交 issue 和 PR。构建/测试命令与约定见 [`CONTRIBUTING.md`](CONTRIBUTING.md) 和 [`AGENTS.md`](AGENTS.md),报告漏洞见 [`SECURITY.md`](SECURITY.md)。

### 代码审查

`shunt` 的拉取请求由两个 AI 代码评审工具审查，两者对开源项目均免费：

- [Greptile](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source) — 依据其 OSS 计划，对非商业 MIT/Apache 项目免费。
- [cubic](https://cubic.dev/) — 对公开仓库免费。

## 许可证

在 [Apache License, Version 2.0](LICENSE-APACHE) 或 [MIT license](LICENSE-MIT) 之间任选其一进行许可。除非你明确另行声明,否则任何由你有意提交、以纳入本 crate 的贡献(如 Apache-2.0 许可证所定义)均应按上述方式双重许可,不附加任何额外条款或条件。

---

Made with Orca 🐋

- https://github.com/stablyai/orca
- https://www.onorca.dev/
