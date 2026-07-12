---
title: 配置
description: shunt 如何加载配置 —— 文件、环境变量以及路由。
---

shunt 按优先级递增的顺序从以下来源加载配置:

1. **内置默认值** —— 每个提供方(`anthropic`、`openai`、`codex`……)都已预配置。
2. 一个 **TOML 文件**。使用 `--config <path>` 时会使用该确切文件(文件缺失即报错)。否则 shunt 取以下位置中找到的第一个文件:
   - `./shunt.toml`
   - `$XDG_CONFIG_HOME/shunt/shunt.toml`(默认为 `~/.config/shunt/shunt.toml`)
   - `$HOMEBREW_PREFIX/etc/shunt.toml`(默认为 `/opt/homebrew` 和 `/usr/local` 前缀)

   启动日志会报告加载了哪个文件,或者正在使用默认值。
3. 以 `SHUNT_` 为前缀的**环境变量**,用 `__` 表示嵌套键 —— 例如 `SHUNT_SERVER__BIND=0.0.0.0:3001`。

由于默认值已经定义了每个提供方,你的 `shunt.toml` 只需包含你想改动的部分。从 [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example) 开始。

## 带注释的示例

```toml
[server]
bind = "127.0.0.1:3001"        # shunt 监听的地址
default_provider = "anthropic" # 面向任何无路由匹配的模型的提供方(透传)

# 每个提供方都是一个 [providers.<name>] 表。
[providers.anthropic]
kind = "anthropic"             # 原样转发 Claude Code 自己的凭据
base_url = "https://api.anthropic.com"

[providers.openai]
kind = "responses"             # 将 Anthropic Messages 转换为 OpenAI Responses
base_url = "https://api.openai.com/v1"
auth = "api_key"
api_key_env = "OPENAI_API_KEY" # 读取 OpenAI 密钥的环境变量
# effort = "high"              # 该提供方可选的默认推理力度

[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"         # 复用 ~/.codex/auth.json
# effort = "high"

# --- 路由:请求的 `model` id 如何选取提供方 ---

# 精确匹配最先胜出。`upstream_model` 和 `effort` 是可选覆盖。
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
# upstream_model = "gpt-5.6-sol"
# effort = "high"

# 然后是前缀匹配。
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"

# 可选:通过发现在 /model 选择器中暴露 Claude 命名的别名。
# id 必须以 "claude" 或 "anthropic" 开头,否则 Claude Code 会忽略它。
# [[models]]
# id = "claude-opus-via-codex"
# display_name = "Opus (via Codex)"
```

## 路由优先级

1. 对请求的 `model` id 的精确 `[[routes]]` 匹配。
2. `[[route_prefixes]]` 前缀匹配。
3. `server.default_provider` —— 默认为 `anthropic`,因此无匹配的模型会原样透传给 Anthropic。

一条路由可以按模型覆盖转发的模型 id(`upstream_model`)和推理力度(`effort`)。

## 部分覆盖

配置映射是深度合并的,因此对内置提供方的部分覆盖会保留其余默认值:

```toml
# 只提高 codex 的默认力度;其他一切保持内置值。
[providers.codex]
effort = "high"
```

## 校验

```bash
shunt check
# -> 打印 "config ok",或一个具体错误(错误的 bind 地址、未知提供方……)
```

每个键见 [配置参考](/zh-cn/reference/configuration/),添加新后端见 [提供方](/zh-cn/guides/providers/)。
