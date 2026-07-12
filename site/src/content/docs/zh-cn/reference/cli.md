---
title: CLI
description: shunt 命令行 —— run、check 和 token。
---

## `shunt run`

启动网关。`run` 是默认子命令,因此裸的 `shunt` 也可以。

```bash
shunt run
shunt run --config /path/to/shunt.toml
```

启动时它会用绑定地址(默认 `127.0.0.1:3001`)记录 `shunt listening`。用 `RUST_LOG` 设置日志详细程度,例如 `RUST_LOG=shunt=debug shunt run`。

不带 `--config` 时,shunt 依次搜索 `./shunt.toml` → `~/.config/shunt/shunt.toml` → `$HOMEBREW_PREFIX/etc/shunt.toml`;带 `--config` 时,文件缺失即报错。见 [配置](/zh-cn/guides/configuration/)。

## `shunt check`

校验解析后的配置并退出(`shunt --check` 也可以):

```bash
shunt check
# -> config ok
```

报告具体错误:错误的 bind 地址、路由中的未知提供方、缺失的 `api_key_env`、错误的 `base_url`、错误的适配器/认证组合。

## `shunt token`

将一个 Claude 订阅 OAuth token 打印到 **stdout**(日志走 stderr),设计用来接入 Claude Code 的 `apiKeyHelper`。两种模式:

- **静态** —— 如果设置了 `SHUNT_GATEWAY_TOKEN` 或 `CLAUDE_CODE_OAUTH_TOKEN`,原样回显该值。把它指向一个 `claude setup-token` 值,则从不刷新任何东西。
- **自动刷新** —— 否则读取 `~/.claude/.credentials.json`(用 `CLAUDE_CREDENTIALS` 覆盖路径),返回 `claudeAiOauth` 访问 token,并在它距 `expiresAt` 5 分钟以内时,针对 `platform.claude.com/v1/oauth/token`(与 Claude Code 使用的同一授权)刷新它,然后以 `0600` 原子写回新 token,保留其他所有字段。刷新只在实际过期时发生,以尊重该端点的速率限制。

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

何时需要它,见 [连接 Claude Code](/zh-cn/guides/connect-claude-code/#2-choose-the-anthropic-credential)。

## 环境变量

| 变量 | 效果 |
| :-- | :-- |
| `SHUNT_*`(如 `SHUNT_SERVER__BIND`) | 覆盖任意配置键;`__` 分隔嵌套键 |
| `RUST_LOG` | 日志过滤器,如 `shunt=debug` |
| `SHUNT_CLIENT_TOKENS` | 面向 [`[server.auth]`](/zh-cn/guides/shared-gateway/) 的客户端 token(名称可通过 `tokens_env` 配置) |
| `SHUNT_GATEWAY_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN` | 面向 `shunt token` 的静态 token |
| `CLAUDE_CREDENTIALS` | 面向 `shunt token` 的备用凭据文件路径 |
| `OPENAI_API_KEY` | `openai` 提供方的默认密钥环境变量(每个提供方通过 `api_key_env`) |
