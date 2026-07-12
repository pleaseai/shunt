---
title: Configuration
description: shunt が設定を読み込む方法 — ファイル、環境変数、ルーティング。
---

shunt は、優先順位の低い方から順に以下から設定を読み込みます。

1. **組み込みのデフォルト** — すべてのプロバイダー（`anthropic`、`openai`、`codex`、…）が事前設定済みです。
2. **TOML ファイル**。`--config <path>` を指定するとその正確なファイルが使われます（ファイルが存在しないとエラーになります）。それ以外の場合、shunt は以下で最初に見つかったファイルを使います。
   - `./shunt.toml`
   - `$XDG_CONFIG_HOME/shunt/shunt.toml`（デフォルト `~/.config/shunt/shunt.toml`）
   - `$HOMEBREW_PREFIX/etc/shunt.toml`（デフォルトの `/opt/homebrew` および `/usr/local` プレフィックス）

   起動ログには、どのファイルが読み込まれたか、またはデフォルトが使われていることが報告されます。
3. **環境変数**。`SHUNT_` プレフィックス付きで、ネストしたキーには `__` を使います — 例 `SHUNT_SERVER__BIND=0.0.0.0:3001`。

デフォルトがすでにすべてのプロバイダーを定義しているため、`shunt.toml` には変更したい部分だけを書けば済みます。[`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example) から始めてください。

## 注釈付きの例

```toml
[server]
bind = "127.0.0.1:3001"        # address shunt listens on
default_provider = "anthropic" # provider for any model with no route (pass-through)

# Each provider is a [providers.<name>] table.
[providers.anthropic]
kind = "anthropic"             # forward Claude Code's own credential unchanged
base_url = "https://api.anthropic.com"

[providers.openai]
kind = "responses"             # translate Anthropic Messages -> OpenAI Responses
base_url = "https://api.openai.com/v1"
auth = "api_key"
api_key_env = "OPENAI_API_KEY" # env var the OpenAI key is read from
# effort = "high"              # optional default reasoning effort for this provider

[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"         # reuses ~/.codex/auth.json
# effort = "high"

# --- Routing: how a request's `model` id picks a provider ---

# Exact match wins first. `upstream_model` and `effort` are optional overrides.
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
# upstream_model = "gpt-5.6-sol"
# effort = "high"

# Then prefix match.
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"

# Optional: expose Claude-named aliases in the /model picker via discovery.
# The id MUST start with "claude" or "anthropic" or Claude Code ignores it.
# [[models]]
# id = "claude-opus-via-codex"
# display_name = "Opus (via Codex)"
```

## ルーティング優先順位

1. リクエストの `model` id に対する厳密な `[[routes]]` マッチ。
2. `[[route_prefixes]]` のプレフィックスマッチ。
3. `server.default_provider` — デフォルトは `anthropic` なので、マッチしないモデルは変更なしで Anthropic へフォールスルーします。

ルートは、転送されるモデル id（`upstream_model`）と推論エフォート（`effort`）をモデルごとにオーバーライドできます。

## 部分的なオーバーライド

設定マップはディープマージされるため、組み込みプロバイダーを部分的にオーバーライドしても残りのデフォルトは保たれます。

```toml
# Only raise codex's default effort; everything else stays at the built-in values.
[providers.codex]
effort = "high"
```

## 検証

```bash
shunt check
# -> prints "config ok", or a specific error (bad bind address, unknown provider, …)
```

すべてのキーについては [Configuration Reference](/ja/reference/configuration/) を、新しいバックエンドの追加については [Providers](/ja/guides/providers/) を参照してください。
