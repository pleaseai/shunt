---
title: 连接 Claude Code
description: 将 Claude Code 指向 shunt、选择正确的 Anthropic 凭据,并选择映射的模型。
---

基于官方的 [将 Claude Code 连接到 LLM 网关](https://code.claude.com/docs/en/llm-gateway-connect) 指南 —— shunt *就是*你要连接的那个网关。

## 1. 将 Claude Code 指向 shunt

将 base URL 设置为你正在运行的网关(默认绑定 `127.0.0.1:3001`),在你的 shell 中,或持久化在 [设置文件](https://code.claude.com/docs/en/settings) 的 `env` 块中:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
```

```json
// ~/.claude/settings.json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3001"
  }
}
```

保留你现有的 Anthropic 凭据 —— 对于每个你未映射的模型,shunt 都会将其**原样转发**到 `api.anthropic.com`,因此未映射的模型会完全照旧工作。映射模型的提供方凭据由 shunt 自己注入;Claude Code 从不发送它们。

## 2. 选择 Anthropic 凭据

Claude Code 发送给 shunt 的凭据扮演两个角色:它认证 **Claude 透传模型**,并**门控 [模型发现](/zh-cn/guides/model-discovery/)** —— 只有当设置了 `ANTHROPIC_AUTH_TOKEN`、一个 API 密钥或一个 `apiKeyHelper` 时,Claude Code 才会发起 `GET /v1/models` 请求。无论哪种方式,映射的模型(`gpt-*` 等)都不受影响。

| 凭据 | Token 刷新 | 发现 | Claude 透传 | 计费 |
| :-- | :-- | :-- | :-- | :-- |
| 仅 claude.ai OAuth **登录** | 自动 | ❌ 从不触发 | ✅ | 订阅 |
| 来自 `claude setup-token` 的 `ANTHROPIC_AUTH_TOKEN` —— **推荐** | 无需(一年期 token) | ✅ | ✅ | 订阅 |
| `apiKeyHelper` = `shunt token` | 由 helper 刷新 | ✅ | ✅ | 订阅 |
| `ANTHROPIC_AUTH_TOKEN=<真实 API 密钥>` | 无需 | ✅ | ✅ | **API(非订阅)** |

像 `sk-dummy` 这样的占位值能满足发现门控,但会破坏透传 —— 它被转发给 Anthropic 并返回 401。

**优先用 `claude setup-token`。** 它铸造一个**一年期**的 OAuth token([认证文档](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)),因此无需刷新任何东西,而且一个值同时覆盖两个角色:

```bash
claude setup-token                        # 浏览器登录 → 打印 sk-ant-oat…
export ANTHROPIC_AUTH_TOKEN=sk-ant-oat…   # 或将其持久化在设置的 `env` 块中
```

:::caution[刷新陷阱]
一旦网关凭据激活,Claude Code 会**停止刷新它自己的登录**,因此 `~/.claude/.credentials.json` 内那个短寿命的访问 token 会在几小时内过期,而一个只*读取*该文件的 helper 就会失效。也不要手动刷新它 —— `platform.claude.com/v1/oauth/token` 有严格的速率限制。要复用实时的订阅登录,请使用内置的 [`shunt token`](/zh-cn/reference/cli/#shunt-token) helper,它会安全地刷新它。
:::

### `shunt token` 凭据 helper

`shunt token` 会把一个 Claude 订阅 OAuth token 打印到 stdout,因此它可以直接接入 Claude Code 的 `apiKeyHelper`:

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

- **静态模式** —— 如果设置了 `SHUNT_GATEWAY_TOKEN` 或 `CLAUDE_CODE_OAUTH_TOKEN`,它会原样回显该值。把它指向一个 `claude setup-token` 值,则从不刷新任何东西。
- **自动刷新模式** —— 否则它读取 `~/.claude/.credentials.json`(用 `CLAUDE_CREDENTIALS` 覆盖),返回访问 token,并仅在过期前 5 分钟内刷新它,以 `0600` 原子写回。

静态 + `setup-token` 这条路径仍是最简单、最安全的默认选择。

:::note[为什么这能认证 Claude 透传]
Claude Code 会在 **`x-api-key` 和 `Authorization: Bearer` 两者中**都发送一个 `apiKeyHelper` 值。一个订阅 OAuth token(`sk-ant-oat…`)只作为 bearer 才有效,因此 `x-api-key` 中的那份副本会让 `api.anthropic.com` 拒绝请求。在透传路径上,当 bearer 是 OAuth token 时,shunt 会剥除那个重复的 `x-api-key`,让它独立存在。没有这一点,`apiKeyHelper` + 一个 OAuth token 将只覆盖发现和映射的模型 —— 透传会 401。
:::

## 3. 提供映射提供方的凭据

这些进入 **shunt 的环境**,而非 Claude Code 的:

```bash
export OPENAI_API_KEY=sk-...   # openai 提供方
codex login                    # codex/ChatGPT 提供方(此后自动刷新)
```

## 4. 选择一个映射的模型

Claude Code 的模型发现只认以 `claude`/`anthropic` 开头的 id,因此对于 OpenAI/Codex id(`gpt-*`),请使用 `ANTHROPIC_CUSTOM_MODEL_OPTION` —— 它添加一个选择器条目,其 id 跳过校验:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

然后在 Claude Code 中从 `/model` 选择它。该 id 正是 shunt 路由所依据的,因此它必须匹配你配置中的一条 `[[routes]]`/`[[route_prefixes]]` 规则。

两种选择器暴露方法以 `claude-`/`anthropic-` 前缀清晰分界 —— 它们互不重叠。发现*只*认 `claude-`/`anthropic-` id;`ANTHROPIC_CUSTOM_MODEL_OPTION` 和 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 窗口覆盖*只*适用于**不**以该前缀开头的 id:

| 项目 | `claude-`/`anthropic-` id(发现别名) | 非 `claude-` id(如 `gpt-5.6-sol`) |
| :-- | :-- | :-- |
| [`/v1/models` 发现](/zh-cn/guides/model-discovery/) → `/model` 选择器 | ✅ 自动列出(“From gateway”),多个模型 | ❌ 被 Claude Code 丢弃 |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ 不生效 | ✅ 加入选择器(**仅一个 id**) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 窗口 | ❌ 忽略 → 200k 默认 | ✅ 生效 → 设置真实窗口 |

因此一个 `claude-…-via-codex` 发现别名很方便(自动列出、一键选择),但其上下文窗口**卡在 200k 默认值**上 —— 该覆盖无法触达一个 `claude-` 前缀的 id([力度与上下文](/zh-cn/guides/effort-and-context/))。若想在多个模型间获得选择器的便利,请选**发现别名**(接受 200k 分母);若需要准确的窗口,则每次针对一个模型选**通过 `ANTHROPIC_CUSTOM_MODEL_OPTION` 的非 `claude-` id**。

:::tip[或重映射分层别名]
第三个选项是把 Claude Code 的内置 `haiku`/`sonnet`/`opus` 别名重新指向 Codex slug(例如 `haiku → gpt-5.6-luna`,`sonnet → gpt-5.6-sol`),使整个会话的分层系统在不用 `ANTHROPIC_CUSTOM_MODEL_OPTION` 的情况下解析到你的 ChatGPT 订阅。见 [ChatGPT / Codex → 将分层别名重映射到 Codex](/zh-cn/guides/codex/#remap-the-tier-aliases-to-codex)。
:::

### 按 agent 分流

按上下文选择通过 Claude Code 自己的旋钮实现 —— 把一个 agent 分流到映射的模型,同时主会话留在 Claude 上:

```yaml
# .claude/agents/researcher.md
---
name: researcher
model: gpt-5.6-sol   # 这个 agent 的推理被分流;主会话留在 Claude 上
---
```

一个命名子 agent 的 `model:` frontmatter 是把子 agent 放到 `gpt-*` id 上的**唯一**方式:该字段接受任意字符串,而 Agent/Task 工具的 `model` 参数被限制为内置别名(`opus`/`sonnet`/`haiku`/`fable`),无法接受网关 id。按类型生成该 agent 时**不带** `model` 覆盖 —— 工具参数优先于 frontmatter(`CLAUDE_CODE_SUBAGENT_MODEL` > 工具 `model` > frontmatter > `inherit`),因此传入一个会遮蔽映射的模型。`CLAUDE_CODE_SUBAGENT_MODEL` 强制每个子 agent 都用同一个模型。窗口会自动跟随模型 id,因此一个全局的 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 就能为映射的子 agent 标定大小,而 Claude 主会话保留它自己的。

## 5. 验证

```bash
# 未映射的模型 -> 转发给 Anthropic(使用你的 Anthropic 凭据)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-6","max_tokens":1,"messages":[{"role":"user","content":"."}]}'

# 映射的模型 -> 分流到提供方(使用 shunt 的提供方凭据)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

然后启动 `claude`,运行 `/status`,并检查 **Anthropic base URL** 那一行显示的是你的网关。关于推理力度和上下文窗口调优,另见 [力度与上下文](/zh-cn/guides/effort-and-context/)。
