---
title: ChatGPT / Codex
description: 通过复用 ~/.codex/auth.json 将 Claude Code 推理路由到你的 ChatGPT/Codex 订阅 —— 认证、模型 slug、力度和上下文窗口。
---

**`codex`** 提供方将映射模型的推理路由到你的 **ChatGPT / Codex
订阅**,而不是 API 密钥。它复用 Codex CLI 已写入
`~/.codex/auth.json` 的凭据,因此无需粘贴任何东西,也没有按 token 计费 —— 请求以你的 ChatGPT 账户进行认证,并由 `codex` CLI 所交谈的同一后端应答。

本页是端到端的设置。它链接到更深入的主题页面
([力度与上下文](/zh-cn/guides/effort-and-context/)、[模型发现](/zh-cn/guides/model-discovery/)、
[提供方](/zh-cn/guides/providers/)),而不是重复它们。

## 工作原理

`codex` 是一个内置的 **`kind = "responses"`** 提供方:shunt 将 Claude Code 的 Anthropic
Messages 请求转换为 OpenAI **Responses API**,发送到 ChatGPT 账户的 Codex 后端,
并将流式回复转换回来。三点使其成为“Codex”而非普通的 OpenAI:

| 方面 | 值 |
| :-- | :-- |
| 端点 | `<base_url>/codex/responses` |
| 认证 | 来自 `~/.codex/auth.json` 的 ChatGPT OAuth,自动刷新 |
| Responses 方言 | `Chatgpt` 风格 —— 丢弃 codex 从不发送的参数(如 `max_output_tokens`),发送 `store: false`,往返传递加密的推理 |

该方言由 `auth = "chatgpt_oauth"` 决定,而非提供方名称。

## 1. 登录

用 Codex CLI 登录一次。shunt 读取并刷新它所写入的文件 —— 它**不会**
为 Codex 运行自己的登录。

```bash
codex login
```

这会创建 `~/.codex/auth.json`。如果该文件缺失、没有 token,或其刷新 token
已失效,shunt 会返回一个 `authentication_error`,提示你再次运行 `codex login`。

:::note[一个不同的 auth 文件位置]
shunt 先查看 `$CODEX_AUTH_FILE`,然后是 `$HOME/.codex/auth.json`,然后是 `.codex/auth.json`。
为 CI、沙箱或第二个账户将其指向别处:

```bash
export CODEX_AUTH_FILE=/etc/shunt/codex-auth.json
```
:::

## 2. 提供方块(可选)

`codex` 是内置的 —— 你无需声明它。以下是完整的默认值;部分表
只覆盖你设置的键(配置映射深度合并):

```toml
[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"   # shunt 追加 /codex/responses
auth = "chatgpt_oauth"                          # 读取 + 自动刷新 ~/.codex/auth.json
# effort = "high"                               # 可选的默认推理力度(§4)
# count_tokens = "tiktoken"                      # 默认;"estimate" 表示退出
```

常见覆盖:为所有 Codex 流量固定一个默认 `effort`,或设置
`count_tokens = "estimate"`。`api_key_env` / `api_key_header` 不适用于 `chatgpt_oauth` ——
凭据来自 auth 文件。每个键见 [配置参考](/zh-cn/reference/configuration/#providersname)。

:::note[ApiKey 模式走 `openai` 提供方]
如果 `~/.codex/auth.json` 处于 **`ApiKey`** 模式(你用 OpenAI API 密钥登录,而非
ChatGPT 账户),`codex` OAuth 路径将找不到 token 并报错。该密钥反而会在 `OPENAI_API_KEY` 未设置时,
作为回退被 **`openai`** 提供方拾取。`codex`
专门是 ChatGPT 订阅路径。
:::

## 3. 将一个模型路由到 `codex`

请求的 `model` id 选取提供方。优先级:精确 `[[routes]]` →
`[[route_prefixes]]` → `server.default_provider`。

```toml
[[routes]]
model = "gpt-5.6-sol"        # Claude Code 发送的 id(见下方 §4)
provider = "codex"
# upstream_model = "gpt-5.6-sol"   # 可选:向上游转发一个不同的 slug
# effort = "high"                  # 可选:为该路由固定力度
```

`upstream_model` 让 Claude Code 发送的 id 可以不同于后端接收的 slug —— 这是
[发现别名](/zh-cn/guides/model-discovery/) 背后的机制,也是一种无需改动 Claude Code 环境
即可替换真实 slug 的方法。

:::caution[模型 slug —— 不带 `-codex`]
ChatGPT 账户后端**拒绝** `gpt-*-codex` slug(例如 `gpt-5.2-codex`),返回 `400`;
它只接受你账户的**实时授权** slug。权威目录(以及每个 slug 接受的
推理级别)是 openai/codex 的
[`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)。
当前 slug:`gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`(前沿)以及
`gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`。较老的账户可能只被授权使用
较早的那些(一个免费账户会解析到 `gpt-5.5`)。shunt 会透出后端自身的错误
`detail`,因此错误的 slug 会返回真实原因。
:::

:::note[`Model not found <slug>` 是客户端版本门控,不是授权问题]
一些 slug 带有 `minimal_client_version`(例如 `gpt-5.6-luna` 需要 ≥ 0.144.0)。当
请求的客户端身份缺失或过旧时,后端会应答 `Model not found <slug>`。
shunt 通过发送固定的 Codex CLI 身份头(`originator: codex_cli_rs`、
`user-agent`、`version`)来避免这一点,固定到 **openai/codex rust-v0.144.1**。见
[openai/codex#31967](https://github.com/openai/codex/issues/31967)。
:::

## 4. 在 Claude Code 中选择模型

Claude Code 的 `/model` 选择器只认以 `claude`/`anthropic` 开头的发现 id,因此一个
裸的 `gpt-*` id 需要两条路径之一 —— 它们以 `claude-` 前缀分界,互不重叠:

| | `claude-…` 发现别名 | 非 `claude-` id(`gpt-5.6-sol`) |
| :-- | :-- | :-- |
| 通过发现进入 `/model` 选择器 | ✅ 自动列出,多个模型 | ❌ 被 Claude Code 丢弃 |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ 不生效 | ✅ 加入选择器(一个 id) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 窗口 | ❌ 忽略 → 200k | ✅ 真实窗口 |

**主要路径** —— 直接把 slug 加入选择器:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

该 id 正是 shunt 路由所依据的,因此它必须匹配一条 `[[routes]]`/`[[route_prefixes]]`
规则。这是推荐路径 —— 它是唯一也能让你设置准确上下文
窗口的路径。要改为在选择器中自动列出多个 Codex 模型,请使用一个 `claude-` 命名的
[发现别名](/zh-cn/guides/model-discovery/)(接受 200k 窗口的取舍)。

#### 让一个子 agent 用上 Codex slug

一个子 agent 可以运行在 Codex slug 上,而主会话仍留在 Claude。`model:` frontmatter
字段接受**任意字符串**(不同于 Agent/Task 工具的 `model` 参数,后者只接受
内置别名)。要将一个**现有**子 agent 指向 `gpt-5.6-sol`,编辑其
`.claude/agents/<name>.md` 并设置 `model:`:

```markdown
---
name: researcher
description: Deep research agent.
model: gpt-5.6-sol        # 原为:sonnet(或缺省 → 继承)
---

<the agent's system prompt — unchanged>
```

生成它时**不带** `model` 覆盖(工具参数优先于 frontmatter)。解析顺序:
`CLAUDE_CODE_SUBAGENT_MODEL` > 工具 `model` > frontmatter > `inherit`。要强制**每个**子 agent
都用同一个 slug,设置 `export CLAUDE_CODE_SUBAGENT_MODEL="gpt-5.6-sol"`。

无论哪种方式,该 slug 都需要一条 `[[routes]]` 条目,并且由于是非 `claude-`,它遵从
`CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` 和 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` —— 窗口会自动
跟随 id。

:::tip[现成的 agent]
**[`shunt-codex` 插件](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-codex)**
提供了面向 `gpt-5.6-sol` / `-terra` / `-luna` 的子 agent —— 在 `/plugin marketplace add pleaseai/shunt`
之后用 `/plugin install shunt-codex@shunt` 安装。
:::

### 将分层别名重映射到 Codex

除了添加一个自定义 id,你还可以将 Claude Code 的**内置分层别名**重新指向 Codex slug,
使整个会话的分层系统解析到你的 ChatGPT 订阅
([model-config 环境变量](https://code.claude.com/docs/en/model-config#environment-variables))。

| 环境变量 | 控制 |
| :-- | :-- |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | `haiku` 别名**以及后台“小而快”模型** |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | `sonnet` 别名 |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_FABLE_MODEL` | `opus` / `fable` 别名 |

一个双层设置 —— `haiku → gpt-5.6-luna`,`sonnet → gpt-5.6-sol`:

```bash
export ANTHROPIC_DEFAULT_HAIKU_MODEL="gpt-5.6-luna"
export ANTHROPIC_DEFAULT_SONNET_MODEL="gpt-5.6-sol"

# 更好看的选择器标签(_NAME/_DESCRIPTION 配套项在网关上有效)
export ANTHROPIC_DEFAULT_SONNET_MODEL_NAME="GPT-5.6-Sol"
export ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION="ChatGPT/Codex Sol via shunt"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME="GPT-5.6-Luna"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION="ChatGPT/Codex Luna via shunt (background tier)"
```

```toml
# shunt.toml —— 两个解析后的 id 都需要一条路由
[[routes]]
model = "gpt-5.6-luna"
provider = "codex"

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

现在在 `/model` 中选择 **Sonnet** 会通过 Codex 运行 `gpt-5.6-sol`,而每个后台/haiku 任务
运行 `gpt-5.6-luna` —— 解析后的 id 正是 shunt 路由所依据的,因此不需要
`ANTHROPIC_CUSTOM_MODEL_OPTION`。

:::note[把它做对]
- 解析后的 id 不以 `claude-` 开头,因此为力度旋钮设置 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`。
  `gpt-5.6-sol` 和 `gpt-5.6-luna` **都是 372k**,因此一个全局的
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000` 同时适配两层。
- `_SUPPORTED_CAPABILITIES` 配套项是为第三方提供方(Bedrock……)记载的,
  在网关上未确认 —— 在 shunt 上请用 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` 控制力度。
- **haiku 层是后台“小而快”模型**(摘要、标题、快速分类)。
  把它路由到一个推理模型没问题,但这会在那些频繁流量上消耗 ChatGPT 配额,
  且可能更慢 —— 如果这一点重要,请在那里选用你最便宜的已授权 slug。
- 重映射是**全局且会话级**的;有了允许列表(`availableModels` /
  `enforceAvailableModels`),别名无法被重定向到列表之外(自 **v2.1.176** 起,
  Claude Code 会对分层别名环境变量强制执行这一点)。
:::

## 5. 推理力度

用 Claude Code 的常规控制项设置力度(`/effort`、`/model` 滑块、`--effort`)。
shunt 将其映射到 Responses 的 `reasoning.effort`,对不支持 `max` 的 slug 将
`max → xhigh` 折叠(只有 **gpt-5.6** 系列支持)。

:::note[自定义 id 必需]
对于 Claude Code 不识别为具备力度能力的 id(如 `gpt-5.6-sol`),你必须设置:

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```

否则 Claude Code 会省略力度字段,shunt 回退到 `medium`。配置中的
`route.effort` / `[providers.codex].effort` 覆盖会胜过客户端值。
:::

完整优先级和力度表:[力度与上下文](/zh-cn/guides/effort-and-context/#reasoning-effort)。

## 6. 上下文窗口

Claude Code 为映射的 id 将其上下文栏固定标定为 **200k**。`gpt-5.6-sol` 的真实窗口
是 **372k**(`gpt-5.5` 是 272k),因此为一个非 `claude-` id 提高它:

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

它是**全局**的(每个会话一个值),将其设置得大于真实窗口会导致
`prompt is too long` 溢出反复折腾 —— 请把它匹配到你映射模型中最小的真实窗口。
shunt 会重写该溢出,使 Claude Code 自动压缩并重试,但每次往返
都是浪费的延迟。详情、实测边界以及 `count_tokens` 行为见:
[力度与上下文](/zh-cn/guides/effort-and-context/#context--usage-display-for-mapped-models)。

## 完整示例

`shunt.toml`:

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"

[providers.codex]
effort = "high"     # 可选:为所有 Codex 流量固定 high 力度

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

Shell(shunt 和 Claude Code 都带这些运行):

```bash
codex login                                          # 一次性
./target/release/shunt run                           # 启动网关

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"   # 加入 /model 选择器
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1            # 让力度滑块触达 Codex
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000         # gpt-5.6-sol 的真实窗口
```

从 `/model` 选择 **gpt-5.6-sol**。会话中的其他一切仍原样流向 Anthropic;
只有映射模型的推理由你的 ChatGPT/Codex 订阅应答。

## 网页搜索

Claude Code 内置的 **网页搜索** 无需额外设置即可通过 Codex 路径工作。启用后,Claude Code 会发送托管的
`web_search_20250305` 工具,shunt 将其注册为 Responses API 的托管 **`web_search`** 工具,因此搜索会在
后端真正执行,而不是作为一次未完成的工具调用被退回。

- 域名过滤器会一并传递 —— Claude Code 的 `allowed_domains` / `blocked_domains` 会成为 Responses
  `web_search` 的 `filters`。
- 适用于 `codex`(ChatGPT)与 `openai`(标准 Responses)提供方。
- **xAI / Grok 路由不支持** —— Grok 的 Responses API 只接受函数工具,因此 shunt 会在那里丢弃托管的网页
  搜索工具;网页搜索请使用 `codex` 或 `openai` 路由。

## 工具搜索

Claude Code 的 **工具搜索** —— 延迟 MCP / LSP 工具的 schema,仅在需要时通过 `ToolSearch` 工具揭示,
从而不让模型把上下文花在从不调用的工具上 —— 同样可以通过 Codex 路径工作,但在 shunt 背后 **默认关闭**。
启用方式:

```bash
export ENABLE_TOOL_SEARCH=true
```

当 base URL 不是第一方 Anthropic 主机时,Claude Code 会禁用其乐观式工具搜索,而 shunt 并非第一方主机。
因此若不设置此标志,从第一轮起每个工具的完整 schema 都会发送到上游,该功能形同虚设(仍能工作,但没有任何
节省)。客户端自身的约定是:**如果你的代理会转发 `tool_reference` 块**,就设置
`ENABLE_TOOL_SEARCH=true` —— shunt 正是这样做的。

启用后,Claude Code 只在提示中列出可延迟工具的 **名称**,而保留其 schema。shunt 会把这些尚未加载的工具
排除在上游工具集之外,直到模型通过 `ToolSearch` 加载某个工具;随之产生的 `tool_reference` 便按需揭示该
工具的完整 schema。这样就收回了被延迟的 schema 从第一轮起占用的上下文窗口 —— 这正是工具搜索的意义所在。

- 无需改动 `shunt.toml` —— 它纯粹是一个 Claude Code 环境变量。
- 适用于 `codex`(ChatGPT)与 `openai`(标准 Responses)提供方。
- 不延迟的工具(以及上文的托管 `web_search` 工具)始终会被转发;仅可延迟的工具才会被渐进揭示。

### 可选开启的原生协议

上面的 shim 通过把 `tool_reference` 渲染为 schema 文本来工作 —— 它不会从上游上下文中收回任何东西,只是
推迟了完整 schema *何时* 被发送。作为一个**可选开启的替代方案**(issue #82),shunt 可以改为把工具搜索
映射到 OpenAI Responses API 自身的**原生、客户端执行的 `tool_search`**协议:Claude Code 的 `ToolSearch`
工具变为 `tool_search`(`execution: "client"`)工具,其 `tool_use` 变为 `tool_search_call`,而
`tool_reference` 结果变为一个 `tool_search_output` 条目,以结构化 JSON 携带已加载工具的完整 schema ——
从而保留真实的工具加载语义和缓存行为,而不是把 schema 折叠进文本。按提供方启用:

```toml
[providers.codex]
tool_search = true
```

要求 —— 不受支持的组合会静默保留 #43 的 shim,不会报错:

- 上游必须是标准 OpenAI 或 ChatGPT/Codex 风格的 Responses 后端。xAI / Grok 路由始终保留 shim。
- 路由到的模型必须是 **gpt-5.4 及以上**(`gpt-5.4`、`gpt-5.5`,或 `gpt-5.6` 系列)。更早的 slug
  (`gpt-5.2` 及以下)即便设置了 `tool_search = true` 也会回退到 shim。
- Claude Code 一侧仍然需要 `ENABLE_TOOL_SEARCH=true` —— 这个标志只改变 shunt *如何* 把该功能转译到
  上游,不改变 Claude Code 是否延迟工具本身。

`tool_search` 默认是 `false`:原生形状被这个标志门控,直到有一次实时探测确认某个后端确实接受它们为止,
因此这是按提供方显式的可选开启,而不是 shunt 自动切换所有 Codex/OpenAI 路由。

## 故障排查

| 症状 | 原因 / 修复 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | 没有 `~/.codex/auth.json`(或 `$CODEX_AUTH_FILE` 错误)。运行 `codex login`。 |
| `ChatGPT auth tokens missing` | auth 文件处于 `ApiKey` 模式 —— 那是 `openai` 提供方。用 ChatGPT 账户重新 `codex login`。 |
| `400 … not supported when using Codex with a ChatGPT account` | 你用了 `gpt-*-codex` slug。使用一个已授权的非 `-codex` slug。 |
| `Model not found <slug>` | 客户端版本门控或一个未授权的 slug —— 通过 `models.json` 确认。 |
| `gpt-*` id 上力度滑块被忽略 | 设置 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`,或是某个路由/提供方的 `effort` 覆盖项胜出。 |
| 上下文栏读数过高 / 过早压缩 | 设置 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`;发现别名无法接收它 —— 请用一个非 `claude-` id。 |
| Grok 路由上网页搜索无结果 | xAI/Grok 的 Responses API 不支持网页搜索,shunt 会丢弃该工具。网页搜索请使用 `codex` 或 `openai` 路由。 |
| 工具搜索无效 / 每轮都发送全部工具 schema | 设置 `ENABLE_TOOL_SEARCH=true` —— Claude Code 在非第一方 base URL 背后默认禁用工具搜索。shunt 会转发 `tool_reference` 块并按需揭示延迟的 schema。 |
| 想让工具搜索真正收回上下文,而不只是推迟发送时机 | 为原生协议在 `[providers.codex]` 下设置 `tool_search = true` —— 需要标准 OpenAI/ChatGPT-Codex 风格,且模型为 gpt-5.4 及以上;见上文 [工具搜索 → 可选开启的原生协议](#可选开启的原生协议)。 |

更多内容见完整的 [故障排查](/zh-cn/reference/troubleshooting/) 参考。
