---
title: 故障排查
description: 常见的 shunt 错误及其修复方法。
---

| 症状 | 原因 / 修复 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | shunt 无法读取 `~/.codex/auth.json`。运行 `codex login`。 |
| 映射模型上的 `authentication_error` | 提供方凭据过期/缺失 —— 重新运行 `codex login`,或 export `OPENAI_API_KEY`。shunt 会透出后端真实的 `detail` 消息。 |
| `400 … model is not supported when using Codex with a ChatGPT account` | 你用了一个 `-codex` slug(或一个你账户未被授权的 slug)。使用 [models.json](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) 中一个已授权的 slug(例如 `gpt-5.6-sol`、`gpt-5.5`),或设置 `upstream_model`。 |
| `/model` 没有列出你的模型 | 对于 `gpt-*` id 使用 `ANTHROPIC_CUSTOM_MODEL_OPTION`;[发现](/zh-cn/guides/model-discovery/) 只暴露 `claude`/`anthropic` 前缀的 id。 |
| 发现从不触发 | 它被门控在一个网关凭据(`ANTHROPIC_AUTH_TOKEN`、API 密钥或 `apiKeyHelper`)加上 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` 上。用 `claude --debug` → `[gatewayDiscovery]` 行调试。 |
| `config check failed` | 运行 `shunt check` 查看确切原因(bind 地址、路由中的未知提供方、错误的适配器/认证)。 |
| Claude Code 要求你登录 | 设置一个 shunt 能为未映射模型转发的 Anthropic 凭据(`ANTHROPIC_AUTH_TOKEN` / 登录)。仅有一个 base URL 不是凭据。 |
| 映射模型上力度卡在 `medium` | 设置 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` —— 见 [力度与上下文](/zh-cn/guides/effort-and-context/#reasoning-effort)。 |
| 映射模型上下文长度错误后会话卡住 | shunt 会把上游溢出错误重写为 `prompt is too long …`,使 Claude Code 自动压缩并重试 —— 见 [上下文溢出恢复](/zh-cn/guides/effort-and-context/#context-overflow-recovery)。如果每隔几轮就复现,把 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 降到模型的真实窗口。 |
| Cloudflare 后流断掉(524) | 把 [`sse_keepalive_seconds`](/zh-cn/guides/shared-gateway/#sse-keepalive-pings) 保持在默认值(30)而非 `0`。 |
| 共享网关上映射模型返回 401 | 客户端 token 缺失/无效 —— 设置 `ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <token>"`;见 [共享网关](/zh-cn/guides/shared-gateway/)。 |

完整的网关故障排查表见 [将 Claude Code 连接到 LLM 网关](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors)。
