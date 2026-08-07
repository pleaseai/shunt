---
title: 入站 Codex 端点
description: 把 OpenAI Codex CLI 本身指向 shunt,并让它在 ChatGPT/Codex OAuth 账户池上做负载均衡。
---

本站其他所有指南都是把 **Claude Code** 路由到另一个后端。shunt 也可以反向运行:一个可选启用的原始 OpenAI Responses 透传,让 **Codex CLI** 把它自己的 `base_url` 指向 shunt,并在 ChatGPT/Codex OAuth 账户池上做负载均衡。它是可选启用的:`[server.codex_endpoint]` 不存在时,这些路由一个都不会注册,shunt 默认的 HTTP 暴露面保持不变。

它建立在与 [Codex 多账户](/zh-cn/guides/codex-multi-account/)相同的账户池之上 —— 选择、冷却和刷新都原封不动地共用。完整规范(包括确切的故障转移表和重载语义)见 [M11 行为规范](https://github.com/pleaseai/shunt/blob/main/docs/m11-inbound-codex-endpoint.md)。

端到端的设置演练 —— 启用端点、把 Codex CLI 指向 shunt、客户端认证、账户预配以及选择一个有权限的模型 —— 请参阅[连接 Codex CLI](/zh-cn/guides/connect-codex-cli/)。本页关注*这个端点做什么*;那篇指南则是*如何连接*的清单。

## 启用该端点

```toml
[server.codex_endpoint]   # all keys optional; default shown
provider = "codex"        # must be a chatgpt_oauth provider
```

```bash
shunt check
shunt run
```

启动校验会拒绝未知的 `provider`,或者不使用 `auth = "chatgpt_oauth"` 的提供方 —— 该端点注入的是运营者的 Codex bearer,因此只有 `chatgpt_oauth` 提供方符合条件。每个键与默认值见[配置参考](/zh-cn/reference/configuration/),已注册的路由见 [HTTP 端点](/zh-cn/reference/endpoints/)。

## 客户端分析数据接收端

Codex CLI 还会向 base URL 提交产品分析数据。shunt 接受该 CLI 可能产生的两条路径:

- `POST /backend-api/codex/analytics-events/events`
- `POST /codex/analytics-events/events`

这些路由使用与 Responses 路由相同的 `[server.auth]` 策略,但绝不会把遥测转发到上游,因为挑选某一个池化账户会把客户端事件错误地归属到该账户。认证之后它们总是返回 `200 {}`,包括请求体格式错误、不可读或过大的情况。

载荷与事件属性既不会被记录也不会被导出。shunt 只把经过净化的 `event_type` 记录为可选启用的 `shunt.codex_client_events` 计数器上的 `event` 属性:名称可以包含小写 ASCII 字母、数字、`.`、`_` 和 `-`,最长 64 字节;无效的名称变为 `other`,无法解析的批次变为 `unparsed`。在没有启用 Sentry 或 OpenTelemetry 指标时,这就是一个纯粹的丢弃接收端。

## 把 Codex CLI 指向 shunt

Codex CLI 总会把 `/responses` 追加到它使用的任何 base URL 之后,因此下面两种 `~/.codex/config.toml` 形态都可以:

**镜像 ChatGPT 后端的 base URL:**

```toml
chatgpt_base_url = "http://127.0.0.1:3001/backend-api/codex"
```

**或者用一个自定义模型提供方**(顶层的 `model_provider` 必须选中它,否则 CLI 会继续使用它内置的提供方):

```toml
model_provider = "shunt"

[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
```

使用自定义提供方时(加上 `requires_openai_auth = false`,这样 CLI 不需要本地登录),一旦指向 shunt,Codex CLI 自己的 `~/.codex/auth.json` 就变得无关紧要 —— 每个请求的账户都来自 shunt 的池。而 `chatgpt_base_url` 形态会让 CLI 停留在 ChatGPT 登录模式,因此它仍需本地登录,并且只能对**未设门控**的端点工作:它的 ChatGPT bearer 不是所配置的 shunt token,因此 `[server.auth]` 会拒绝它。

## 客户端认证

如果 shunt 配置了 [`[server.auth]`](/zh-cn/guides/shared-gateway/) —— 回环之外的场景都推荐配置 —— 请**要么**把客户端 token 作为 OpenAI 风格的 Bearer key 提供(`OPENAI_API_KEY` / 自定义提供方的 `env_key`,即 LiteLLM/llmgateway 的惯用法),**要么**作为 `x-shunt-token` 头部提供:

```toml
# A. Bearer — built-in openai provider. Set the base URL in ~/.codex/config.toml,
#    NOT via the OPENAI_BASE_URL env var: the env var leaves the CLI's Responses
#    WebSocket pointed at wss://api.openai.com, so it bypasses shunt. See
#    "Point the Codex CLI at shunt" in the connect guide.
openai_base_url = "http://127.0.0.1:3001/v1"
```

```bash
export OPENAI_API_KEY="<shunt-token>"      # sent as Authorization: Bearer
```

```toml
# B. Header — a custom provider carries it (use env_http_headers to keep it out of the file):
[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
http_headers = { "x-shunt-token" = "<token>" }
```

没有 `[server.auth]` 时,该端点对任何能触达它的人开放 —— 对回环或个人使用可以接受,对共享网关则不行。客户端提供的凭据**仅**用于向 shunt 认证:它(以及 CLI 碰巧发送的任何 `Authorization`)都会被剥除,绝不转发到上游。由于入站客户端是真正的 Codex CLI,该透传会逐字转发它的请求头部(`version`、`originator`、`OpenAI-Beta`、`x-codex-*` 等),并**只**换入所选池账户的 `Authorization` bearer 与 `chatgpt-account-id`。完整的认证演练见[连接 Codex CLI](/zh-cn/guides/connect-codex-cli/#3-提供-shunt-客户端-token当配置了-serverauth-时)。

## 账户预配

复用与 [Codex 多账户](/zh-cn/guides/codex-multi-account/#配置账户池)相同的账户池:

```bash
codex login
shunt login codex --name main
```

```toml
[[providers.codex.accounts]]
name = "main"
```

在**没有**配置 `[[providers.codex.accounts]]` **且 shunt 账户存储为空**时,该端点会回退到单个默认的 `~/.codex/auth.json` 凭据 —— 没有池化,也没有故障转移 —— 因此只要设置了 `[server.codex_endpoint]`,一个 Codex 登录就能工作。(处理器会先扫描账户存储,并把发现的任何账户组成池,因此导入到存储中的账户仍然会启用池化。)

## 与 `/v1/messages` 的差异

- **没有转换。**入站的 Responses 请求体会逐字节转发到上游,而上游的响应 —— 无论 SSE 还是 JSON、成功还是错误 —— 都逐字中继回来(状态码与 `content-type` 保留)。完全没有 Anthropic Messages ⇄ Responses 的转换步骤。
- **压缩的请求体直接透传。**当前的 Codex 版本在与 ChatGPT 后端通信时会用 zstd 压缩请求体,这也包括指向本端点的 `chatgpt_base_url` 形态。这些字节及其 `content-encoding: zstd` 头部会被原样转发;shunt 只是额外在内存中解码一份副本,用来读取请求的 `model` 以供指标、日志和 span 使用。shunt 无法解码的请求体照样能正常中继 —— 只有 `model` 标签会退化为 `unknown`,并附带一条说明原因的警告。
- **没有基于模型的路由。**每个请求都发往 `[server.codex_endpoint]` 中指定的那一个提供方;请求体的 `model` 字段原样转发,绝不参与选择提供方。
- **耗尽时逐字中继。**如果所有池化账户都已尝试过,并且至少收到过一个上游响应,shunt 会原样中继最后那个响应,而不是把它重新塑形成 Anthropic 风格的错误 —— 因为 Responses 客户端期待的是它从真实 ChatGPT 后端会得到的原始形态。
- **网关自身的错误使用 OpenAI 形态。**当失败源自 shunt 自己时 —— 客户端 token 错误或缺失(`401`)、账户池不可用且没有任何上游响应(`502`)、请求体过大,或端点未配置 —— shunt 会以 OpenAI Responses 的错误形态(`{"error":{"message":…,"type":…,"code":null}}`)返回,并保持相同的状态码,这样 Codex CLI 就能走它自己的错误解析路径,而不是 Anthropic 的 `{"type":"error",…}` 信封。被中继的*上游*错误(来自后端的 429/4xx/5xx)仍然逐字透传。
- **仅 HTTP/SSE。**即使目标提供方设置了 `websocket = true`,这个端点也始终使用 HTTP 传输。

## 安全

- 在回环之外的任何场景都请用 `[server.auth]` 为该端点设置门控 —— 提供方会在每个请求上注入一个真实的 Codex bearer。
- 客户端自己的凭据不会有任何部分到达 Codex 后端;该透传逐字转发 Codex CLI 自己的请求头部,并只换入所选池账户的 bearer 与 `chatgpt-account-id`(shunt 客户端 token 头部会被剥除,绝不转发)。
- 路由集合在启动时一次性确定。在运行时开启或关闭 `[server.codex_endpoint]` 会记录一条需要重启的警告;而 reload 仍可以改变它所指向的提供方。
