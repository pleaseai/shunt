---
title: Kimi (Moonshot)
description: 用 MOONSHOT_API_KEY 将映射的模型路由到 Moonshot 的兼容 Anthropic Kimi 端点,或通过 OAuth 复用 Kimi Code 订阅。
---

**Kimi** 是 Moonshot AI 的模型家族,通过一个**兼容 Anthropic** 的端点提供 ——
shunt 注入 Moonshot API 密钥并转发 Claude Code 的 Messages 请求。非 Anthropic 的
上游 id 会被剥离延迟工具字段(与 OpenRouter 的 stealth slug 同一条规则)。
`kimi` 预设是内置的,所以配置就是一条上游条目加上路由。

本页涵盖两个凭据各自独立的 Kimi 服务:计量的 Moonshot
API(`kimi` 预设,API 密钥,见下文)和 **Kimi Code** 订阅(`kimi-code` 预设、
OAuth 登录,见本页底部的 [Kimi Code(OAuth 订阅)](#kimi-codeoauth-订阅))。
它们是不同的端点,不可互换。

## 快速开始

让编码 agent 为你完成接入 —— `shunt add` 会打印一份内置的设置蓝图
(离线且只读;配置由 agent 编辑,该命令绝不会修改配置):

```bash
shunt add upstream kimi --print | claude
```

或者按照下面的手动步骤操作。

## 配置上游

`kimi` 预设提供了 `kind = "anthropic"`、`base_url = "https://api.moonshot.ai/anthropic"`,
以及来自 `MOONSHOT_API_KEY` 的 API 密钥认证:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

[[upstreams]]
name = "kimi"
provider = "kimi"

[[routes]]
model = "kimi-k3"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方,因此路由到 `kimi` 的配置
还必须声明它仍然指向的 `anthropic` 默认项(`server.default_provider` 默认为
`anthropic`);只有在你同时把 `default_provider` 设为某个已声明的上游时,才可以去掉
`anthropic` 条目。

旧的 `[providers.kimi]` 表形式仍然受支持(较早的示例用的是
`api_key_env = "KIMI_API_KEY"`,显式设置时它依然有效)—— 但不要在同一个文件中混用
`[[upstreams]]` 和 `[providers.*]`。

## 凭据

```bash
export MOONSHOT_API_KEY='...'
```

绝不要把密钥写进配置。`shunt check` 校验配置的结构,但不会
读取密钥的值 —— 如果 `MOONSHOT_API_KEY` 未设置,第一个被路由到 `kimi` 的请求会返回
一个认证错误。

## 模型

| 模型 id | 说明 |
| :-- | :-- |
| `kimi-k3` | 前沿等级;客户端可能会附加 Claude Code 的 `[1m]` 上下文标记(`kimi-k3[1m]`)—— shunt 会在匹配前把它剥离,所以请路由不带后缀的 id |
| `kimi-k2.7-code` | 面向编码的等级 |

在 Claude Code 中通过 `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子 agent 的
`model:` frontmatter 选择一个已路由的 id。若想改为在 `/model` 选择器中呈现一个条目,
请用 `[models.upstream_model]` 映射声明一个以 `claude` 为前缀的别名 —— 见
[模型发现](/zh-cn/guides/model-discovery/)。

## 校验

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"kimi-k2.7-code","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `kimi`,然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。

## 子 agent 插件

[`shunt-kimi` 插件](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-kimi) 为上面
每个模型各提供一个现成的 Claude Code 子 agent:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-kimi@shunt
```

## Kimi Code(OAuth 订阅)

**Kimi Code** 是与上文计量的 Moonshot API 不同的、按订阅计费的服务 ——
不同的主机(`api.kimi.com`,而非 `api.moonshot.ai`),不同的凭据(一个由 shunt 托管的 OAuth
token,而非 `MOONSHOT_API_KEY`)。它同样讲 Anthropic Messages 的 wire 形状,
所以用的是同一个适配器,只是换了预设:`kimi-code`。

### 快速开始

```bash
shunt add upstream kimi-code --print | claude
```

或者按照下面的手动步骤操作。

### 1. 登录

```bash
shunt login kimi --name <account-name>
```

`--name` 是必需的;`--mode`、`--long-lived` 和 `--manual` 对这个登录不被接受 ——
凭据始终是可刷新的,也没有手动粘贴的回退。shunt 运行一个
[RFC 8628](https://www.rfc-editor.org/rfc/rfc8628) 设备授权许可流程:它打印一个 URL
和一个短码,你在浏览器中(在本设备或另一台设备上)批准,shunt 则轮询
直到批准完成或该码过期。保存的账号落在
`~/.shunt/accounts/kimi/<account-name>.json`(0600,位于 0700 目录中),可用
`SHUNT_KIMI_ACCOUNTS_DIR` 覆盖。

Kimi 在每次刷新时轮换 refresh token,而其访问 token 只持续约
15 分钟,因此刷新很频繁。每个 Kimi 账号文件只运行一个 shunt 进程 —— 两个
共用一个文件的进程会在第一次刷新时互相作废。请改为为每个进程开通
一个单独的账号。

### 2. 配置上游

`kimi-code` 预设提供了 `kind = "anthropic"`、`base_url = "https://api.kimi.com/coding"`,
以及 `auth = "kimi_oauth"`:

```toml
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# 声明 [[upstreams]] 会替换内置的提供方集合,因此要在末尾保留一个
# anthropic 透传 —— 否则 `shunt check` 会拒绝默认的
# server.default_provider。这与 `shunt init` 追加的条目相同。
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

`kimi_oauth` 和 `claude_oauth`、`chatgpt_oauth` 一样支持账号池:用
`accounts = [...]` 代替 `account`,即可把多个具名账号池化到一个上游之下
(两者互斥),或者两个都省略以扫描整个由 shunt 托管的 Kimi 账号
存储。

### 模型

shunt 不会查询 Kimi Code 自己的模型列表端点 —— 它用 shunt 内置的目录来服务
`/v1/models`。请路由你的订阅实际被授权的模型 id:

```toml
[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

### 校验

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"<model-id-your-subscription-exposes>","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `kimi-code`。

一个带 `"We're unable to verify your membership benefits at this time"` 的
`402 Payment Required` 意味着登录成功了,但那个账号没有有效的 Kimi Code 会员。凭据
没问题;需要处理的是订阅。

### 池化账号与管理后台

一个 `kimi_oauth` 池参与与 Claude 和 Codex 池相同的负载均衡、故障转移和配额感知的
账号轮换,并且在启用了 `GET /admin/pool` 和经过脱敏的 `GET /usage` 聚合时,
其账号会出现在其中。它比其他池多一个轮换条件:上文那个
`402` 会员响应。由于失效的会员在每个请求上都返回 402,shunt 会把它当作
账号级别的失败 —— 它会给该账号降温并尝试下一个,而不是把这个 402 交给你的客户端、
让健康的账号闲着。如果池中*每一个*账号都失效了,你仍会拿回 Kimi 自己的 402 状态和消息,
所以原因依然可见。
[管理后台 web 界面](https://shunt.dev/guides/admin-remote-provisioning/) 中由浏览器驱动的
账号开通不支持 Kimi 账号 —— 该界面的池视图对 Kimi 是只读的;请在 CLI 上用
`shunt login kimi` 开通 Kimi 账号。
