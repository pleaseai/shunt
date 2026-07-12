---
title: xAI / Grok
description: 将 Claude Code 推理路由到 xAI 的 Grok —— 使用你的 SuperGrok / X Premium+ 订阅(grok 提供方,OAuth),或使用 xAI 开发者 API(xai 提供方,API 密钥)。
---

两个内置提供方将 Claude Code 路由到 xAI 的 **Grok** 模型。它们的区别仅在于**如何
认证以及命中哪个 xAI 面向端** —— 二选一:

| 提供方 | 认证 | 后端 | 计费 |
| :-- | :-- | :-- | :-- |
| **`grok`** | `xai_oauth` —— 你的 **SuperGrok / X Premium+** 登录 | `cli-chat-proxy.grok.com/v1`(Grok CLI 聊天代理) | 你的订阅 —— 无按 token 计费 |
| **`xai`** | `api_key` (`XAI_API_KEY`) | `api.x.ai/v1`(开发者 API) | 计量的 API 额度 |

两者都是讲 xAI Responses 方言的 **`kind = "responses"`** 提供方 —— 与 [Codex](/zh-cn/guides/codex/)
相同的转换路径,本页与其保持一致。它链接到更深入的主题页面
([力度与上下文](/zh-cn/guides/effort-and-context/)、[模型发现](/zh-cn/guides/model-discovery/)、
[提供方](/zh-cn/guides/providers/)),而不是重复它们。

:::caution[两条路径不可互换]
**订阅** bearer 只对 **`grok`** 代理有效 —— 开发者 API(`api.x.ai`)
会以 `402`(`personal-team-blocked:spending-limit`,*"…need a Grok subscription…"*)拒绝它。
**API 密钥** 只对 **`xai`** 有效。请将你的 Grok slug 路由到与你所持凭据相匹配的那个提供方。
:::

## 工作原理

shunt 将 Claude Code 的 Anthropic Messages 请求转换为 OpenAI **Responses API**,发送
到 xAI,并将流式回复转换回来。`xai` **Responses 风格**(丢弃 xAI
拒绝的参数,将工具保留为函数工具)由两种方式选中:通过 **`api.x.ai` 主机**,或通过
**`auth = "xai_oauth"`**(`grok` 代理不是 x.ai 主机,因此其方言以 auth 为键)。

| 方面 | `grok`(订阅) | `xai`(API 密钥) |
| :-- | :-- | :-- |
| 端点 | `cli-chat-proxy.grok.com/v1/responses` | `api.x.ai/v1/responses` |
| 认证 | 来自 `~/.shunt/xai-auth.json` 的 Grok CLI OAuth,自动刷新 | `Bearer $XAI_API_KEY` |
| 身份头 | Grok CLI 头(`x-xai-token-auth`、`x-grok-client-identifier`、`x-grok-client-version`),使代理认可该订阅 | 无 |

:::note[跨源 token 防护]
一个 `xai_oauth` 提供方只会将其订阅 bearer 发送到 **x.ai 或 grok.com 主机、且经由
HTTPS**,并且仅在 `kind = "responses"` 时。将其指向别处,`shunt check` 就会拒绝它 ——
shunt 不会把订阅 token 泄露给任意 `base_url`。
:::

## 路径 A —— SuperGrok 订阅(`grok`)

### 1. 登录

运行 shunt 自己的设备码登录(RFC 8628)。它会打印一个 URL 和一个码;在任意设备的
浏览器中批准 —— 无需回环回调服务器:

```bash
shunt login xai
```

成功后 shunt 会将 token 以 `0600` 权限写入 **`~/.shunt/xai-auth.json`**,并自动
刷新它们(5 分钟的过期缓冲;xAI 在每次刷新时轮换刷新 token,因此 shunt
在单飞锁下持久化轮换后的那个)。如果刷新 token 丢失或响应
省略了轮换后的 token,shunt 会提示你再次运行 `shunt login xai`。

:::note[一个不同的 auth 文件位置]
为 CI、沙箱或第二个账户用 `$SHUNT_XAI_AUTH_FILE` 覆盖路径:

```bash
export SHUNT_XAI_AUTH_FILE=/etc/shunt/xai-auth.json
```
:::

### 2. 提供方块(可选)

`grok` 是内置的 —— 你无需声明它。以下是完整的默认值;部分表
只覆盖你设置的键(配置映射深度合并):

```toml
[providers.grok]
kind = "responses"
base_url = "https://cli-chat-proxy.grok.com/v1"   # shunt 追加 /responses
auth = "xai_oauth"                                # 读取 + 自动刷新 ~/.shunt/xai-auth.json
# effort = "high"                                  # 可选 —— 选择性启用推理力度(§ 推理力度)
```

### 3. 将一个模型路由到 `grok`

```toml
[[routes]]
model = "grok-4.5"
provider = "grok"
# upstream_model = "grok-4.5"   # 可选:向上游转发一个不同的 slug
```

## 路径 B —— xAI 开发者 API(`xai`)

### 1. 导出密钥

```bash
export XAI_API_KEY=xai-…
```

### 2. 提供方块(可选)

```toml
[providers.xai]
kind = "responses"
base_url = "https://api.x.ai/v1"   # shunt 追加 /responses
auth = "api_key"
api_key_env = "XAI_API_KEY"
```

### 3. 将一个模型路由到 `xai`

```toml
[[routes]]
model = "grok-4.5"
provider = "xai"
```

:::caution[需要 API 额度,而非订阅]
`api.x.ai` 按你的 xAI **API** 额度计费。SuperGrok / X Premium+ 订阅**不**
授权开发者 API —— 一个未充值的账户会返回 `402 Payment Required`
(*"You have run out of credits or need a Grok subscription…"*)。请在
[console.x.ai](https://console.x.ai/) 充值,或改用**路径 A**来消费你的订阅。
:::

## 模型 slug

slug 目录是 **xAI 的**,而非 shunt 的 —— shunt 转发你所路由的任意 slug。当前的
编码/前沿 slug 有 `grok-4.5`、`grok-4.3` 和 `grok-build-0.1`。在路由中用 `upstream_model`
可将一个别名映射到一个实时 slug,而无需改动你的 Claude Code 环境。(模型
[发现](/zh-cn/guides/model-discovery/) 只会透出你声明的 `claude-` 命名别名,因此无法列出原始 Grok
slug —— 请通过下文的 `ANTHROPIC_CUSTOM_MODEL_OPTION` 或分层重映射来访问。)

## 在 Claude Code 中选择模型

Grok slug 不以 `claude-` 开头,因此 Claude Code 的 `/model` 选择器不会从发现中列出它们。
其机制与 **Codex 完全相同** —— 直接把 id 加入选择器:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="grok-4.5"   # 必须匹配一条 [[routes]] 规则
```

同一节 [Codex 章节](/zh-cn/guides/codex/#4-在-claude-code-中选择模型) 逐字覆盖其余内容:
通过 `model:` frontmatter 将一个**子 agent** 置于某个 Grok slug 上,以及将**分层别名**
(`ANTHROPIC_DEFAULT_SONNET_MODEL`……)重映射到 Grok slug 以贯穿整个会话。

:::tip[现成的 agent]
**[`shunt-xai` 插件](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-xai)** 提供了
面向 `grok-4.5` / `grok-4.3` / `grok-build-0.1` 的子 agent —— 在
`/plugin marketplace add pleaseai/shunt` 之后用 `/plugin install shunt-xai@shunt` 安装。每个 agent
固定其 `model:`,因此只有该子 agent 会分流;主会话仍留在 Claude。按你所持的凭据将 slug 路由到
`grok` 或 `xai`(在 `shunt.toml` 中)。
:::

## 推理力度

**与 Codex 不同,力度对 Grok 是选择性启用的。** 若干 Grok 模型(`grok-4*`、`grok-3`、
`grok-code-fast`……)在遇到 `reasoning.effort` 字段时会返回 `400`,即便它们原生就会推理,因此
shunt **仅在你于提供方或路由上配置它时**(或按请求传入时)才发送该旋钮 ——
否则模型使用其原生推理:

```toml
[providers.grok]
effort = "high"        # 应用于所有 grok 流量

# ……或按路由
[[routes]]
model = "grok-4.5"
provider = "grok"
effort = "high"
```

`grok-4.5` 接受 `reasoning.effort`(已实测)。对任何在其上 `400` 的 slug,请让 `effort`
保持未设置。完整优先级和力度表:[力度与上下文](/zh-cn/guides/effort-and-context/#推理力度)。

## 上下文窗口

Claude Code 为映射的 id 将其上下文栏固定标定为 **200k**。如果你的 Grok slug 的真实
窗口更大,请提高它 —— 该值会自动跟随一个非 `claude-` 的 id:

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=256000   # 设为你的 slug 的真实窗口,按 xAI 的模型文档
```

它是**全局**的(每个会话一个值);请将其设置为你映射模型中最小的真实窗口,
因为超过某个模型的真实窗口会导致 `prompt is too long` 溢出反复折腾。详情和
`count_tokens` 行为见:[力度与上下文](/zh-cn/guides/effort-and-context/#映射模型的上下文--用量显示)。

## 网页搜索

Claude Code 内置的**网页搜索在 Grok 路由上不生效。** xAI 的 Responses API 只接受
函数工具,因此 shunt 会在 `xai` 风格上(`grok` 和 `xai` 均如此)丢弃托管的 `web_search` 工具。
当你需要托管的网页搜索时,请使用 [`codex` 或 `openai` 路由](/zh-cn/guides/codex/#网页搜索)。

## 完整示例(订阅路径)

`shunt.toml`:

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"

[providers.grok]
effort = "high"     # 可选:为所有 Grok 流量选择性启用推理力度

[[routes]]
model = "grok-4.5"
provider = "grok"
```

Shell(shunt 和 Claude Code 都带这些运行):

```bash
shunt login xai                                     # 一次性设备码登录
./target/release/shunt run                          # 启动网关

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="grok-4.5"     # 加入 /model 选择器
```

从 `/model` 选择 **grok-4.5**。会话中的其他一切仍原样流向 Anthropic;
只有映射模型的推理由你的 SuperGrok 订阅应答。

## 故障排查

| 症状 | 原因 / 修复 |
| :-- | :-- |
| 启动时出现 `run shunt login xai` | 没有 `~/.shunt/xai-auth.json`(或 `$SHUNT_XAI_AUTH_FILE` 错误)。运行 `shunt login xai`。 |
| `xAI refresh response missing refresh_token; run shunt login xai` | 存储的刷新 token 已被消费/轮换掉。重新登录。 |
| `402 … personal-team-blocked:spending-limit` / *"need a Grok subscription"* | 在 **`xai`**(开发者 API)路径上且无 API 额度。请在 [console.x.ai](https://console.x.ai/) 充值,或路由到 **`grok`** 以使用你的订阅。 |
| `403 … not authorized for API access`(订阅层级门控) | 在 **`grok`** 路径上,你的订阅层级不包含 API 访问 —— **重新登录也无济于事**。设置 `XAI_API_KEY` 并使用 `xai` 路径,或在 [x.ai/grok](https://x.ai/grok) 升级。 |
| `refusing to send a subscription token off-origin`(来自 `shunt check`) | 一个 `xai_oauth` 提供方的 `base_url` 主机不是 `x.ai`/`grok.com`,不是 HTTPS,或不是 `kind = "responses"`。修正该块。 |
| 设置力度后出现 `400` | 该 Grok slug 拒绝 `reasoning.effort`。为其从提供方/路由中移除 `effort`。 |
| `model <slug> is not enabled for this account` | 未授权的 slug —— 对照 xAI 的目录确认该 slug。 |
| 网页搜索返回空 | Grok 路由不支持;shunt 会丢弃该工具。请使用 `codex`/`openai` 路由。 |

更多内容见完整的 [故障排查](/zh-cn/reference/troubleshooting/) 参考。
