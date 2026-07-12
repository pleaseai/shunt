---
title: CLI
description: shunt コマンドライン — run、check、token。
---

## `shunt run`

ゲートウェイを起動します。`run` はデフォルトのサブコマンドなので、素の `shunt` でも動作します。

```bash
shunt run
shunt run --config /path/to/shunt.toml
```

起動時に、バインドされたアドレス（デフォルト `127.0.0.1:3001`）と共に `shunt listening` をログに記録します。ログの詳細度は `RUST_LOG` で設定します。例 `RUST_LOG=shunt=debug shunt run`。

`--config` なしの場合、shunt は `./shunt.toml` → `~/.config/shunt/shunt.toml` → `$HOMEBREW_PREFIX/etc/shunt.toml` を検索します。`--config` ありの場合、ファイルが存在しないとエラーになります。[Configuration](/ja/guides/configuration/) を参照してください。

## `shunt check`

解決された設定を検証して終了します（`shunt --check` でも動作します）。

```bash
shunt check
# -> config ok
```

具体的なエラーを報告します: 不正なバインドアドレス、ルート内の未知のプロバイダー、`api_key_env` の欠落、不正な `base_url`、誤ったアダプター/認証の組み合わせ。

## `shunt token`

Claude サブスクリプションの OAuth トークンを **stdout** に出力します（ログは stderr へ）。Claude Code の `apiKeyHelper` に組み込むよう設計されています。2 つのモード:

- **静的** — `SHUNT_GATEWAY_TOKEN` または `CLAUDE_CODE_OAUTH_TOKEN` が設定されている場合、その値を変更せずに出力します。`claude setup-token` の値を指定すれば、何もリフレッシュされません。
- **自動リフレッシュ** — それ以外の場合、`~/.claude/.credentials.json`（パスは `CLAUDE_CREDENTIALS` でオーバーライド）を読み込み、`claudeAiOauth` のアクセストークンを返し、`expiresAt` の 5 分前以内になったとき `platform.claude.com/v1/oauth/token`（Claude Code が使うのと同じグラント）に対してリフレッシュし、新しいトークンを他のすべてのフィールドを保ったまま `0600` でアトミックに書き戻します。リフレッシュは、エンドポイントのレート制限を尊重するため、実際の期限切れ時にのみ発生します。

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

これが必要になる場面については [Connect Claude Code](/ja/guides/connect-claude-code/#2-choose-the-anthropic-credential) を参照してください。

## 環境変数

| 変数 | 効果 |
| :-- | :-- |
| `SHUNT_*`（例 `SHUNT_SERVER__BIND`） | 任意の設定キーをオーバーライド。`__` がネストしたキーを区切る |
| `RUST_LOG` | ログフィルター、例 `shunt=debug` |
| `SHUNT_CLIENT_TOKENS` | [`[server.auth]`](/ja/guides/shared-gateway/) 向けのクライアントトークン（名前は `tokens_env` で設定可能） |
| `SHUNT_GATEWAY_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN` | `shunt token` 向けの静的トークン |
| `CLAUDE_CREDENTIALS` | `shunt token` 向けの代替認証情報ファイルパス |
| `OPENAI_API_KEY` | `openai` プロバイダーのデフォルトキー環境変数（プロバイダーごとに `api_key_env` で） |
