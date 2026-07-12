---
title: 提供方
description: 内置的提供方,以及如何用一个 TOML 表添加任意兼容 Anthropic 的后端。
---

提供方是一个**名称 → 配置的映射**:一个新的上游只不过是又一个 `[providers.<name>]` 表 —— 无需改动代码。三种适配器类型即可覆盖一切:

- **`kind = "anthropic"`** —— 上游讲 Anthropic Messages API。shunt 将请求透传,可选择注入一个不同的 API 密钥。
- **`kind = "responses"`** —— 上游讲 OpenAI Responses API。shunt 在 Anthropic Messages ⇄ Responses 之间转换,含流式传输。
- **`kind = "cursor"`** —— 原生 Cursor 适配器。shunt 将 Cursor 的 ConnectRPC/protobuf AgentService(及其工具协议)桥接到 Anthropic Messages API,含流式传输。由内置的 `cursor` 提供方使用。

## 内置提供方

| 名称 | 类型 | 认证 | 后端 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` —— 转发调用方自己的凭据 |
| `openai` | `responses` | `api_key` (`OPENAI_API_KEY`) | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` —— 复用 `~/.codex/auth.json` |
| `xai` | `responses` | `api_key` (`XAI_API_KEY`) | `api.x.ai/v1` —— xAI 开发者 API |
| `grok` | `responses` | `xai_oauth` | `cli-chat-proxy.grok.com/v1` —— 通过 `shunt login xai` 使用 SuperGrok / X Premium+ 订阅 |
| `cursor` | `cursor` | `cursor_oauth` | `api2.cursor.sh` —— 复用 `~/.shunt/cursor-auth.json`(`shunt login cursor`) |

### codex 提供方(ChatGPT 订阅)

用 Codex CLI 登录一次;shunt 会读取并自动刷新 `~/.codex/auth.json`:

```bash
codex login
```

如果该文件缺失或过期,shunt 会返回一个 `authentication_error`,提示你运行 `codex login`。

完整设置 —— auth 文件处理、模型选择、力度以及上下文大小 —— 见专门的 [ChatGPT / Codex 指南](/zh-cn/guides/codex/)。

:::caution[模型 slug]
ChatGPT 账户的 Codex 后端**拒绝** `gpt-*-codex` slug —— 它只接受该账户实时授权的 slug。权威目录是 openai/codex 的 [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)。当前的 slug 有 `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`(前沿)以及 `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`;较老的账户可能只被授权使用较早的那些。在路由中使用 `upstream_model` 可将任意别名映射到一个已授权的 slug。
:::

### cursor 提供方(Cursor 订阅)

内置的 `cursor` 提供方通过 Cursor 自己的 ConnectRPC/protobuf AgentService(`api2.cursor.sh`)访问你的 **Cursor** 订阅 —— `kind = "cursor"` 原生适配器在其与 Anthropic Messages 之间双向转换,含流式传输和 Cursor 的原生工具调用。登录一次:

```bash
shunt login cursor
```

这会运行 Cursor OAuth 流程并写入 `~/.shunt/cursor-auth.json`,shunt 会读取并自动刷新它。如果该文件缺失或过期,shunt 会返回一个 `authentication_error`,提示你运行 `shunt login cursor`。

将一个 `cursor:*` 模型 id 路由到它 —— 该提供方默认已预置,因此无需 `[providers.cursor]` 表:

```toml
[[routes]]
model = "cursor:gpt-5.5"
provider = "cursor"
```

**模型 id 与 agent 模式。** 前缀用于选择 Cursor 的 agent 模式,后缀是 Cursor 模型 id:

| 形式 | Agent 模式 | 示例 |
| :-- | :-- | :-- |
| `cursor:<id>` / `cursor-agent:<id>` | Agent | `cursor:gpt-5.5` |
| `cursor-plan:<id>` | Plan | `cursor-plan:gpt-5.5` |
| `cursor-ask:<id>` | Ask | `cursor-ask:gpt-5.5` |

同样接受旧式的裸名称:`cursor`、`cursor-agent`、`cursor-composer`、`cursor-composer-fast`(Agent);`cursor-plan`、`composer-2.5`(Plan);`cursor-ask`、`composer-2.5-fast`(Ask)。任何其他模型 id 都会以 `invalid_request_error` 被拒绝。

:::note[覆盖项]
`SHUNT_CURSOR_BASE_URL` 覆盖端点,`SHUNT_CURSOR_AUTH_FILE` 覆盖凭据路径,`SHUNT_CURSOR_CLIENT_VERSION` 覆盖 `x-cursor-client-version` 头部(若 Cursor 开始拒绝过时的客户端版本,可无需重新构建即予以提升)。`cursor_oauth` 提供方被固定到某个 Cursor 主机并使用 HTTPS —— 将 `base_url` 指向非同源地址会被拒绝,以防 bearer token 泄露。
:::

:::caution[由你自己决定]
从非官方客户端复用 Cursor 订阅由你自己决定 —— 这可能违反 Cursor 的条款或触发账号层面的处置。使用风险自负。
:::

### xai / grok 提供方(Grok)

两个内置提供方触达 xAI 的 **Grok** 模型,按凭据划分:**`grok`** 经由 OAuth 消费你的
**SuperGrok / X Premium+** 订阅(`shunt login xai`,无按 token 计费),而
**`xai`** 用一个 `XAI_API_KEY` 对接计量的开发者 API。订阅 bearer 和 API 密钥
**不可**互换 —— 各自只对其自己的提供方有效。

完整设置 —— 登录、两个提供方块、模型 slug、选择性启用的力度旋钮,以及
授权方面的坑 —— 见专门的 [xAI / Grok 指南](/zh-cn/guides/xai/)。

## 添加一个兼容 Anthropic 的后端

大多数第三方“在 Claude Code 中使用 X”网关都兼容 Anthropic-Messages:`kind = "anthropic"` 搭配 `auth = "api_key"`,仅在 `base_url` 和密钥环境变量上不同。开箱即用的 base:

| 提供方 | `base_url` | 示例模型 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`、`deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`、`glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | 见 [MiniMax 文档](https://platform.minimax.io/docs/token-plan/claude-code) |
| Mimo (Xiaomi) | `https://api.xiaomimimo.com/anthropic` | `mimo-v2.5-pro` — 见 [Mimo 文档](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8`(接受 `x_api_key`) |

例如,要通过 shunt 路由 Kimi 的模型:

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "KIMI_API_KEY"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

然后 `export KIMI_API_KEY=…`,[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/),并选择 `kimi-k2.7-code`(通过 `ANTHROPIC_CUSTOM_MODEL_OPTION` 或 `ANTHROPIC_MODEL`)。运行 `shunt check` 校验 —— 它会报告路由中的未知提供方、缺失的 `api_key_env` 或错误的 `base_url`。

每个提供方键(`kind`、`auth`、`api_key_header`、`count_tokens`……)都在 [配置参考](/zh-cn/reference/configuration/) 中有文档。

## 子 agent 插件

[`pleaseai/shunt` 市场](https://github.com/pleaseai/shunt/tree/main/plugins) 提供了固定到各提供方模型的现成 Claude Code 子 agent —— 每个模型一个 agent。安装一个插件,然后用 `@` 提及一个模型或设置 `CLAUDE_CODE_SUBAGENT_MODEL`。每个 agent 的 `model:` frontmatter 只让该子 agent 分流;主会话仍留在 Claude。

| 插件 | 模型(每个一个 agent) | 提供方 |
| :-- | :-- | :-- |
| `shunt-codex` | `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna` | `codex`(ChatGPT 订阅) |
| `shunt-xai` | `grok-build-0.1`、`grok-4.5`、`grok-4.3` | `xai`(API 密钥)或 `grok`(订阅) |
| `shunt-kimi` | `kimi-k2.7-code` | `kimi` |
| `shunt-deepseek` | `deepseek-v4-pro`、`deepseek-v4-flash` | `deepseek` |
| `shunt-zai` | `glm-5.2`、`glm-4.7` | `zai` |
| `shunt-minimax` | `MiniMax-M3[1m]` | `minimax` |
| `shunt-mimo` | `mimo-v2.5-pro` | `mimo` |

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-xai@shunt
```

每个插件仍需要在 `shunt.toml` 中路由其提供方(见上文各节)并导出匹配的凭据 —— 插件自己的 README 会列出确切的路由和环境变量。grok 模型可由任一 xAI 提供方提供:`xai`(API 密钥,按 token 计费)或 `grok`(通过 `shunt login xai` 使用 SuperGrok / X Premium+ 订阅;按等级设限 —— 遇到 403 时回退到 `xai`)。
