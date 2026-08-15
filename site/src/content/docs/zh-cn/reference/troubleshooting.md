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
| `opus` 选择 Opus 4.7 / `sonnet` 选择 Sonnet 4.6 | Claude Code 的内置别名表会为网关会话钉住这些层级。使用 `ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5` 在客户端钉住层级,或在 shunt 中重映射 id —— 见[模型别名](/zh-cn/guides/model-aliases/#别名解析)。 |
| Opus/Fable 的上下文窗口显示 200K | 只有 base URL 为 `api.anthropic.com` 时,Claude Code 才会信任模型的原生 1M 窗口。选择 `opus[1m]` / `fable[1m]` —— 见[模型别名](/zh-cn/guides/model-aliases/#1m-上下文不会自动应用)。 |
| `/model` 中缺少 Fable,或 `claude-fable-5` 回退到 Opus | 使用普通 `ANTHROPIC_BASE_URL` 时,Fable 会被过滤掉。设置 `ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5`,或使用[网关登录](/zh-cn/guides/gateway-login/) —— 见[模型别名](/zh-cn/guides/model-aliases/#fable-从选择器中消失)。 |
| 发现从不触发 | 它被门控在一个网关凭据(`ANTHROPIC_AUTH_TOKEN`、API 密钥或 `apiKeyHelper`)加上 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` 上。用 `claude --debug` → `[gatewayDiscovery]` 行调试。 |
| `config check failed` | 运行 `shunt check` 查看确切原因(bind 地址、路由中的未知提供方、错误的适配器/认证)。 |
| Claude Code 要求你登录 | 设置一个 shunt 能为未映射模型转发的 Anthropic 凭据(`ANTHROPIC_AUTH_TOKEN` / 登录)。仅有一个 base URL 不是凭据。 |
| 映射模型上力度卡在 `medium` | 设置 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` —— 见 [力度与上下文](/zh-cn/guides/effort-and-context/#推理力度)。 |
| 映射模型上工具搜索未生效(每轮都发送全部工具 schema) | 设置 `ENABLE_TOOL_SEARCH=true`。Claude Code 在非第一方 base URL 背后会自动禁用乐观式工具搜索;shunt 会转发 `tool_reference` 块并按需揭示延迟的 schema —— 见 [ChatGPT / Codex → 工具搜索](/zh-cn/guides/codex/#工具搜索)。 |
| 工具搜索能工作,但不收回上下文(仍在为文本 shim 的缓存失效代价买单) | 原生 `tool_search` 在默认("auto")模式下,只对已确认支持该协议的宿主生效 —— ChatGPT/Codex 后端和 `api.openai.com` —— 并且仍需通过风格/模型门控(标准 OpenAI/ChatGPT-Codex 风格、gpt-5.4 及以上模型)。自定义的 OpenAI 兼容端点(LiteLLM、vLLM、OpenRouter、自托管)**不会**自动启用;请在确认它实现了 `tool_search` 条目后设置 `tool_search = true`。如果你期望在已知宿主上使用原生协议但仍在走 shim,请确认你没有设置 `tool_search = false` —— 见 [ChatGPT / Codex → 工具搜索 → 原生协议](/zh-cn/guides/codex/#原生协议)。 |
| 映射模型上下文长度错误后会话卡住 | shunt 会把上游溢出错误重写为 `prompt is too long …`,使 Claude Code 自动压缩并重试 —— 见 [上下文溢出恢复](/zh-cn/guides/effort-and-context/#上下文溢出恢复)。如果每隔几轮就复现,把 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 降到模型的真实窗口。 |
| Cloudflare 后流断掉(524) | 把 [`sse_keepalive_seconds`](/zh-cn/guides/shared-gateway/#sse-keepalive-ping) 保持在默认值(30)而非 `0`。 |
| 共享网关上映射模型返回 401 | 客户端 token 缺失/无效 —— 设置 `ANTHROPIC_AUTH_TOKEN=<token>`(以 `Authorization: Bearer` 被接受,仅池化网关)或 `ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <token>"`(混有透传模型时推荐,让每个调用方真实的凭据保有自己的槽位;确实出现在 `Authorization` / `x-api-key` 中的客户端 token 会从该槽位清除而不是被转发);见 [共享网关](/zh-cn/guides/shared-gateway/#入站客户端-token)。 |
| Anthropic 适配器模型返回 429 | 检查网关日志中的 `rate_limit_kind`。`quota`(带有 `retry-after` / `anthropic-ratelimit-*` 头部)是真实的速率限制 —— 请退避或减少并行负载。`client-shape-rejection`(OAuth 请求、两种头部都没有、body 只有 `"Error"`)表示 api.anthropic.com 拒绝了一个不像 Claude Code 的订阅 OAuth 请求 —— 非 Claude Code 客户端必须使用 API 密钥而不是 OAuth token。Claude Code 的 auto 模式权限分类器是唯一省略上游所检查的 identity 块的 first-party 请求;shunt 仅对该请求恢复此块,因此 auto 模式不会再以“model temporarily unavailable”fail-closed。其余请求均保持 byte-for-byte 透传。`no-ratelimit-headers`(非 OAuth 凭据)是缺少速率限制元数据的提供方 429 —— 按 `quota` 处理。 |
| 共享网关上返回 `503 overloaded_error` | 网关已达入站并发上限,直接拒绝了该请求而不是排队(body 消息为 `too many requests are already in flight`,并带 `Retry-After: 1`)。这是 shunt 自己的准入控制,不是上游 503 —— 上游 503 会带着提供方自己的消息原样中继。请在指定延迟后重试、减少并行负载,或调高 [`max_concurrent_requests`](/zh-cn/guides/shared-gateway/#入站并发上限) 并重启(该上限在启动时固定)。 |

完整的网关故障排查表见 [将 Claude Code 连接到 LLM 网关](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors)。
