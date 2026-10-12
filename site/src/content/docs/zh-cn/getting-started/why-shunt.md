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

也可以让某一个 model id 自己做决定:[阶段路由器](/zh-cn/guides/stage-router/)指定一个强力档位和一个高效档位,并根据对话最近的 tool-result 元数据(`tool_use.name` 与 `tool_result.is_error`,绝不是提示词文本)逐轮在两者之间选择。任何条目还可以带一张 [`[models.subagents]`](/zh-cn/reference/configuration/#modelssubagents可选) 覆盖层,把被委派的工作送到另一个目标,而父会话仍去自己的目的地。两者都不配置则行为不变。

## 基于 NVIDIA Switchyard 的路由

阶段路由器和基于评判模型的路由器并不是 shunt 自己发明的启发式规则。它们以 NVIDIA [Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) 项目的路由库 [`switchyard-libsy`](https://github.com/NVIDIA-NeMo/Switchyard/tree/3ddea9d30174ad835cf505a93617ba251c8eb8dd/crates/libsy) 为基础。Switchyard 的目标是把“每一次 LLM 调用路由到仍能完成任务的最便宜模型”。这带来以下好处:

- **有公开结果的评分器。** shunt 原样使用 Switchyard 的阶段评分器。在上游的 Terminal-Bench 2.1 运行中,阶段路由保留了 Opus 4.8 基线准确率的 95.7%,成本降低 30.5%。参与对比的四个固定单模型,得分全部低于 56%。这些是上游测出的数字,不是 shunt 的:shunt 没有跑过这个基准,信号也是用自己的方式提取的。请把它们当作这种思路确实划算的证据,而不是对你配置的模型组合的承诺。
- **多种算法。** 除阶段评分外,shunt 还提供 Switchyard 的其他算法,每一项都按模型条目单独启用。每个条目选择一种路由器类型,例如 `stage_router`(可附加交接笔记)、`llm_classifier`(其 `escalation` 模式即升级路由)、`composite`、`advisor` 等。子 agent 路由则另外通过 `[models.subagents]` 表配置。
- **为 Claude Code 会话所做的调整。** Switchyard 与提供方无关。shunt 补上 Claude Code 网关所需的部分。工具名直接按 Claude Code 实际使用的名字匹配。会话的档位用非对称迟滞固定,避免长会话逐轮切换档位而丢掉已预热的提示缓存。被委派子 agent 的路由与父会话分开处理。
- **仍是普通 model id 的目标。** 路由后的目标会重新进入 shunt 的常规路由,因此沿用各自的故障转移链、账号池和适配器。客户端看到的 model id 也与请求时一致。
- **轻量的依赖。** 这个依赖让发布二进制增大约 0.3%。它固定在一个经过审阅的上游修订上,因此每次升级也都是一份经过审阅的 diff。既没有 `[models.router]` 表也没有 `[models.subagents]` 表的条目,路由方式与之前完全相同。
- **无需另起服务器的路由。** Switchyard 的 [Server Path](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/getting_started.md#server-path) 会把 `switchyard-server` 作为独立代理单独运行。对于像 Claude Code 这样用 Anthropic Messages 发请求的客户端,同样的路由类型(`auto`、`stage_router`、`llm_classifier`)可以直接写进 shunt 的 `[models.router]`,不必再多部署一个代理。不过 shunt 只对 Anthropic Messages 请求(`/v1/messages` 及其 `/v1/messages/count_tokens` 探测)应用路由器,OpenAI Chat Completions 或 Responses 客户端的路由不在 shunt 的覆盖范围内。

shunt 从 Switchyard 引入了什么、舍弃了什么,基准的注意事项,以及算法的逐步说明,见 [Switchyard 集成](/zh-cn/guides/switchyard/)。

## shunt 实现了什么

- **`POST /v1/messages`** —— 推理,按请求的 `model` id 路由。未映射的模型使用调用方自己的凭据逐字节转发给 Anthropic;但 shunt 为其他提供方生成的 [`thinking` signature](/zh-cn/providers/anthropic/) 例外——该值会被 Anthropic 拒绝,因此会被移除。
- **Anthropic Messages ⇄ OpenAI Responses 转换** —— 面向映射的 OpenAI 系列模型,含流式传输。
- **ChatGPT 订阅复用** —— `codex` 提供方复用(并自动刷新)Codex CLI 的 `~/.codex/auth.json` 登录。
- **`GET /v1/models`** —— 面向 Claude 命名别名的 [模型发现](/zh-cn/guides/model-discovery/)。
- **Token 计数** —— 转换类提供方用本地 tiktoken 计数,透传时用上游的精确计数。
- **流式韧性** —— [SSE keepalive ping](/zh-cn/guides/shared-gateway/#sse-keepalive-ping),使 Cloudflare 之类的代理不会中断长时间的推理过程。
- **可选的入站认证** —— 面向共享部署的 [按客户端 token](/zh-cn/guides/shared-gateway/)。

准备好试试了?前往 [安装](/zh-cn/getting-started/installation/)。
