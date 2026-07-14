---
title: Anthropic 多账户
description: 将多个 Claude 订阅 OAuth 账户组成池,以会话粘性、感知模型的主动轮换和被动故障转移运行。
---

shunt 可以在内置的 `anthropic` 提供方背后把多个 Claude 订阅 OAuth 凭据组成池。当 Claude Code 提供 `x-claude-code-session-id` 时请求具有会话粘性;不带该头部的请求使用按提供方的轮询。shunt 跟踪每个账户的上游配额头部,当粘性账户接近与模型相关的配额时主动轮换;而配额拒绝、认证失败和上游故障仍由被动故障转移兜底。

:::caution[订阅条款]
仅在你的账户条款允许的范围内使用订阅凭据。shunt 是非官方客户端,不会改变 Anthropic 的账户或订阅政策。
:::

## 配置账户池

设置 `auth = "claude_oauth"` 并添加显式的账户条目:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

# 现有的 Claude Code 凭据文件。shunt 会刷新并写回。
[[providers.anthropic.accounts]]
name = "primary"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000" # 可选

# 长期有效的 `claude setup-token` 值。按原样使用;不刷新。
[[providers.anthropic.accounts]]
name = "backup"
token_env = "CLAUDE_BACKUP_OAUTH_TOKEN"
uuid = "11111111-1111-1111-1111-111111111111" # 可选
```

```bash
export CLAUDE_BACKUP_OAUTH_TOKEN='<value from claude setup-token>'
shunt check
shunt run
```

可用三种 Claude 登录模式中的任意一种存储账户:

```bash
# 创建一个新的可刷新登录(默认使用自动 localhost callback)。
shunt login claude --name primary --mode oauth

# 导入当前可刷新的 Claude Code 登录。
shunt login claude --name imported --mode import

# 生成并存储一个一年期、仅推理的 setup token。
shunt login claude --name backup --mode setup-token
```

在 TTY 中省略 `--mode` 时,会打开默认选中 OAuth 的三选一提示。非交互输入继续沿用原有的 `import` 默认值。`--long-lived` 是 `--mode setup-token` 的 deprecated alias。Full OAuth 通常通过一次性的 `127.0.0.1` callback 完成。要粘贴 `<code>#<state>`,请使用 `--manual`。如果浏览器启动、callback bind 或 5 分钟等待失败,shunt 也会回退到手动粘贴。

然后使用只带名字的条目:

```toml
[[providers.anthropic.accounts]]
name = "primary"

[[providers.anthropic.accounts]]
name = "backup"
```

存储文件位于 `~/.shunt/accounts/claude/<name>.json`;设置 `SHUNT_CLAUDE_ACCOUNTS_DIR` 可覆盖该目录。如果配置的 `accounts` 列表为空,shunt 会扫描存储目录,按文件名顺序使用所有有效的 JSON 账户文件。存储文件是私有的(Unix 上为 `0600`,目录为 `0700`)。

远程运营者可以通过可选启用的[管理 Web 界面](/zh-cn/guides/admin-remote-provisioning/),在浏览器中预配可刷新的 Full OAuth 账户或一年期 setup token 账户,并查看池的当前健康状况。导入已有 credential 文件仍然仅限 CLI。

Full OAuth 创建一个新的可刷新 credential;import 把当前的 `~/.claude/.credentials.json` credential 复制进 shunt 的存储。两者都保留刷新能力,import 还会记录当前账户 UUID。setup-token 模式运行与 `claude setup-token` 相同的一年期、仅推理 PKCE 流程。批准后,shunt 交换显示的授权码,把 token 与签发账户 UUID 一起存储,并且不打印 token。这使得当池选中另一个账户时,`metadata.user_id.account_uuid` 保持一致。复用同一个名称会替换该账户的存储文件。已有的外部 setup token 仍需要 `token_env` 加显式 `uuid`。

:::caution[Refresh token 轮换]
成功刷新可能返回替换用的 refresh token,并使旧值失效。每个可刷新存储文件只能有一个正在运行的 shunt owner。不要让多个进程指向同一个文件,也不要在另一台主机上独立运行其副本。请为每个进程分别预配;如果有意共享不可刷新的 credential,请使用静态 setup token。
:::

## 账户字段

| 字段 | 必填 | 含义 |
| :-- | :-- | :-- |
| `name` | 是 | 只含小写字母、数字和连字符的唯一标签。若没有其他来源字段,则解析同名的 shunt 存储文件。 |
| `credentials` | 可用来源之一 | Claude Code `.credentials.json` 形态的文件。`~/` 会被展开。shunt 在临近过期时刷新,并将刷新后的 token 原子性地写回。 |
| `token_env` | 可用来源之一 | 包含 setup token 的环境变量。其值按原样使用,401 之后无法刷新。 |
| `uuid` | 否 | 所选账户的 Anthropic UUID,用于改写已存在的 `metadata.user_id.account_uuid`。 |
| `threshold` | 否 | `[0.0, 1.0]` 范围内的按账户软配额阈值,适用于所有没有按窗口取值的窗口。较低的取值把该账户标记为提前轮换出去的后备账户。 |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | 否 | 按窗口的软阈值;各自在其窗口上优先于 `threshold`。 |
| `priority` | 否 | 粘性账户不健康时的选择优先级;数值越低越优先,默认 `100`。 |
| `disabled` | 否 | `true` 把该账户完全移出选择,同时保留在配置和管理仪表板上。 |

不要在同一个账户上同时设置 `credentials` 和 `token_env`。

## 选择与主动轮换

- 带 `x-claude-code-session-id` 时:一个稳定的哈希选出粘性账户。如果该账户可用且低于切换阈值,shunt 会让它保持在首位。
- 不带该头部时:每个提供方有自己的轮询计数器。
- 在 `claude_oauth` 账户池处理的每个上游响应上,shunt 会记录以下头部(如存在):
  - `anthropic-ratelimit-unified-5h-utilization`、`anthropic-ratelimit-unified-7d-utilization` 和 `anthropic-ratelimit-unified-7d_oi-utilization`;
  - `anthropic-ratelimit-unified-5h-reset`、`anthropic-ratelimit-unified-7d-reset` 和 `anthropic-ratelimit-unified-7d_oi-reset`(Unix 秒);以及
  - `anthropic-ratelimit-unified-status`。
- 默认切换阈值是 `0.98`。当 unified status 为 `rejected`、共享 5 小时使用率达到其阈值,或起决定作用的周使用率达到其阈值时,该账户即接近配额。阈值可以按账户(上文的 `threshold*` 字段)或池级(见[调优选择](#调优选择serverpool))调低。
- 5 小时桶适用于所有模型。Fable 模型 id 在 `7d_oi` 周桶使用率存在时使用它,否则回退到共享 `7d`。其他所有模型家族使用共享 `7d`;由于目前没有 Sonnet 专属头部,Sonnet 也使用 `7d`。
- 接近配额、处于冷却中或被禁用的粘性账户会被主动轮换掉。shunt 优先选择低于阈值的可用账户,先按 `priority`(数值低者优先)排序,再按起决定作用的周桶最早重置的顺序排列,先花掉"不用即失"的配额。周重置未知的账户排在最前。随后是可用但接近配额的账户,再后是按最快恢复排序的冷却中账户。配置了 `[server.pool]` 时,燃烧率余量会取代周重置这一次级排序依据(见下文)。
- shunt 从不因本地配额状态而安全失败(fail closed):即使所有账户都接近配额或在冷却中,每个非 `disabled` 账户仍留在尝试顺序里。
- 配额桶在其重置时间戳过后自动清除。成功的响应会清除所选账户的冷却。

池的选择、冷却和配额状态在进程存活期间跨配置热重载保留。如果主动轮换无法避开上游限制,被动故障转移仍然生效。

## 调优选择(`[server.pool]`)

可选的 `[server.pool]` 表(issue #135)在上述行为之上增加按窗口的软阈值与感知燃烧率(burn-rate)的排序。没有该表时,选择逻辑使用单一的内置 `0.98` 阈值,与之前完全一致。

```toml
[server.pool]
# hard_threshold = 0.98      # (默认)兜底;达到或超过时始终排在最后
default_threshold = 0.9      # 所有窗口的软默认值
default_threshold_5h = 0.95  # 按窗口覆盖
default_threshold_fable = 0.85
burn_rate_avoidance = true   # 避开按预测会在重置前触及阈值的账户

[[providers.anthropic.accounts]]
name = "primary"
priority = 1                 # 粘性账户不健康时优先选择

[[providers.anthropic.accounts]]
name = "backup"
threshold = 0.5              # 后备:配额用掉一半即轮换出去

[[providers.anthropic.accounts]]
name = "spare"
disabled = true              # 保留配置,但永不被选中
```

- **阈值解析。**对每个窗口 `X`(`5h`、`7d`、`fable`),生效的软阈值为:账户 `threshold_X` → 账户 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`,并以 `hard_threshold` 为上限。所有取值都是 `[0.0, 1.0]` 范围内的使用率分数;超出范围会使 `shunt check` 失败。
- **燃烧率余量。**根据每个窗口的使用率与重置时刻(窗口长度固定为 5 小时和 7 天),shunt 按观测到的平均速度预测触及软阈值所需的时间,再减去窗口重置所需的时间。余量为正意味着按当前速度该账户能撑到重置。`priority` 相同的可用账户按余量最大者优先排序;未观测到的窗口按无限余量计。
- **预测性规避。**设置 `burn_rate_avoidance = true` 时,预测余量为负的账户被视为接近配额,在真正触及阈值*之前*就被轮换掉。默认关闭 —— 而按余量排序始终生效。
- **全员接近配额的兜底。**当每个账户都超过了软阈值(或被预测将耗尽)时,池不会变空:接近配额的账户按最佳余量的顺序继续服务,而达到或超过 `hard_threshold` 的账户仍排在最后,其后才是冷却中的账户。
- **适用范围。**这些配额旋钮只作用于 Claude(Anthropic)池 —— Codex 后端不发送配额头部,因此对 [Codex 池](/zh-cn/guides/codex-multi-account/) 它们不起作用,而 `priority` 和 `disabled` 仍然适用。
- 管理池端点(`GET /admin/pool`)会报告每个账户的 `priority`、`disabled` 标志,以及在配置了 `[server.pool]` 时该账户当前以秒为单位的余量预测;仪表板的状态列会标记被禁用的账户。

## 故障转移规则

| 响应 | 行为 |
| :-- | :-- |
| 2xx | 中继并标记为健康。 |
| 429 且 `anthropic-ratelimit-unified-5h-status`、`-7d-status` 或 `-7d_oi-status` 中出现 `rejected` | 配额耗尽:按数值 `retry-after` 冷却(默认 60 秒,钳制到 1–3600 秒),然后轮换。 |
| 普通 429 | 瞬时限流:按数值 `retry-after` 等待(默认 1 秒,上限 300 秒),对**同一**账户重试一次,然后中继该重试的响应。 |
| 使用 `credentials` 时的 401 | 强制刷新,对同一账户重试一次;若仍是 401,冷却 5 分钟并轮换。 |
| 使用 `token_env` 或存储管理的 setup token 时的 401 | 无法刷新:冷却 5 分钟并轮换。 |
| 5xx 或传输失败 | 冷却 30 秒并轮换。 |
| 其他状态 | 直接中继,不做故障转移。 |

分类发生在响应体开始流式传输之前,因此流中途的失败绝不会被重放。如果池在收到响应后耗尽了尝试,客户端会得到最后一个真实的上游状态和响应体。如果所有账户在收到任何上游响应之前都失败了,shunt 返回一个网关自身的错误。

路由到 Anthropic 的 `POST /v1/messages/count_tokens` 请求使用同一个池。

## 请求与响应的改动

对所选账户,shunt 将客户端认证替换为:

```http
Authorization: Bearer <selected OAuth token>
anthropic-beta: ...,oauth-2025-04-20
```

它会移除传入的 `authorization` 和 `x-api-key`,仅在缺失时追加 `oauth-2025-04-20`,并保留其他端到端头部。

经过池的响应会标识账户:

```http
x-shunt-account: backup
```

在共享网关上请使用中性的账户名。该头部会把配置的标签暴露给收到响应的每个已授权客户端。池耗尽后对最后一个上游响应的中继会省略 `x-shunt-account`。

### `account_uuid`

Claude Code 可能把账户元数据以 JSON 形式编码在字符串值的 `metadata.user_id` 里。如果所选账户有 `uuid`,shunt 会用该值替换**已存在的**内部 `account_uuid`。若元数据缺失、格式错误、缺少 `account_uuid`,或所选账户没有 UUID,则请求体保持原样。它不会注入缺失的元数据。

## 安全约束

`claude_oauth` 仅在以下条件下被接受:

- 提供方的 `kind = "anthropic"`;
- `base_url` 使用 HTTPS;且
- 其主机是 `anthropic.com` 或诸如 `api.anthropic.com` 的子域。

这些启动检查防止 OAuth bearer 被发送到源之外或以明文传输。HTTPS 与主机检查在**回环主机上放宽**(`localhost`、`127.0.0.1`、`[::1]` 等):回环的 `base_url` 可以使用纯 HTTP 和任意主机,这样本地调试代理或 mock 能接收流量 —— bearer 无法离开运营者的机器。非回环主机始终要求 HTTPS + `anthropic.com`。在共享部署上,还应配置 [`[server.auth]`](/zh-cn/guides/shared-gateway/#入站客户端-token),因为 `claude_oauth` 花费的是网关自有的凭据。客户端随后可以用它已经在发送的 `ANTHROPIC_AUTH_TOKEN` 完成认证(客户端 token 除 `x-shunt-token`、`x-api-key` 外,也接受 `Authorization: Bearer`)—— 在仅池化的网关上不需要 `ANTHROPIC_CUSTOM_HEADERS` 行。

## 遗留的后续事项

- **风暴控制(storm-control):** 对刚切换过来的账户逐步提升并发仍是后续工作,尚未实现。

实现行为参考了 [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) 与随产品发布的 Claude Code 二进制。shunt 对 teamclaude 没有运行时依赖。
