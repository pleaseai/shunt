---
title: 管理与远程预配
description: 启用 shunt 的管理 Web 界面,以远程预配 Claude 账户并查看账户池健康状况。
---

shunt 可以暴露一个需管理员认证的 Web 界面,用于预配上游 Claude 账户并查看每个 `claude_oauth` 账户池的健康状况。它是可选启用的:当 `[server.admin]` 不存在时,任何 `/admin*` 路由都不会注册,shunt 默认的 HTTP 暴露面保持不变。

它建立在 [Anthropic 多账户](/zh-cn/guides/anthropic-multi-account/)的存储与选择行为之上。浏览器流程创建一年期、仅推理的 setup token 账户;导入可刷新的 Claude Code 登录仍然仅限 CLI。

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

## 在浏览器中预配账户

1. 打开 `/admin`,用管理员 token 登录。
2. 输入一个只含小写字母、数字和连字符的账户名,然后选择 **Start**。
3. 在另一个标签页打开显示的授权 URL。登录目标 Claude 账户并批准访问。
4. 把得到的 `<code>#<state>` 值复制回管理页面,选择 **Complete**。
5. shunt 存储该账户。`accounts` 列表为空的提供方会在下一个请求时拾取它,无需重启。否则,添加一个只带名字的条目并重载:

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

已开始的流程在 `pending_ttl_secs`(默认 10 分钟)内保持有效,给运营者留出打开授权页并粘贴结果的时间。完成响应会报告账户是否已存储,以及当前的提供方配置是否让它生效。

账户存储的变化按请求发现,因此扫描模式的提供方在账户增删后不需要重启。

## 查看池健康状况

仪表盘展示每个配置了 `auth = "claude_oauth"` 的提供方的账户存储元数据与当前健康状况。其中包括从上游响应观测到的 5 小时、共享 7 天和 `7d_oi` 使用率,以及 unified status、剩余冷却时间、接近配额状态,和该账户当前是否可用。

账户列表只暴露元数据:账户名、凭据类型(`setup_token` 或 `imported`)、过期时间和 UUID。它绝不返回 token 材料。shunt 在选择账户时如何使用配额状态、冷却和感知模型的周桶,见 [Anthropic 多账户](/zh-cn/guides/anthropic-multi-account/#选择与主动轮换)。

要通过 API/curl 访问账户元数据、池健康状况或删除账户,请在配置的头部(默认 `x-shunt-admin-token`)中发送管理员 token,并使用 [HTTP 端点](/zh-cn/reference/endpoints/)中记录的 JSON 路由。头部认证的请求不使用浏览器会话,免于 CSRF 检查;setup token 的预配请通过上面的仪表盘流程进行。

## SSH 与可刷新导入的兜底

当无法在浏览器中触达 shunt 主机,或需要可刷新的导入登录时,使用 CLI。通过 SSH,long-lived 流程会打印一个可以在笔记本上打开的授权 URL,并在远程终端接受返回的代码:

```bash
shunt login claude --name backup --long-lived
```

若要改为导入主机当前可刷新的 Claude Code 登录,省略 `--long-lived`:

```bash
shunt login claude --name primary
```

浏览器管理流程有意只支持 setup token 预配。可刷新导入会读取主机的 Claude Code 凭据,因此保持仅限 CLI。

## 安全

- 把管理界面放在 HTTPS 或 WireGuard、Tailscale 等可信隧道之后。shunt 本身提供纯 HTTP;对外暴露时请在前面做 TLS 终止。
- 生成强管理员 token,并与 `[server.auth]` 客户端凭据分开保管。管理员访问可以添加和删除上游账户。
- 浏览器登录创建 HttpOnly、SameSite=Strict 的会话 cookie。除回环主机外该 cookie 为 Secure,因此本地 HTTP 开发仍可用。
- 产生变更的浏览器请求需要按会话的 `x-csrf-token` 并通过同源检查。API/curl 调用改用管理员头部认证,不携带环境(ambient)cookie 权限。
- 预配完成受速率限制。shunt 从不记录或返回 token 材料,账户的添加与删除会按账户名记入审计日志。

没有 `[server.admin]`,这些路由就不存在。这比留着一个未认证的闲置仪表盘更强:除非显式启用,管理界面根本不存在。
