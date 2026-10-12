---
title: OpenCode Zen
description: 用 OPENCODE_API_KEY 将映射的模型路由到 OpenCode Zen 的兼容 Anthropic 端点。
---

**OpenCode Zen** 是 OpenCode 团队策划的模型目录 —— 来自 Anthropic、OpenAI、Google、xAI、Z.ai 等方的精选模型,在一个端点后面提供,按 Zen API 密钥即用即付计费。Zen 原生使用 **Anthropic Messages** 线缆格式(含 `stream: true` SSE),所以 shunt 原样转发 Claude Code 的 Messages 请求并注入 Zen 密钥。`opencode` 预设是内置的,所以配置就是一条上游条目加上路由。

## 快速开始

让一个编码代理替你接线 —— `shunt add` 打印内嵌的安装蓝图(离线且只读;代理编辑配置,命令本身从不改):

```bash
shunt add upstream opencode --print | claude
```

或按以下手动步骤操作。

## 配置上游

`opencode` 预设提供 `kind = "anthropic"`、`base_url = "https://opencode.ai/zen"`、来自 `OPENCODE_API_KEY` 的 API 密钥认证,以及 zen 端点读取的 `x-api-key` 头:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 为未路由的模型(如 claude-*)保留 Anthropic 默认

[[upstreams]]
name = "opencode"
provider = "opencode"

[[routes]]
model = "claude-fable-5-1-via-zen"
provider = "opencode"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方,所以路由到 `opencode` 的配置也必须声明它仍指向的 `anthropic` 默认(`server.default_provider` 默认为 `anthropic`);只有当你同时把 `default_provider` 设为某个已声明的上游时,才去掉 `anthropic` 条目。

旧式 `[providers.opencode]` 表仍受支持,但预设不会填充它 —— 旧式表必须自己写明 `kind`、`base_url`、`auth = "api_key"`、`api_key_env` 和 `api_key_header = "x_api_key"`。不要在同一个文件里混用 `[[upstreams]]` 和 `[providers.*]`。

## 凭据

在 [opencode 控制台](https://opencode.ai/console) 创建 API 密钥(或复用 `opencode` CLI 登录时存储在 `~/.local/share/opencode/auth.json` 的密钥),然后在启动 shunt 的环境中导出:

```bash
export OPENCODE_API_KEY='...'
```

不要把密钥写进配置。`shunt check` 只验证配置结构,不读取密钥值 —— 如果 `OPENCODE_API_KEY` 未设置,第一个路由到 `opencode` 的请求会返回认证错误。

Zen 只从 `x-api-key` 读取凭据 —— `Authorization: Bearer` 头会被当作缺失密钥拒绝。预设已经发送正确的头,无需配置;显式的 `auth = { mode = "api_key", header = "bearer" }` 映射是覆盖它的方式(不是 zen 想要的)。

## 模型

Zen 提供跨厂商目录 —— Claude、GPT、Gemini、Grok、GLM 等。实时列表公开在 `https://opencode.ai/zen/v1/models`;一些示例 id:

| 模型 id | 说明 |
| :-- | :-- |
| `claude-fable-5-1` | zen 上的 Anthropic 前沿档 |
| `claude-opus-5` | zen 上的 Anthropic 旗舰档 |
| `gpt-6-astra` | zen 上的 OpenAI 档 |

模型可用性随目录轮换而变化 —— 路由新 id 前先查实时列表,并在宣传前对照你的计划验证模型 id。

在 Claude Code 中通过 `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子代理的 `model:` frontmatter 选择路由 id。要在 `/model` 选择器中显示条目,请用 `[models.upstream_model]` 映射宣传一个 `claude` 前缀别名 —— 见[模型发现](/zh-cn/guides/model-discovery/)。

## 验证

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-fable-5-1-via-zen","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头是 `opencode`,然后[把 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。

## 注意

- Zen 的 `/v1/messages/count_tokens` 不存在:调用它的客户端会收到 zen 的 HTML 404 页而不是 Anthropic 错误形状。这是 zen 侧的缺口;shunt 的 `count_tokens` 直通会原样到达。Claude Code 的 `/context` 在计数端点失败时退回自己的计数,所以实际影响是 `/context` 变慢,而不是坏掉。
- 402 `Payment Required` 或类似计费错误是 zen 自己的响应 —— 凭据已生效,需要关注的是账户余额或计划。
