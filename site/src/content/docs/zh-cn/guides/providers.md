---
title: 提供方
description: 内置的提供方,以及如何用一个 TOML 表添加任意兼容 Anthropic 的后端。
---

提供方是一个**名称 → 配置的映射**:一个新的上游只不过是又一个 `[providers.<name>]` 表 —— 无需改动代码。两种适配器类型即可覆盖一切:

- **`kind = "anthropic"`** —— 上游讲 Anthropic Messages API。shunt 将请求透传,可选择注入一个不同的 API 密钥。
- **`kind = "responses"`** —— 上游讲 OpenAI Responses API。shunt 在 Anthropic Messages ⇄ Responses 之间转换,含流式传输。

## 内置提供方

| 名称 | 类型 | 认证 | 后端 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` —— 转发调用方自己的凭据 |
| `openai` | `responses` | `api_key` (`OPENAI_API_KEY`) | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` —— 复用 `~/.codex/auth.json` |

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

## 添加一个兼容 Anthropic 的后端

大多数第三方“在 Claude Code 中使用 X”网关都兼容 Anthropic-Messages:`kind = "anthropic"` 搭配 `auth = "api_key"`,仅在 `base_url` 和密钥环境变量上不同。开箱即用的 base:

| 提供方 | `base_url` | 示例模型 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`、`deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`、`glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | 见 [MiniMax 文档](https://platform.minimax.io/docs/token-plan/claude-code) |
| Mimo (Xiaomi) | `https://api-mimo.mi.com/anthropic` | 见 [Mimo 文档](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) |
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
