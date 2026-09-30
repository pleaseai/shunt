---
title: Claude Code 插件
description: 安装 shunt 的 Claude Code 插件 —— 显示池余量的 /shunt:usage mod,以及为 shunt 分流的模型准备的子 agent 套件。
---

shunt 自带一个 Claude Code 插件市场。只需添加一次:

```
/plugin marketplace add pleaseai/shunt
```

那里有两类插件。一类是 **`shunt` mod** —— 它添加一条命令,报告网关自身的池使用量。另一类是 **提供方套件** —— 它们添加运行在 shunt 分流到其他提供方的模型上的子 agent。

## `shunt` mod: `/shunt:usage`

```
/plugin install shunt@shunt
```

`/shunt:usage` 打印共享账户池的剩余余量,数据读取自网关的 [`GET /usage`](/zh-cn/reference/endpoints/) 端点:

```
shunt: pool — degraded   http://127.0.0.1:3001

  5h    ▓▓▓▓▓▓░░░░  62% left   resets 04:11
  7d    ▓▓▓▓▓▓▓▓░░  81% left   resets Sun 01:11
  fable ▓▓░░░░░░░░  19% left   resets Sun 01:11

  claude  ok         5h  71%  7d  84%  fable  19%
  codex   exhausted  5h   0%  7d  40%  fable    —

  headroom left, averaged over the pool's accounts; a shared figure, not a promise about your next request
```

该 mod 自己回答这条命令,因此不会向模型发送任何内容,答案也不消耗 token。

### 读懂这些数字

`remaining` 是池的总容量中仍**未使用**的比例。`62%` 表示还剩 62% 的余量,而不是已经用掉 62%。它是报告该窗口的、未被禁用的账户上的 `mean(1 - utilization)`,因此九个已耗尽的账户加一个全新账户读作 `10%`,而不是 `100%`。

它是池级汇总,**不是预测**。路由还会权衡可用性、模型、会话亲和性和优先级,因此一个健康的数值并不保证你的下一个请求会被接纳。

| 窗口  | 覆盖范围 |
| ------- | -------------- |
| `5h`    | 滚动的 5 小时会话窗口 |
| `7d`    | 共享的每周窗口 |
| `fable` | Fable 范围的每周窗口(`7d_oi`) |

当没有任何未被禁用的账户报告某个窗口时,该窗口读作 `—`。ChatGPT/Codex 账户从 `x-codex-*` 响应头填充 `5h` 和 `7d`,并且没有自己的 Fable 范围信号。

第一块是跨所有已池化提供方的汇总;其下的各行是按每个已池化提供方给出的同一汇总,因此路由到某一个提供方的会话可以读取该提供方的余量,而不是混合后的数值。该端点从不携带账户名称、数量、优先级或按账户的数值 —— 这些细节留在仅限管理员的 `GET /admin/api/pool` 之后。

### 前置条件

1. 像 [连接 Claude Code](/zh-cn/guides/connect-claude-code/) 中那样,把 Claude Code 指向你的网关:

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
   export ANTHROPIC_AUTH_TOKEN=<your client token>
   ```

2. 启用该端点。`GET /usage` 是选择性开启的,并且需要 [`[server.auth]`](/zh-cn/guides/shared-gateway/),因此你的 [配置](/zh-cn/reference/configuration/) 中两个表都必须存在:

   ```toml
   [server.auth]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

   `[server.auth]` 从环境变量而不是 TOML 中读取令牌,默认是 `SHUNT_CLIENT_TOKENS`,格式为 `name:token` 键值对;若该变量未设置,网关将启动失败。请在运行网关的一侧导出它,使用与上面 `ANTHROPIC_AUTH_TOKEN` 相同的令牌:

   ```bash
   export SHUNT_CLIENT_TOKENS="claude-code:<your client token>"
   ```

3. 在启用 function hooks 的情况下运行 Claude Code —— 该功能处于早期访问阶段:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

没有第 3 步时命令依然存在,但会回退为请模型用一次工具调用去读该端点,而不是直接作答。

### 它读取什么

该 mod 读取五个环境变量,且不写入任何变量。默认情况下,它把会话自己的凭据发送到会话已经在向其发送每条消息的那个网关,因此不会触达任何会话原本没有在使用的主机。`SHUNT_BASE_URL` 是唯一有意为之的例外,它会改为指向你指定的网关。在完全未设置 base URL 时,它会如实说明,而不是去调用 Anthropic 自己的 API。

| 变量 | 用途 |
| -------- | ------- |
| `SHUNT_BASE_URL` | 网关 base URL;覆盖 `ANTHROPIC_BASE_URL` |
| `ANTHROPIC_BASE_URL` | 本会话已经在经由的网关 |
| `SHUNT_TOKEN` | 客户端 token;覆盖下面两者,以 `Authorization: Bearer` 发送 |
| `ANTHROPIC_AUTH_TOKEN` | 按 Claude Code 发送它的方式,以 `Authorization: Bearer` 发送 |
| `ANTHROPIC_API_KEY` | 按 Claude Code 发送它的方式,以 `x-api-key` 发送 |

正是 `SHUNT_BASE_URL` 让你可以在把流量路由经由另一个网关的同时,读取某一个网关的池。

### 为什么是 `/shunt:usage` 而不是 `/usage`

`/usage` 是 Claude Code 自己的内置命令,引擎不允许插件占用内置命令的名字。插件的 markdown 命令改为由插件加上命名空间,因此这条命令以 `shunt:usage` 的形式发布,不与任何东西冲突。

## 提供方子 agent 插件

它们添加固定到某个模型 id 的子 agent,而 shunt 会把该 id 路由到另一个提供方。会话仍在 Claude Code 的运行框架内继续运行 —— 同样的工具、同样的技能 —— 只有 token 生成被分流。

| 插件 | 模型 | 设置 |
| ------ | ------ | ----- |
| `shunt-codex` | GPT-6 Sol · Luna、GPT-5.6 Sol · Terra · Luna | [ChatGPT / Codex](/zh-cn/guides/codex/) |
| `shunt-xai` | Grok 4.6 · 4.5 · Build | [xAI / Grok](/zh-cn/guides/xai/) |
| `shunt-kimi` | Kimi K2.7 Code · K3 | [Kimi](/zh-cn/providers/kimi/) |
| `shunt-deepseek` | DeepSeek V4 Pro · Flash | [DeepSeek](/zh-cn/providers/deepseek/) |
| `shunt-zai` | GLM 5.2 · 4.7 | [Z.ai](/zh-cn/providers/zai/) |
| `shunt-minimax` | MiniMax-M3 | [MiniMax](/zh-cn/providers/minimax/) |
| `shunt-mimo` | MiMo V2.5 Pro | [MiMo](/zh-cn/providers/mimo/) |

安装方式相同:

```
/plugin install shunt-codex@shunt
```

每个插件都需要在你的网关配置中把对应的模型 id 路由到那个提供方;否则 Claude Code 会把模型 id 直接发给 Anthropic,请求就会失败。

## 注意事项

function hooks 处于早期访问阶段。hooks 模块只在启用它们的地方加载,而 `shunt` mod 所针对的 API 可能在 Claude Code 的各个发布之间无预告地变化。提供方套件不使用 function hooks,因此不受影响。
