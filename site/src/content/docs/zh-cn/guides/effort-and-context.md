---
title: 力度与上下文
description: 推理力度、token 计数以及上下文指示器对映射模型的行为方式。
---

## 推理力度

Claude Code 的力度级别(`/effort`、`/model` 滑块、`--effort` 或 `CLAUDE_CODE_EFFORT_LEVEL`)以 `output_config.effort` 请求字段发送,shunt 将其为映射的模型映射到 Responses 的 `reasoning.effort`:

| Claude Code 力度 | → `reasoning.effort` |
| :-- | :-- |
| `low` / `medium` / `high` / `xhigh` | 透传 |
| `max` | 在接受它的模型上透传(**gpt-5.6** 系列),否则折叠为 `xhigh` |

一个 Codex slug 接受哪些推理级别,按模型列在 openai/codex 的 [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) 中(`supported_reasoning_levels`)。

:::note[自定义模型 id 需要一个标志]
对于像 `gpt-5.6-sol` 这样的自定义网关 id,你必须设置 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` —— 否则 Claude Code 会为它不识别为具备力度能力的模型 id 省略 `output_config.effort`,而 shunt 回退到 `medium`。

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```
:::

shunt 中的优先级:配置中的 `route.effort` / `[providers.*].effort` 覆盖最先胜出;否则遵从请求的 `output_config.effort`;否则 `thinking.enabled → high`,然后是模型名后缀(`-xhigh`/`-high`/`-medium`/`-low`,其中 `-spark` 被当作 `-low`),否则 `medium`。

## Token 计数(`count_tokens`)

对于一个 **Anthropic 路由**的模型,shunt 将 `POST /v1/messages/count_tokens` 透传给上游(精确计数)。对于一个 **`responses` 路由**的模型,没有等价的上游端点,因此由提供方的 `count_tokens` 设置决定:

- **`count_tokens = "tiktoken"`(默认)** —— shunt 用 tiktoken 的 `o200k_base` 编码器在本地计算计数,并返回 `{"input_tokens": N}`。对 GPT 系列模型上的文本近乎精确,并在进程内应答(约毫秒级) —— 这一点很重要,因为 Claude Code 的 `/context` 会为每个显示项发起一次 `count_tokens` 调用(每次调用 30–50 次)。
- **`count_tokens = "estimate"`(主动启用)** —— shunt 返回 **404**,这是网关协议明确允许的。主循环上下文栏随后在本地估算,但 `/context` 会通过网络对 Haiku 重新运行每个类别的计数 —— 慢,并且在没有 Anthropic 凭据可用时静默报告为 0 tokens。

无论哪种方式,请求都不会抵达 responses 适配器,因此一个计数请求绝不会被转成(并按)一次完整的推理调用(计费)。

## 映射模型的上下文 / 用量显示

Claude Code 从助手消息的 token `usage` 除以模型的上下文窗口大小,在本地计算上下文指示器。对于路由到 `responses` 提供方的模型:

- **Token 计数(分子)是准确的。** shunt 从 Responses 的 `usage` 中读取 `input_tokens`(以及缓存 token)并在 Anthropic 的 `message_delta` 中转发它们,将缓存部分剥离到 `cache_read_input_tokens`。
- **窗口(分母)对未识别的 id 默认为固定的 200k。** 一个真实窗口更大的模型(例如 372k 的 `gpt-5.6-sol`)会显示一个保守的、偏高的百分比 —— 这只会让自动压缩稍微提前触发。

200k 默认值可以在客户端用 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 覆盖(Claude Code 2.1.205+);它适用于任何**不**以 `claude-` 开头的模型 id:

```bash
# 例如 gpt-5.6-sol 的真实窗口
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

由于该覆盖**只**适用于不以 `claude-` 开头的 id,一个 [发现别名](/zh-cn/guides/model-discovery/)(它*必须*以 `claude-` 开头)无法接收它 —— 其窗口保持钉在 200k 默认值上。在选择器中很方便,但当你需要准确窗口时,请用一个非 `claude-` id(通过 `ANTHROPIC_CUSTOM_MODEL_OPTION`,或通过 [重映射分层别名](/zh-cn/guides/codex/#remap-the-tier-aliases-to-codex))。当两个映射的层共享一个窗口时 —— `gpt-5.6-sol` 和 `gpt-5.6-luna` 都是 372k —— 一个全局值即可覆盖两者。

:::caution
该值是**全局**的 —— 会话中每个非 `claude-` 模型都用同一个值 —— 而将它设置得大于真实上游窗口会把自动压缩推迟到请求溢出真实上限之时。shunt 会 [重写该溢出错误](#context-overflow-recovery),使 Claude Code 自动压缩并重试,但每次溢出往返都是浪费的延迟 —— 请把该值匹配到你映射模型中最小的真实窗口。

`gpt-5.6-sol`(真实窗口 372k)的实测边界:365k 输入 token 正常应答;在 372k+ 时流式请求返回一个 `prompt is too long` 错误,触发自动压缩(`gpt-5.5` 是 272000)。而一个*非*流式请求则退化为一个带 `input_tokens: 0` 的空 `200`,但 Claude Code 的主循环始终使用流式。
:::

另一个客户端杠杆是 `[1m]` 模型 id 后缀,它强制一个 1M 窗口 —— 只有当上游确实拥有该窗口时才使用它。(shunt 在路由匹配和转发之前会剥除尾部的 `[1m]`,因此该提示纯粹留在客户端,提供方永远看不到它。)

| 字段 | 映射(`responses`)模型 | Claude 透传 |
| :-- | :-- | :-- |
| 已用上下文 token | ✅ 准确(由 shunt 转发) | ✅ 准确 |
| 上下文窗口(分母) | ⚠️ 200k 默认;设置 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | ✅ 精确 |
| `count_tokens`(预检) | ⚠️ 本地 tiktoken 计数(默认) | ✅ 精确(上游) |
| `rate_limits`(5 小时 / 每周) | ❌ 需要 Anthropic 头部 | ✅ 显示 |

## 上下文溢出恢复

当一段对话超出上游模型的真实窗口时,提供方会用它自己的措辞拒绝请求 —— OpenAI 的 `context_length_exceeded`、`"This model's maximum context length is N tokens…"`,或某个代理的 `"prompt token count of N exceeds the limit of M"`。Claude Code 的自动压缩并重试只在 Anthropic 的措辞上触发,因此若不重写,这些错误会让会话搁浅,直到手动 `/compact`([有记载的网关陷阱](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors))。

shunt 会检测 `responses` 路由模型上的上下文溢出错误,并将它们重写成 Claude Code 能匹配的 Anthropic 形状:

```json
{"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long: 372982 tokens > 272000 maximum"}}
```

当上游消息同时携带两个 token 计数时,shunt 会保留它们(无论上游以何种顺序陈述) —— Claude Code 解析 `N tokens > M maximum` 的差距,并在一次重试中把整个超出量都压缩掉。当上游没有给出计数时(例如 Responses API 平实的 *"Your input exceeds the context window of this model"*),shunt 单独发出 `prompt is too long`,它仍会触发压缩。非溢出错误则带着原始消息透传。

## 归属块

Claude Code 会在系统提示前添加一行归属信息。Anthropic 在处理前会剥除它,但 shunt 原样转发,因此一个映射的提供方会把它作为 `instructions` 的第一行接收。对于非 Anthropic 模型,它是无害但无意义的噪声。要丢弃它:

```bash
export CLAUDE_CODE_ATTRIBUTION_HEADER=0
```

这是全局的,因此它也会从 Anthropic 透传流量(用于成本追踪)中移除归属 —— 当你在路由到另一个提供方时,这没问题。
