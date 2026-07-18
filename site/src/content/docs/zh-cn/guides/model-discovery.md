---
title: 模型发现
description: 用 Claude 命名的别名自动填充 Claude Code 的 /model 选择器。
---

发现(`GET /v1/models`)可以自动填充 Claude Code 的 `/model` 选择器。默认情况下,shunt 会先返回管理员维护的 `[[models]]` 条目,再追加与参考 Claude apps gateway 保持一致的内置 Claude 模型目录。对于 id 完全相同的条目,会保留管理员维护的条目并去重。若只想公开维护的列表,请在顶层设置 `auto_include_builtin_models = false`。内置模型不需要专门的 `[[routes]]` 条目;它们按常规路由规则解析,当 `[[routes]]` 与 `[[route_prefixes]]` 均未匹配时回退到 `server.default_provider`。

Claude Code 会忽略任何不以 `claude`/`anthropic` 开头的发现 id([协议参考](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery))。因此,在维护 `gpt-*` 等非 Claude 模型时,请创建一个 **Claude 命名别名**,并通过 `[[routes]]` 条目将其重写为真实的上游 slug:

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

## Claude Desktop 只识别 tier 命名的 id

Claude Code 接受任何以 `claude`/`anthropic` 开头的发现 id,但 **Claude Desktop 更严格**:它只显示 tier 命名的 id —— `claude-sonnet-*`、`claude-opus-*`、`claude-haiku-*`、`claude-fable-*`。因此上面的 `claude-<slug>-via-<provider>` 别名会出现在 Claude Code 中,但由于 `gpt` 不是 tier 名称,它会**在 Claude Desktop 中被静默丢弃**。

内置目录全部是 tier 命名的,因此在 Desktop 中仍然可见;丢失的只有你维护的 `claude-<slug>-via-<provider>` 别名。要向 Claude Desktop 公开非 Anthropic 后端,请复用一个 tier 命名的 id,并通过 `[[routes]]` 的 `upstream_model` 进行映射:

```toml
[[routes]]
model = "claude-sonnet-5"        # Claude Desktop 识别的 tier 命名 id
provider = "codex"
upstream_model = "gpt-5.6-sol"   # 真实后端 slug
```

在 Desktop 中选择它会解析到预期的上游。该 route 会覆盖内置目录中该 id 的默认路由,因此请选择一个后端映射对用户仍然有意义的 tier 名称。

## 发现需要一个网关凭据

仅有 claude.ai OAuth *登录* 不会触发发现。只有当设置了 `ANTHROPIC_AUTH_TOKEN`、一个 API 密钥或一个 `apiKeyHelper` 时,Claude Code 才会发起 `/v1/models` 请求;在纯 Max/Pro 订阅登录下它什么都不发送 —— 没有请求抵达 shunt,也没有缓存被写入 —— 即使开启了标志也是如此。见 [选择凭据](/zh-cn/guides/connect-claude-code/#2-choose-the-anthropic-credential);`claude setup-token` 是推荐路径。

## 调试

发现会**静默**失败(3 秒超时,任何重定向都算作失败)并回退到缓存的/内置的列表。运行 `claude --debug` 并查找 `[gatewayDiscovery]` 行以确认它是否运行过。
