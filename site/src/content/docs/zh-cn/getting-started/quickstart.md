---
title: 快速开始
description: 在五分钟内配置 shunt、运行网关,并将 Claude Code 指向它。
---

本演练带你从一个已安装的 `shunt` 二进制文件,走到一个 `gpt-*` 模型运行在 Claude Code 自身框架内的 Claude Code 会话。请先安装 shunt —— 见 [安装](/zh-cn/getting-started/installation/)。

## 1. 配置

shunt 出厂即预配置好所有提供方,因此一份最小配置只需声明路由。创建 `shunt.toml`(在工作目录中,或 `~/.config/shunt/shunt.toml`):

```toml
# 精确模型 id -> 提供方
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"     # 通过 `codex login` 复用你的 ChatGPT 登录

# 或者把每个 gpt-* id 都发送到 OpenAI API
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"    # 使用 OPENAI_API_KEY
```

校验它:

```bash
shunt check
# -> config ok
```

## 2. 提供提供方凭据

选择你路由到的那个提供方:

```bash
codex login                     # codex 提供方:ChatGPT 订阅登录
# 或
export OPENAI_API_KEY=sk-...    # openai 提供方:API 密钥
```

## 3. 运行网关

```bash
shunt run
# -> shunt listening on 127.0.0.1:3001
```

## 4. 将 Claude Code 指向它

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1   # 使 /effort 映射到 reasoning.effort
claude
```

在 Claude Code 中运行 `/model` 并选择 `gpt-5.6-sol`。未映射的模型(你所有的 `claude-*` id)会完全照旧工作 —— shunt 使用你自己的凭据将它们转发给 Anthropic。

## 5. 验证

在打开 Claude Code 之前(或不打开它)直接测试网关:

```bash
# 映射的模型 -> 分流到提供方(使用 shunt 的提供方凭据)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

一个以 `{"id":"msg_` 开头的 JSON 响应意味着它成功了。在 Claude Code 中,`/status` 应把 **Anthropic base URL** 显示为 `http://127.0.0.1:3001`。

## 接下来去哪

- [配置](/zh-cn/guides/configuration/) —— 配置文件、环境变量覆盖、路由优先级。
- [提供方](/zh-cn/guides/providers/) —— 添加 Kimi、DeepSeek、GLM、OpenRouter 及其他后端。
- [连接 Claude Code](/zh-cn/guides/connect-claude-code/) —— 深入讲解凭据、按 agent 路由。
- [故障排查](/zh-cn/reference/troubleshooting/) —— 常见错误及修复。
