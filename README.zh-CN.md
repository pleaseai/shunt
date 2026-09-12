# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · **简体中文**

> 将 Claude Code 分流到任意模型。

`shunt` 是一个符合规范的 [Claude Code LLM 网关](https://code.claude.com/docs/en/llm-gateway-protocol):一个透明代理，针对**你映射的模型**，在**推理层**将推理分流到另一个 LLM 提供方。它按请求的 `model` id 进行路由 —— 其余一切均原样透传给 Anthropic(即“分流”;回退目标可通过 `server.default_provider` 配置)。

名字即机制:电气/铁路中的 *shunt(分流)* 将流量中被选中的部分导向一条并行路径。在这里,被映射模型的推理被分流到另一个提供方,而 Claude Code 的工具和技能保持完好。

内置了 OpenAI、ChatGPT/Codex、xAI、Grok、Cursor、Kimi Code、智谱、MiniMax 国内版、Gemini、Antigravity 以及 Anthropic 透传,其中不少可以直接复用你已经在付费的订阅。任何兼容 Anthropic-Messages 的后端只需一个配置表即可接入,无需改动代码。参见[提供方](#提供方)。

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

上面的 `cargo install` 构建出的二进制不含管理看板:看板的前端包需要 Node.js 22.12+,且只有 `--features ui` 才会内嵌,而 Homebrew 和发布二进制已经启用了该特性。其余部分(包括管理 JSON API)两者完全相同。从源码构建的步骤见 [安装](https://shunt.dev/getting-started/installation/)。

### 作为服务运行 (macOS/Homebrew)

```bash
brew services start shunt
```

日志会写入 `$(brew --prefix)/var/log/shunt.log`。`brew services stop` 会发送 `SIGTERM`,
shunt 会先处理完正在进行的请求再退出 —— 最多等待 `[server] shutdown_timeout_seconds`(默认 30 秒;
修改该值需要重启),超时后不再等待并退出,因此安静的 SSE 流无法无限期地拖住进程。在 Unix 上,关机开始时 Antigravity
的 agent 轮次会被终止,因此它们各自独立的进程组无法拖住这次排空。之后修改配置文件不需要重启 ——
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

### 内置

以下提供方默认已内置,无需自己编写 `[providers.*]` 表,直接用 `provider = "<name>"` 即可路由 —— **但仅限未声明 `[[upstreams]]` 时**。有序的 `[[upstreams]]` 会整体替换提供方映射,在该形式下,包括预设在内的所有路由目标提供方都必须在其中声明:

| 名称 | 类型 | 认证 | 后端 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | 透传或 Claude OAuth 账号池 | `api.anthropic.com` —— 默认转发调用方自己的凭据;`auth = "claude_oauth"` 可启用池化的订阅凭据 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` —— 复用 `~/.codex/auth.json`(`codex login`) |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` —— 开发者 API,按 token 计费 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` —— Grok CLI 代理;复用 `~/.shunt/xai-auth.json`(使用 SuperGrok / X Premium+ 订阅执行 `shunt login xai`) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` —— 复用 `~/.shunt/cursor-auth.json`(`shunt login cursor`) |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` —— Google Code Assist 后端,复用 `~/.gemini/oauth_creds.json` |
| `antigravity` | `antigravity` | Antigravity OAuth | `daily-cloudcode-pa.googleapis.com` —— 通过 HTTP 访问的 Google Antigravity 后端,使用 `~/.shunt/antigravity-auth.json`(`shunt login antigravity`) |
| `antigravity-cli` | `antigravity_cli` | 无(本地 CLI) | **已弃用。** 本地 `agy` 二进制 —— 通过子进程访问同一后端,已被上面的 `antigravity` 取代 |

有序的 `[[upstreams]]` 条目还接受 `kimi`、`kimi-code`、`zhipu`、`minimax-cn` 预设,它们会补齐对应后端的 `kind`、`base_url` 和默认认证。

各提供方的设置、模型 id 和注意事项都在[提供方](https://shunt.dev/zh-cn/guides/providers/)下,包括 xAI 的 OAuth 层级限制（[xAI / Grok](https://shunt.dev/zh-cn/guides/xai/)）、Cursor 的 agent 模式前缀（[Cursor](https://shunt.dev/zh-cn/providers/cursor/)）以及 Antigravity 的两种传输方式和 `kind = "antigravity"` 迁移（[Antigravity](https://shunt.dev/zh-cn/providers/antigravity/)）。

> [!WARNING]
> `antigravity-cli` 已弃用,并且相当于**任意代码执行**:它以 shunt 运行者的身份、带 `--dangerously-skip-permissions` 以 agent 模式运行本地 `agy` 二进制。请保持 `sandbox` 设置开启,并把监听地址留在回环地址上。建议改用上面的 `antigravity` 提供方,它完全不需要这些。参见[已弃用的传输方式](https://shunt.dev/zh-cn/guides/providers/#已废弃的-antigravity_cli-传输)。

### 任何兼容 Anthropic 的后端

只需一个表,无需改动代码:

| 提供方 | `base_url` | 示例模型 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`、`kimi-k2.7-code` |
| Kimi Code(订阅制,OAuth) | `https://api.kimi.com/coding` | 使用你订阅提供的 ID |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`、`deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`、`glm-4.7` |
| 智谱（GLM 国内版） | `https://open.bigmodel.cn/api/anthropic` | `glm-5.3`、`glm-5.3-flash` |
| MiniMax | `https://api.minimax.io/anthropic` | 见 [MiniMax 文档](https://platform.minimax.io/docs/token-plan/claude-code) |
| MiniMax 国内版 | `https://api.minimax.cn/anthropic` | `MiniMax-M3` |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"
```

上表中的行大多使用 `auth = "api_key"`。**Kimi Code** 是例外:它与按量计费的 Moonshot API 是两个服务,按订阅计费,主机不同,并且用 OAuth 而非 API 密钥。它有内置的 `kimi-code` 预设。该预设仅在有序的 `[[upstreams]]` 条目中生效(它不在已内置的提供方映射里),因此请在那里声明并登录。参见 [Kimi Code](https://shunt.dev/zh-cn/providers/kimi/#kimi-codeoauth-订阅)。

### 复用订阅

OpenAI 的 Thibault Sottiaux 已公开欢迎通过其他编码 harness 运行 Codex：

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([来源](https://x.com/thsottiaux/status/2075830097488249060))

他还[进一步演示](https://x.com/thsottiaux/status/2076119366647894371)了如何亲自将 Claude Code（“你那只橙色的螃蟹”）指向 GPT-5.6 Sol —— 这正是 `shunt` 所做的推理层替换，无需单独的应用。

话虽如此，是否从非官方客户端复用你的 ChatGPT/Codex 或 SuperGrok 订阅（或 Kimi、Cursor 等其他后端），由你自己决定 —— 公开的欢迎并不保证未来的政策或账号层面的处置。使用风险自负。

**Antigravity 是条款对此有明文规定的例外。** Google 的 [Antigravity 条款](https://antigravity.google/terms)写明：“使用第三方软件、工具或服务访问本服务（例如将 OpenClaw 与 Antigravity OAuth 配合使用）即违反本协议”，且此类违约“可能成为暂停或终止你的 Antigravity 和/或 Gemini CLI 账号的理由”。shunt 的 `antigravity` 提供方正是如此 —— 使用 Antigravity OAuth 的第三方软件 —— 因此经由该提供方路由恰好落在该条款之内。运行 `shunt login antigravity` 之前，请据此做出决定。

## 可选的服务端功能

除非某一行另有说明,下列功能**默认关闭**;缺少对应的配置表时,既不会注册任何路由,也不会启动后台任务。

| 功能 | 启用方式 | 文档 |
| :-- | :-- | :-- |
| Anthropic 多账号池化 —— 粘性会话、配额感知轮换、预测性规避 | 拥有两个及以上账号的 `auth = "claude_oauth"`；`[server.pool]` 只是可选调优 | [指南](https://shunt.dev/zh-cn/guides/anthropic-multi-account/) |
| Codex 多账号池化 —— `x-codex-*` 窗口跟踪、慢启动爬坡、重新探测 | 拥有两个及以上账号的 `auth = "chatgpt_oauth"`；`[server.pool]` 只是可选调优 | [指南](https://shunt.dev/zh-cn/guides/codex-multi-account/) |
| 入站 Codex 端点 —— 把 **Codex CLI** 指向 shunt 并纳入同一个池,还可按模型选择性路由 | `[server.codex_endpoint]` | [指南](https://shunt.dev/zh-cn/guides/inbound-codex-endpoint/) |
| Claude 应用网关登录 —— OAuth 设备流、managed settings、按用户策略 | 具备 `public_url`、不少于 32 字节的 JWT 密钥,以及静态用户或 `[server.gateway.oidc]` 的 `[server.gateway]` | [指南](https://shunt.dev/zh-cn/guides/gateway-login/) |
| 网关遥测接收 —— 原样转发受管客户端的 OTLP | 已配置的 `[server.gateway]`,以及 `forward_to` 非空的 `[server.gateway.telemetry]` | [参考](https://shunt.dev/zh-cn/reference/configuration/#servergatewaytelemetry可选) |
| 管理 Web 界面 —— 账号与用量看板、浏览器预配 | 手写 `[server.admin]` 并提供管理员凭据（`tokens_env`、`tokens_file` 或一条 `write_keys`），**或者**用 `shunt dashboard setup` 一次性写入配置表并签发令牌 —— 但写入配置表和签发令牌仅发生在 `[server.admin]` 不存在时；若已存在，它会保留现有凭据，只补上缺失的 `[server.oauth_usage]`。看板本身由只有 `--features ui` 构建才会内嵌的前端包提供 —— 预构建的发布二进制和 Homebrew formula 已包含，普通的 `cargo build`/`cargo install` 则没有 | [指南](https://shunt.dev/zh-cn/guides/admin-remote-provisioning/) |
| 支出上限 Admin API —— 组织级和用户级上限(stage 1 只存储,尚未实施) | `[server.admin]`（必须提供管理员凭据: `tokens_env`、`tokens_file` 或一条 `write_keys`/`read_keys` —— read 级别只服务 GET） + `[server.spend]` | [参考](https://shunt.dev/zh-cn/reference/configuration/#serverspend可选) |
| 客户端用量端点 —— `GET /usage` 返回脱敏聚合后的池余量 | `[server.auth]`（必须在 `tokens_env` 中提供客户端令牌,默认 `SHUNT_CLIENT_TOKENS`） + `[server.usage]` | [参考](https://shunt.dev/zh-cn/reference/configuration/#serverusage可选) |
| Claude Code CLI 原生用量条 —— 提供 `GET /api/oauth/usage` | `[server.oauth_usage]`;非回环 bind 还需 `[server.auth]`（必须在 `tokens_env` 中提供客户端令牌,默认 `SHUNT_CLIENT_TOKENS`）或 `[server.gateway]` | [参考(英文)](https://shunt.dev/reference/configuration/#serveroauth_usage-optional) |
| 上游状态轮询 —— 将 Statuspage 指示灯作为指标展示,在 `--features ui` 构建中还会出现在看板上 | 至少含一个 `[[server.status.sources]]` 条目的 `[server.status]` | [参考(英文)](https://shunt.dev/reference/configuration/#serverstatus-optional) |
| 有界的上游重试 —— **默认开启**,保守,且绝不在流中途重试 | `[providers.<name>.retry]` | [参考(英文)](https://shunt.dev/reference/configuration/#providersnameretry) |
| 共享部署限制 —— **默认启用**(并发 1024、请求体 32 MiB、TTFB 120 秒、设备流限速),CIDR、请求头与 URL 限制需显式配置 | `[server] max_concurrent_requests`、`[server.access_control]`、`[server.limits]`、`[server.timeouts]`、`[server.rate_limits]` | [指南](https://shunt.dev/zh-cn/guides/shared-gateway/) |
| 密钥引用 —— 任意字符串值可写成 `${VAR}` 或 `${file:/abs/path}`,每次热重载重新解析(`[sentry]`/`[otel]` 除外,启动时构建一次,需重启) | 配置中的任意字符串(**始终启用**) | [参考](https://shunt.dev/zh-cn/reference/configuration/) |
| OpenTelemetry 指标与链路追踪 | `endpoint` 非空的 `[otel]` | [指南](https://shunt.dev/zh-cn/guides/opentelemetry/) |

## 文档

面向用户的文档都在 **[shunt.dev](https://shunt.dev)**:

- [快速开始](https://shunt.dev/getting-started/quickstart/) · [为什么选 shunt?](https://shunt.dev/getting-started/why-shunt/) · [提供方](https://shunt.dev/guides/providers/) · [配置](https://shunt.dev/guides/configuration/) · [故障排查](https://shunt.dev/reference/troubleshooting/)
- **面向 agent:** 每个页面都有一个 Markdown 孪生版本(在任意 URL 后追加 `.md`,或使用页面的 *Copy Markdown* / *Open in AI* 按钮),并且站点按 [llms.txt 规范](https://llmstxt.org/) 发布了 [`/llms.txt`](https://shunt.dev/llms.txt)、[`/llms-small.txt`](https://shunt.dev/llms-small.txt) 和 [`/llms-full.txt`](https://shunt.dev/llms-full.txt)。

面向贡献者的设计笔记和里程碑规范位于 [`docs/`](docs/) —— 从 [`docs/implementation-plan.md`](docs/implementation-plan.md) 开始。

## 为什么

Claude Code 会把每一轮都发送到 Anthropic API。`shunt` 位于前面(通过 `ANTHROPIC_BASE_URL`),针对你映射的模型,将它们的推理分流到另一个提供方(OpenAI、Codex/ChatGPT……)。由于路由发生在 HTTP/推理层 —— 而不是把任务移交给另一个 CLI —— 会话仍在 Claude Code 的框架内运行:相同的工具循环、相同的预加载技能、相同的捆绑脚本路径解析。只有 token 生成被外包出去。

与另一种方案(把 `subagent_type` 移交给像 Codex CLI 这样的另一个运行时)相比,后者在技术栈中切得更高,会丢失人设和预加载技能。

### 按模型,而非按 agent —— 也不是全局替换

选择性由**每个请求上的 `model` id** 驱动,而 Claude Code 本来就允许你按上下文选择它:主会话的 `/model` 选择器、子 agent 定义的 `model:` frontmatter、面向所有子 agent 的 `CLAUDE_CODE_SUBAGENT_MODEL`,或用 `ANTHROPIC_CUSTOM_MODEL_OPTION` 向选择器添加一个自定义条目。因此“只分流这个 agent / 这个会话”是在 Claude Code 中决定的,而 shunt 只是遵从它收到的 model id —— 没有脆弱的按 agent 系统提示指纹识别。与全局模型替换代理不同,主会话可以留在 Claude 上,而只有你指名的模型才被分流。

## Claude Code 集成(官方接口)

Claude Code 在 `ANTHROPIC_BASE_URL` 后暴露了一个**一等公民的网关契约**。`shunt` 实现的正是这个契约,而不是早期 Claude Code 代理所依赖的“对子 agent 的系统提示做哈希”这种脆弱启发式。

- [LLM 网关协议](https://code.claude.com/docs/en/llm-gateway-protocol) —— 该 API 契约规定了端点、需要转发与需要消费的头部和 body 字段、功能透传以及归属信息。运行中的网关会在 `GET /protocol` 提供机器可读的规范。Claude Code 会在系统提示前加上客户端版本和会话指纹;是否抑制它是开发者通过 `CLAUDE_CODE_ATTRIBUTION_HEADER=0` 决定的事,因此 shunt 原样转发该归属块。
- [模型发现](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) —— Claude Code 在启动时查询 `GET /v1/models?limit=1000`(通过 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` 选择加入),并把返回的模型加入 `/model` 选择器。shunt 会返回精选的 `[[models]]` 条目,并在 `auto_include_builtin_models` 仍为 `true` 时附上调用方的实时目录 —— 仅当 `server.default_provider` 为 Anthropic 类型时才会拉取,否则(或缺少凭据、拉取失败时)回退到内置快照。**约束:** `id` 不以 `claude`/`anthropic` 开头的条目会被忽略 —— 非 Claude 模型必须设置别名或手动添加。参见[模型发现](https://shunt.dev/zh-cn/guides/model-discovery/)。
- [添加自定义模型选项](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) —— `ANTHROPIC_CUSTOM_MODEL_OPTION` 会在不替换内置别名的前提下,向 `/model` 选择器添加一个经网关路由的条目;该 ID 不做校验,因此网关接受的任何字符串都可用。鉴于上面的发现约束,**这是选择非 Claude 模型的主要方式**(例如 `gpt-5.6-sol`)。
- **工具搜索**(`ENABLE_TOOL_SEARCH`)—— Claude Code 会延迟加载 MCP/LSP 工具 schema,按需揭示,从而回收上下文。由于 shunt 不是 Anthropic 第一方主机,除非你主动开启,Claude Code 会保持其**关闭**。开启后延迟能否保留取决于上游而不只是设置:`claude*` 和 `anthropic/*` id 会逐字节保留该协议,其他 id 的 `defer_loading` 标记会被剥离(因为这些主机会拒绝),而 Responses 路径有自己的三态 `tool_search` 设置。参见[工具搜索](https://shunt.dev/zh-cn/guides/codex/#工具搜索)。

**设计原则:** 做一个符合规范的 Anthropic-Messages 网关(`/v1/messages`、`/v1/models`、正确的头部与归属透传),按请求的 `model` id 路由,并为已映射的模型在 Anthropic Messages ⇄ OpenAI Responses API 之间做转换 —— 不使用会随 Claude Code 提示变更而失效的提示形状启发式。

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