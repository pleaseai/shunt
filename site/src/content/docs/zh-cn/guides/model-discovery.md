---
title: 模型发现
description: 用 Claude 命名的别名自动填充 Claude Code 的 /model 选择器。
---

发现(`GET /v1/models`)可以自动填充 Claude Code 的 `/model` 选择器 —— **但 Claude Code 会忽略任何不以 `claude`/`anthropic` 开头的 id**([协议参考](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery))。因此一个 `gpt-*` id 无论如何都会在客户端被丢弃;只有当你暴露一个由 `[[routes]]` 条目重写为真实上游 slug 的 **Claude 命名别名**时,发现才有用:

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"     # 必须以 claude/anthropic 开头
display_name = "GPT-5.6-Sol (via Codex)"

[[routes]]
model = "claude-gpt-5.6-sol-via-codex"  # Claude Code 发送的别名
provider = "codex"
upstream_model = "gpt-5.6-sol"          # 转发给 ChatGPT 后端的真实 slug
```

然后启用发现(Claude Code v2.1.129+)并重启 shunt + Claude Code:

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

该别名会出现在 `/model` 中,标记为 *From gateway*;选择它会发送 `claude-gpt-5.6-sol-via-codex`,shunt 将其路由到 `codex` 并重写为 `gpt-5.6-sol`。

对于没有别名的 `gpt-*` id,请改用 `ANTHROPIC_CUSTOM_MODEL_OPTION` —— 见 [连接 Claude Code](/zh-cn/guides/connect-claude-code/#4-select-a-mapped-model)。

## 发现需要一个网关凭据

仅有 claude.ai OAuth *登录* 不会触发发现。只有当设置了 `ANTHROPIC_AUTH_TOKEN`、一个 API 密钥或一个 `apiKeyHelper` 时,Claude Code 才会发起 `/v1/models` 请求;在纯 Max/Pro 订阅登录下它什么都不发送 —— 没有请求抵达 shunt,也没有缓存被写入 —— 即使开启了标志也是如此。见 [选择凭据](/zh-cn/guides/connect-claude-code/#2-choose-the-anthropic-credential);`claude setup-token` 是推荐路径。

## 调试

发现会**静默**失败(3 秒超时,任何重定向都算作失败)并回退到缓存的/内置的列表。运行 `claude --debug` 并查找 `[gatewayDiscovery]` 行以确认它是否运行过。
