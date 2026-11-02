---
title: 迁移到存储
description: 将池配额、网关 refresh token 和支出限制从 JSON state_path 快照迁移到 SQLite 或 PostgreSQL 存储,并确认一次性导入已读取每个快照。
---

没有 [`[store]`](/zh-cn/reference/configuration/#store可选) 时,shunt 把池配额、网关 refresh token 和支出限制保存在三个 JSON 快照中,每个子系统一个 `state_path`。添加 `[store]` 后,它们会被一个事务型数据库取代。没有单独的迁移命令:设置 `[store]` 后的第一次 `shunt run` 会把每个快照导入一次,之后该子系统改由存储提供数据。

导入对每个子系统只执行一次,无法重复。请在第一次启动之前把路径设置正确,因为当时没有找到的快照以后不会再被导入。

## 导入的内容

| 子系统 | 快照 | 导入条件 |
| :-- | :-- | :-- |
| 池配额 | `[server.pool] state_path`(无默认值) | 已设置 `[store]` |
| 网关 refresh token | `[server.gateway] state_path`,默认 `~/.shunt/gateway-sessions.json` | 已配置 `[server.gateway]` |
| 支出限制与审计记录 | `[server.spend] state_path`,默认 `~/.shunt/gateway-spend.json` | 已配置 `[server.spend]` |

未配置的子系统会被跳过,也不会留下记录。因此如果之后才启用 `[server.gateway]` 或 `[server.spend]`,它的快照会在启用后的第一次启动时导入。

部分状态仍留在存储之外:provider 凭证文件、管理员浏览器会话、OIDC 状态、冷却状态,以及 `<stem>.counters.json` 中的支出计量计数器。

## 1. 找到并备份快照

列出每个已配置子系统的 `state_path`(包括上面的默认值),并确认文件位于 shunt 进程会去查找的位置。默认值相对于运行 shunt 的用户的主目录解析,在 systemd 或容器中,这可能不是你自己的主目录。shunt 从 `HOME` 读取主目录,未设置时回退到 `USERPROFILE`;两者都未设置时,网关和支出子系统没有默认快照,首次启动会记录 `NoSnapshot`,之后也不会再导入。这种情况下请显式设置每个 `state_path`。

把这些文件复制到安全的地方。shunt 只在导入期间读取它们,附加存储后也不会再写入,但之后想查看迁移前的状态,只能依靠这份副本。

## 2. 添加 `[store]`

保留每个 `state_path` 键不变。设置 `[store]` 后,这些键会告诉 shunt 从哪里导入。

```toml
[store]
backend = "sqlite"
url = "/var/lib/shunt/shunt.db"
```

使用 PostgreSQL 时,设置 `backend = "postgres"` 和 `postgres://` URL;非回环主机需要 `sslmode=verify-full`。所有键见 [`[store]` 参考](/zh-cn/reference/configuration/#store可选)。

## 3. 运行 `shunt check`

```bash
shunt check
```

```text
config ok
store ok: sqlite sqlite:/var/lib/shunt/shunt.db, file does not exist yet; it is created with mode 0600 at boot
```

`check` 以只读方式打开存储,不会迁移、导入或创建 SQLite 文件,因此可以在停止运行中的实例之前执行。配置错误或无法连接的存储会在这里失败,而不是等到启动时。

## 4. 停止运行中的实例

正常停止当前的 shunt。没有存储时,refresh token 和支出限制在每次变更时都会写入快照,因此这些快照已经是最新的。池配额则不同:不使用存储的 shunt 每 15 秒保存一次池配额,关闭时不会再保存,因此快照可能缺少最近最多 15 秒的配额变更。配额会根据上游响应重新推算,代价只是受影响账户短暂冷启动;如果连这一点也想避免,请先停止发送流量,等待 15 秒后再停止 shunt。导入之后,没有使用存储的进程写入快照的内容不会进入存储,因此在旧实例退出之前不要启动新实例。

## 5. 带存储启动 shunt

```bash
shunt run
```

在开始提供服务之前,shunt 会创建或迁移数据库,以每个子系统一个事务的方式导入各个快照,并从存储中恢复状态。启动日志行 `store opened; state is served from it` 会报告每个子系统的结果。

| 结果 | 含义 |
| :-- | :-- |
| `Imported { rows: N }` | 快照已复制到存储中 |
| `NoSnapshot` | 路径未设置或文件不存在;已写入一条记录,因此**该子系统以后不会再导入** |
| `AlreadyImported` | 之前的某次启动已处理过该子系统 |
| `Off` | 该子系统未配置,因此不使用存储 |

导入失败时,启动会以 `importing the <subsystem> state snapshot <path> into the store failed` 加冒号和原因的错误中止。该子系统没有导入任何内容;但如果原因是提交失败,导入可能已经生效,下次启动会把该子系统报告为 `AlreadyImported`,而不会重新读取你修正后的快照。在它之前已导入的子系统的行会保留。请按冒号后的原因处理:快照格式错误或版本不受支持时,修复或删除提示的文件;文件无法读取时,修正其权限或路径;存储事务失败时,检查存储本身。处理后重新启动。其他存储启动错误见 [故障排查](/zh-cn/reference/troubleshooting/)。

## 6. 检查导入结果

- 每个已导入快照旁边通常会生成一个 `<state_path>.imported` 文件,记录后端、时间和快照的 SHA-256。shunt 在导入提交之后写入该文件;如果无法写入(例如快照所在目录为只读),只会记录警告 `imported the snapshot but could not write its .imported sidecar` 并继续运行。缺少该文件并不代表导入失败:以启动日志中的结果和 `import_record` 为准。
- 存储中每个使用存储的子系统有一行 `import_record`。对于 SQLite:

  ```bash
  sqlite3 /var/lib/shunt/shunt.db \
    'SELECT subsystem, source_path, row_count FROM import_record'
  ```

  `source_path` 为 `NULL` 表示该子系统没有找到快照。
- 网关客户端无需重新登录即可继续工作,`GET /v1/organizations/spend_limits`(在 `x-shunt-admin-token`(默认管理请求头)或 `x-api-key` 中携带 `[server.admin]` 凭据,见[端点](/zh-cn/reference/endpoints/))列出的支出限制与之前相同。

## 如果漏掉了某个快照

意料之外的 `NoSnapshot` 通常意味着第一次启动时路径有误:主目录不同,或文件当时还不存在。之后再放入文件不会有任何改变,因为是否导入由 `import_record` 行决定,而不是看表是否为空。

在 SQLite 上,如果自首次启动以来网关没有签发、刷新或撤销过任何令牌,也没有修改过支出限制,请停止 shunt,连同 `-wal` 和 `-shm` 文件一起删除数据库文件,修正路径后重新启动。快照没有变化,因此每个子系统都会重新导入。这期间保存的池配额也会丢失,但会根据上游响应重新推算。令牌或限制发生变化后不要重置:重新导入旧快照会让此后撤销的 refresh token 重新生效,并丢失替代它们签发的令牌。

## 回退

导入是复制,不是同步,shunt 也不会把存储中的状态导出回 JSON。移除 `[store]` 后,每个子系统都会从导入时的快照开始,这可能让已撤销的会话重新生效,并撤销此后所做的支出限制修改。只要快照自导入以来没有变化,不使用存储的启动会记录一条说明这一点的警告。详情见 [回到不使用存储](https://github.com/pleaseai/shunt/blob/main/docs/store.md#going-back-to-no-store)(英文)。
