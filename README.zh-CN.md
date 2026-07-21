# shunt

[![Crates.io](https://img.shields.io/crates/v/shunt-gateway.svg)](https://crates.io/crates/shunt-gateway)
[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![Socket Badge](https://socket.dev/api/badge/cargo/package/shunt-gateway)](https://socket.dev/cargo/package/shunt-gateway)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/crates/l/shunt-gateway.svg)](#license)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · **简体中文**

> 将 Claude Code 分流到任意模型。

`shunt` 是一个符合规范的 [Claude Code LLM 网关](https://code.claude.com/docs/en/llm-gateway-protocol):一个透明代理，针对**你映射的模型**，在**推理层**将推理分流到另一个 LLM 提供方。它按请求的 `model` id 进行路由 —— 其余一切均原样透传给 Anthropic(即“分流”;回退目标可通过 `server.default_provider` 配置)。

名字即机制:电气/铁路中的 *shunt(分流)* 将流量中被选中的部分导向一条并行路径。在这里,被映射模型的推理被分流到另一个提供方,而 Claude Code 的工具和技能保持完好。

它内置了 **OpenAI**、**ChatGPT/Codex**(通过 `codex login` 复用你的订阅)、**xAI**(API 密钥)、**Grok**(通过 `shunt login xai` 复用你的 SuperGrok / X Premium+ 订阅)、**Cursor**(通过 `shunt login cursor` 复用你的订阅)以及 **Anthropic** 透传 —— 而任何兼容 Anthropic-Messages 的后端(Kimi、DeepSeek、GLM、MiniMax、OpenRouter、Vercel AI Gateway……)只需一个 TOML 表即可接入,无需改动代码。

> [!NOTE]
> `shunt` 是仍在活跃开发中的 1.0 之前(pre-1.0)软件。按照 [SemVer](https://semver.org/lang/zh-CN/#spec) 惯例,`0.x` 版本可能包含对配置键、CLI 和行为的破坏性变更(breaking change) —— 升级前请查看[发布说明](https://github.com/pleaseai/shunt/releases)。

## 安装

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo —— crate 名为 `shunt-gateway`;二进制文件仍是 `shunt`
cargo install shunt-gateway
```

预构建二进制文件(macOS/Linux,arm64/x64)附于每个 [GitHub release](https://github.com/pleaseai/shunt/releases)。预构建二进制和从源码构建的说明见 [安装](https://shunt-docs.pages.dev/getting-started/installation/)。

## 快速开始

```toml
# shunt.toml —— 将一个 gpt-* id 路由到你的 ChatGPT 订阅
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

未映射的模型(你所有的 `claude-*` id)会完全照旧工作 —— shunt 使用你自己的凭据将它们转发给 Anthropic。完整演练见 [快速开始](https://shunt-docs.pages.dev/getting-started/quickstart/)。

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

该链先尝试 `anthropic-primary`，再尝试 `codex-fallback`。`auth` 接受 mode 字符串或映射；`claude_oauth` 与 `chatgpt_oauth` 映射可用 `account = "name"` 或 `accounts = [...]` 缩小凭据范围。旧式 `[providers.<name>]` 仍受支持，并会成为按名称排序的隐式上游。不要在配置文件中同时声明两种形式；混用 `[[upstreams]]` 与 `[providers.*]` 会导致配置错误。有关 preset、失败类别和迁移细节，请参阅[配置参考](https://shunt-docs.pages.dev/reference/configuration/)。

**内置:**

| 名称 | 类型 | 认证 | 后端 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | 透传 | `api.anthropic.com` —— 转发调用方自己的凭据 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` —— 复用 `~/.codex/auth.json`(`codex login`) |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` —— 开发者 API,按 token 计费 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` —— Grok CLI 代理;复用 `~/.shunt/xai-auth.json`(使用 SuperGrok / X Premium+ 订阅执行 `shunt login xai`) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` —— 复用 `~/.shunt/cursor-auth.json`(`shunt login cursor`) |

xAI 可能按订阅层级限制 OAuth 访问 —— 如果 `grok` 返回 403,请改用 `xai` API 密钥提供方。详见 [`docs/m6-xai-provider.md`](docs/m6-xai-provider.md)。

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
model = "cursor:gpt-5.5"                             # cursor-plan:<id> / cursor-ask:<id> 选择 agent 模式
provider = "cursor"
```

`cursor:` / `cursor-agent:` / `cursor-plan:` / `cursor-ask:` 前缀用于选择 Cursor 的 agent 模式;后缀是 Cursor 模型 id。详情见 [提供方 → Cursor](https://shunt-docs.pages.dev/guides/providers/#the-cursor-provider-cursor-subscription)。

**任何兼容 Anthropic 的后端**只需一个表即可接入 —— 无需改动代码:

| 提供方 | `base_url` | 示例模型 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`、`deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`、`glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | 见 [MiniMax 文档](https://platform.minimax.io/docs/token-plan/claude-code) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

完整列表和各提供方说明见 [提供方](https://shunt-docs.pages.dev/guides/providers/)。

## 文档

一切都在 **[shunt-docs.pages.dev](https://shunt-docs.pages.dev)**:

- [快速开始](https://shunt-docs.pages.dev/getting-started/quickstart/) · [为什么选 shunt?](https://shunt-docs.pages.dev/getting-started/why-shunt/) · [提供方](https://shunt-docs.pages.dev/guides/providers/) · [配置](https://shunt-docs.pages.dev/guides/configuration/) · [故障排查](https://shunt-docs.pages.dev/reference/troubleshooting/)
- **面向 agent:** 每个页面都有一个 Markdown 孪生版本(在任意 URL 后追加 `.md`,或使用页面的 *Copy Markdown* / *Open in AI* 按钮),并且站点按 [llms.txt 规范](https://llmstxt.org/) 发布了 [`/llms.txt`](https://shunt-docs.pages.dev/llms.txt)、[`/llms-small.txt`](https://shunt-docs.pages.dev/llms-small.txt) 和 [`/llms-full.txt`](https://shunt-docs.pages.dev/llms-full.txt)。

设计笔记和里程碑规范位于 [`docs/`](docs/)(从 [`docs/implementation-plan.md`](docs/implementation-plan.md) 开始)。要将 Claude Code 路由到你的 ChatGPT/Codex 订阅,见 [Codex 配置参考](docs/codex-configuration.md)。

## 为什么

Claude Code 会把每一轮都发送到 Anthropic API。`shunt` 位于前面(通过 `ANTHROPIC_BASE_URL`),针对你映射的模型,将它们的推理分流到另一个提供方(OpenAI、Codex/ChatGPT……)。由于路由发生在 HTTP/推理层 —— 而不是把任务移交给另一个 CLI —— 会话仍在 Claude Code 的框架内运行:相同的工具循环、相同的预加载技能、相同的捆绑脚本路径解析。只有 token 生成被外包出去。

与另一种方案(把 `subagent_type` 移交给像 Codex CLI 这样的另一个运行时)相比,后者在技术栈中切得更高,会丢失人设和预加载技能。

### 按模型,而非按 agent —— 也不是全局替换

选择性由**每个请求上的 `model` id** 驱动,而 Claude Code 本来就允许你按上下文选择它:主会话的 `/model` 选择器、子 agent 定义的 `model:` frontmatter、面向所有子 agent 的 `CLAUDE_CODE_SUBAGENT_MODEL`,或用 `ANTHROPIC_CUSTOM_MODEL_OPTION` 向选择器添加一个自定义条目。因此“只分流这个 agent / 这个会话”是在 Claude Code 中决定的,而 shunt 只是遵从它收到的 model id —— 没有脆弱的按 agent 系统提示指纹识别。与全局模型替换代理不同,主会话可以留在 Claude 上,而只有你指名的模型才被分流。

## Claude Code 集成(官方接口)

Claude Code 在 `ANTHROPIC_BASE_URL` 后暴露了一个**一等公民的网关契约** —— `shunt` 实现的是这个契约,而不是早期 Claude Code 代理所依赖的脆弱的“对子 agent 系统提示做哈希”启发式方法。

- [LLM 网关协议](https://code.claude.com/docs/en/llm-gateway-protocol) —— API 契约:端点、需转发 vs 消费的头部/正文字段、特性透传以及归属信息。运行中的网关在 `GET /protocol` 提供机器可读的规范。
  - [模型发现](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) —— Claude Code 在启动时查询 `GET /v1/models?limit=1000`(通过 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` 主动启用),并将返回的模型加入 `/model` 选择器。**约束:** `id` 不以 `claude`/`anthropic` 开头的条目会被忽略 —— 非 Claude 模型必须设置别名或手动添加。
  - **系统提示归属块** —— Claude Code 会在系统提示前添加一段客户端版本 + 会话指纹;在会话生命周期内保持稳定(v2.1.181+)。`shunt` 原样转发它(从不剥除 —— 那是开发者通过 `CLAUDE_CODE_ATTRIBUTION_HEADER=0` 决定的事)。
- [添加自定义模型选项](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) —— `ANTHROPIC_CUSTOM_MODEL_OPTION` 向 `/model` 选择器添加一个走网关路由的条目,而不替换内置别名;其 ID 跳过校验,因此任何网关接受的字符串都有效。**这是选择非 Claude 模型的主要方式**(例如 `gpt-5.6-sol`),因为发现会忽略不以 `claude`/`anthropic` 开头的 id。

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

- [Greptile](https://www.greptile.com/) — 依据其 OSS 计划，对非商业 MIT/Apache 项目免费。
- [cubic](https://cubic.dev/) — 对公开仓库免费。

## 许可证

在 [Apache License, Version 2.0](LICENSE-APACHE) 或 [MIT license](LICENSE-MIT) 之间任选其一进行许可。除非你明确另行声明,否则任何由你有意提交、以纳入本 crate 的贡献(如 Apache-2.0 许可证所定义)均应按上述方式双重许可,不附加任何额外条款或条件。

---

Made with Orca 🐋

- https://github.com/stablyai/orca
- https://www.onorca.dev/
