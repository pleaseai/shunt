---
title: 为什么选 shunt
description: shunt 是什么、它与其他 Claude Code 代理有何不同,以及何时使用它。
---

`shunt` 是一个符合规范的 [Claude Code LLM 网关](https://code.claude.com/docs/en/llm-gateway-protocol):一个透明代理,针对**你映射的模型**,在**推理层**将推理分流到另一个 LLM 提供方。它按请求的 `model` id 进行路由 —— 默认情况下,其余一切均原样透传给 Anthropic(即“分流”;回退目标可通过 `server.default_provider` 配置)。

名字即机制:电气/铁路中的 *shunt(分流)* 将流量中被选中的部分导向一条并行路径。在这里,被映射模型的推理被分流到另一个提供方,而 Claude Code 的工具和技能保持完好。

## 工作原理

Claude Code 会把每一轮都发送到 Anthropic API。`shunt` 位于前面(通过 `ANTHROPIC_BASE_URL`),针对你映射的模型,将它们的推理分流到另一个提供方(OpenAI、Codex/ChatGPT……)。由于路由发生在 HTTP/推理层 —— 而不是把任务移交给另一个 CLI —— 会话仍在 Claude Code 的框架内运行:相同的工具循环、相同的预加载技能、相同的捆绑脚本路径解析。只有 token 生成被外包出去。

将其与把子 agent 移交给另一个运行时(如 Codex CLI)相比,后者在技术栈中切得更高,会丢失人设和预加载技能。

## 按模型,而非按 agent —— 也不是全局替换

大多数 Claude Code 代理把**所有**流量路由到一个替代提供方(全局模型替换)。`shunt` 的重点是由请求的 `model` id 驱动的**选择性、按模型**分流:让主会话留在 Claude 上,只把你指名的模型分流到其他提供方。

选择性是在 Claude Code 自身中决定的,它本来就允许你按上下文选择模型:

- 主会话的 `/model` 选择器,
- 子 agent 定义的 `model:` frontmatter,
- 面向所有子 agent 的 `CLAUDE_CODE_SUBAGENT_MODEL`,
- 用 `ANTHROPIC_CUSTOM_MODEL_OPTION` 向选择器添加一个自定义条目。

shunt 只是遵从它收到的 model id —— 没有脆弱的按 agent 系统提示指纹识别。同样的选择性无需 shunt 检查调用方身份即可下探到单个 agent。

## shunt 实现了什么

- **`POST /v1/messages`** —— 推理,按请求的 `model` id 路由。未映射的模型使用调用方自己的凭据逐字节转发给 Anthropic。
- **Anthropic Messages ⇄ OpenAI Responses 转换** —— 面向映射的 OpenAI 系列模型,含流式传输。
- **ChatGPT 订阅复用** —— `codex` 提供方复用(并自动刷新)Codex CLI 的 `~/.codex/auth.json` 登录。
- **`GET /v1/models`** —— 面向 Claude 命名别名的 [模型发现](/zh-cn/guides/model-discovery/)。
- **Token 计数** —— 转换类提供方用本地 tiktoken 计数,透传时用上游的精确计数。
- **流式韧性** —— [SSE keepalive ping](/zh-cn/guides/shared-gateway/#sse-keepalive-pings),使 Cloudflare 之类的代理不会中断长时间的推理过程。
- **可选的入站认证** —— 面向共享部署的 [按客户端 token](/zh-cn/guides/shared-gateway/)。

准备好试试了?前往 [安装](/zh-cn/getting-started/installation/)。
