---
title: 管理与远程预配
description: 启用 shunt 的管理 Web 界面,以远程预配 Claude 与 Codex 账户并查看账户池状态。
---

shunt 可以暴露一个需管理员认证的 Web 界面,用于预配上游 Claude 与 Codex/ChatGPT 账户,并查看 `claude_oauth` 与 `chatgpt_oauth` 账户池的状态。它是可选启用的:当 `[server.admin]` 不存在时,任何 `/admin*` 路由都不会注册,shunt 默认的 HTTP 暴露面保持不变。

它建立在 [Anthropic 多账户](/zh-cn/guides/anthropic-multi-account/)的存储与选择行为之上。浏览器表单可以创建可刷新的 Full OAuth 账户或一年期、仅推理的 setup token 账户。导入已有的 Claude Code credential 文件仍然仅限 CLI。

## 启用管理界面

添加这个可选表,并通过配置的环境变量提供至少一个管理员凭据:

```toml
[server.admin]                        # 所有键都可选;显示的是默认值
header = "x-shunt-admin-token"
tokens_env = "SHUNT_ADMIN_TOKENS"
session_ttl_secs = 3600
pending_ttl_secs = 600
```

```bash
export SHUNT_ADMIN_TOKENS="ops:$(openssl rand -hex 32)"
shunt check
shunt run
```

凭据使用与 `SHUNT_CLIENT_TOKENS` 相同的逗号分隔 `name:token` 格式,但它们是相互独立的安全边界。不要把 `[server.auth]` 的客户端 token 复用为管理员 token。如果存在 `[server.admin]` 但其 token 环境变量未设置、为空或格式错误,启动会安全失败(fail closed)。

每个键与默认值见[配置参考](/zh-cn/reference/configuration/#serveradmin可选)。[端点参考](/zh-cn/reference/endpoints/)列出了浏览器路由和 JSON 路由。

## 在浏览器中预配 Claude 账户

1. 打开 `/admin`,用管理员 token 登录。
2. 输入一个只含小写字母、数字和连字符的账户名。
3. 选择 **Full OAuth (refreshable)**(仪表盘默认值)或 **Setup token (1-year, inference-only)**,然后选择 **Start**。
4. 在另一个标签页打开显示的授权 URL。登录目标 Claude 账户并批准访问。
5. 把得到的 `<code>#<state>` 值复制回管理页面,选择 **Complete**。
6. shunt 存储该账户。`accounts` 列表为空的 provider 会在下一个请求时拾取它,无需重启。否则,添加一个只带名称的条目并 reload:

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

已开始的流程在 `pending_ttl_secs`(默认 10 分钟)内保持有效,给运营者留出打开授权页并粘贴结果的时间。服务器把选定的 mode 与 pending attempt 一起记录,所以 completion 请求无法切换 token 类型。Full OAuth 存储 access token 与 refresh token,其 credential kind 显示为 `imported`;setup-token mode 存储 kind 为 `setup_token` 的静态 credential。完成响应会报告账户是否已存储,以及当前的 provider 配置是否让它生效。

账户存储的变化按请求发现,因此扫描模式的提供方在账户增删后不需要重启。

## 在浏览器中预配 Codex 账户

1. 在 **Add Codex account** 中输入小写账户名,选择 **Start Codex login**。
2. 打开授权 URL,登录目标 ChatGPT 账户并批准访问。
3. 浏览器会跳转到 `http://localhost:1455/auth/callback`。本地页面无法加载是正常现象。
4. 从浏览器地址栏复制**完整 URL**,粘贴回管理页面并选择 **Complete Codex login**。JSON API 也接受 `<code>#<state>`。
5. shunt 交换 code,并把可刷新的 Codex credential 写入私有账户文件。

`accounts` 为空的 `chatgpt_oauth` provider(包括默认 `codex`)会在下一个请求中发现新账户。若使用显式账户列表,请添加一个只含名称的 entry。`SHUNT_CODEX_TOKEN_URL` 仅用于本地集成测试覆盖 token endpoint;生产环境请保持未设置。

## 查看池健康状况

仪表盘展示配置了 `auth = "claude_oauth"` 或 `auth = "chatgpt_oauth"` 的 provider 的账户存储元数据与当前状态。Claude 行显示从上游观测到的配额使用率。Codex 不发送配额 header,因此使用率列保持为 `—`;shunt 不会推断或解析 Codex 使用量。

账户列表只暴露元数据:账户名、凭据类型(`setup_token` 或 `imported`)、过期时间和 UUID。它绝不返回 token 材料。shunt 在选择账户时如何使用配额状态、冷却和感知模型的周桶,见 [Anthropic 多账户](/zh-cn/guides/anthropic-multi-account/#选择与主动轮换)。

要通过 API/curl 访问账户 metadata、池健康状况、预配或删除账户,请在配置的头部(默认 `x-shunt-admin-token`)中发送管理员 token,并使用 [HTTP 端点](/zh-cn/reference/endpoints/)中记录的 JSON route。头部认证的请求不使用浏览器会话,免于 CSRF 检查。开始预配时发送 `{ "name": "backup", "mode": "oauth" }` 或 `mode: "setup_token"`;省略 `mode` 时,为保持 API 向后兼容,默认使用 `setup_token`。

## CLI 与 SSH 回退

当无法在浏览器中触达 shunt 主机时,请使用 CLI。Full OAuth 通常会打开浏览器,并通过临时的 `127.0.0.1` callback 完成。在 SSH 或 headless 环境中,强制使用与管理页面相同的手动粘贴 redirect:

```bash
shunt login claude --name backup --mode oauth --manual
```

若要导入主机当前可刷新的 Claude Code 登录:

```bash
shunt login claude --name primary --mode import
```

若要创建一年期、仅推理的 credential:

```bash
shunt login claude --name ci --mode setup-token
```

`--long-lived` 保留为 `--mode setup-token` 的 deprecated alias。管理界面支持 Claude Full OAuth/setup-token 与 Codex ChatGPT OAuth 预配;只有导入已有 credential 文件需要主机访问,因此仍限 CLI。

:::caution[Refresh token 轮换]
一个可刷新账户只能有一个正在运行的 owner。OAuth 刷新可能替换 refresh token 并使旧副本失效,因此不要在进程间共享一个存储文件,也不要复制到另一台主机后独立运行。请为每个进程分别预配;如果适合使用不可刷新的静态 credential,请选择 setup-token mode。
:::

## 安全

- 把管理界面放在 HTTPS 或 WireGuard、Tailscale 等可信隧道之后。shunt 本身提供纯 HTTP;对外暴露时请在前面做 TLS 终止。
- 生成强管理员 token,并与 `[server.auth]` 客户端凭据分开保管。管理员访问可以添加和删除上游账户。
- 浏览器登录创建 HttpOnly、SameSite=Strict 的会话 cookie。除回环主机外该 cookie 为 Secure,因此本地 HTTP 开发仍可用。
- 产生变更的浏览器请求需要按会话的 `x-csrf-token` 并通过同源检查。API/curl 调用改用管理员头部认证,不携带环境(ambient)cookie 权限。
- 预配完成受速率限制。shunt 从不记录或返回 token 材料,账户的添加与删除会按账户名记入审计日志。

没有 `[server.admin]`,这些路由就不存在。这比留着一个未认证的闲置仪表盘更强:除非显式启用,管理界面根本不存在。
