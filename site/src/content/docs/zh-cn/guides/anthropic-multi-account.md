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
| `uuid` | 否 | 所选账户的 Anthropic UUID,用于改写已存在的 `metadata.user_id.account_uuid`,同时也是池中用于合并别名的稳定身份。仅有名称的条目(通过存储扫描解析)会在选择发生前自动从存储的 `shuntAccountUuid` 填充。通过 `credentials` 或 `token_env` 配置的条目,其身份在设置了 `uuid` 时为该值,否则为 `name`;只要该身份与另一别名显式的 `uuid` 或名称回退身份相等,就会与之合并——为清晰、有意的合并,请在两个条目上设置匹配且非空的 `uuid`(当某个显式 `uuid` 意外匹配另一账户的名称回退身份时,shunt 也会发出警告)。 |
| `threshold` | 否 | `[0.0, 1.0]` 范围内的按账户软配额阈值,适用于所有没有按窗口取值的窗口。较低的取值把该账户标记为提前轮换出去的后备账户。 |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | 否 | 按窗口的软阈值;各自在其窗口上优先于 `threshold`。 |
| `priority` | 否 | 粘性账户不健康时的选择优先级;数值越低越优先,默认 `100`。 |
| `disabled` | 否 | `true` 把该账户完全移出选择,同时保留在配置和管理仪表板上。 |

不要在同一个账户上同时设置 `credentials` 和 `token_env`。

:::note[Duplicate names for one real account]
`uuid` 也是池的稳定上游身份。如果两个名称携带相同的 UUID,shunt 会将它们视为**同一个账户**:它们共享配额、冷却、使用量、健康状态和刷新锁,故障转移会跳过重复的别名。粘性哈希和轮询基于不同的身份运作,因此添加一个别名不会移动会话。代表账户是 `priority` 最低的已启用别名,其次是第一个条目;只有该代表的 token 会被尝试。shunt 会记录一条重复身份警告(配置文件中 `[[providers.anthropic.accounts]]` 之间的重复,每次成功加载配置时记录一次,包括重新加载;存储扫描发现的重复,每当重复集合发生变化时记录一次——两者都不是每次请求都记录)。因此,即使该代表 token 失效而另一个别名的 token 有效,shunt 仍不会尝试该别名。

通过 admin web 界面删除存储管理的账户时,只有在确认没有其他存储别名仍解析为同一身份后,才会清除该身份共享的进程内健康状态;扫描失败时会保留健康状态。这是 admin 存储删除的语义——从 TOML 配置中移除别名,或直接删除其 credential 文件,都不会经过此清理流程。
:::

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
usage_refresh_seconds = 300  # 为可刷新账户校正带外用量
state_path = "shunt-state.json"  # 跨重启保留配额(热启动)
ramp_initial_concurrency = 2 # 风暴控制:对刚切换过来的账户逐步放行(slow-start)

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
- **适用范围。**这些配额旋钮作用于两个池家族: 本池依据 `anthropic-ratelimit-unified-*` 头部,[Codex 池](/zh-cn/guides/codex-multi-account/)依据上报的 `x-codex-*` 5h/7d 窗口(issue #195)。Codex 没有 Fable 专属的 `7d_oi` 窗口,因此 `default_threshold_fable` 在那里不起作用;`priority` 和 `disabled` 在所有池中都适用。
- 管理池端点(`GET /admin/pool`)会报告每个账户的 `priority`、`disabled` 标志,以及在配置了 `[server.pool]` 时该账户当前以秒为单位的余量预测;仪表板的状态列会标记被禁用的账户。

## Usage-API 对账

配额头部只反映流经 shunt 的流量。`usage_refresh_seconds` 通过轮询 `GET /api/oauth/usage`,把权威的使用率与重置时刻应用到同样的 5 小时、共享周(`7d`)和 Fable 专用周(`7d_oi`)窗口,从而弥补这一差距。

字段未设置或为 `0` 时轮询关闭;低于 60 的正值会被取整到 60 秒。只有 imported 的可刷新账户符合条件,长期 `claude setup-token` 与 `token_env` 账户会被跳过,因为其令牌无法调用该端点。间隔在启动时固定,因此配置重载不会启动、停止或重新调整轮询器。这一周期性校正是对反应式头部状态的补充,而非替代。

## 配额状态持久化

池的配额存在内存中,因此重启会从冷状态开始:每个账户在重启后首个响应之前都显示为未观测,这会禁用 burn-rate 规避,并使 `GET /usage` 在流量重新填充池之前返回空值。设置 `state_path` 会把每个账户的按窗口使用率与重置保存到该文件,使池从最后观测到的状态热启动。

该文件是尽力而为的缓存,而非权威来源 —— 配额无论如何都会从上游响应重新导出,因此文件缺失、陈旧或损坏只会导致冷启动,绝不会导致启动失败。写入使用私有 temp 文件(Unix 上为 `0600`)并将其原子重命名覆盖目标,且仅在配额变化时按 15 秒后台定时器进行。写入失败时会保持 dirty 状态,并在下一个 tick 重试。冷却不会被持久化(重启即失效),恢复的窗口中重置已过期的会在恢复后的首次选择或 snapshot 时延迟丢弃。路径在启动时固定;字段未设置时持久化关闭。

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

## 风暴控制(storm control)

设置 `[server.pool] ramp_initial_concurrency`(默认关闭)会按账户身份以 slow-start 逐步放行(ramp)并发准入,这样一次故障转移切换就不会用所有在途请求同时冲垮刚选中的账户。刚开始承接流量的身份最多准入所配置数量的并发请求;每次成功响应把额度翻倍,一次故障转移会重启该 ramp,被拒绝的请求则顺延到选择顺序中的下一个账户(最后一个候选始终会被尝试)。参见 [`[server.pool]`](/zh-cn/reference/configuration/#serverpool可选)。

实现行为参考了 [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) 与随产品发布的 Claude Code 二进制。shunt 对 teamclaude 没有运行时依赖。
