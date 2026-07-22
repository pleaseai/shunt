---
title: CLI
description: shunt コマンドライン — run、check、token、provider login。
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

## `shunt add`

コーディングエージェント向けの組み込み Markdown blueprint を取得します。Blueprint はインストーラーではなく実装ガイドです。このコマンドはファイルの編集、インストール、ネットワークアクセスを行いません。

```bash
shunt add                                      # 両方の blueprint kind を一覧表示
shunt add upstream                             # 名前付き upstream ガイドを一覧表示
shunt add upstream kimi --print                # ガイドを 1 つ出力
shunt add upstream https://example.com/docs    # 互換 endpoint を調査
shunt add provider https://example.com/docs    # ソースコード統合を調査
```

Kind は `upstream`（提供済み preset または互換 endpoint の設定）と `provider`（新しい provider protocol サポートへの貢献）です。既知の upstream slug または alias を指定すると名前付きガイドを取得します。空白や認証情報を含まず正しく解析できる絶対 `http://` または `https://` URL は、その kind の汎用 research ガイドに挿入されます。相対パスや不正な URL は拒否されます。

Blueprint Markdown はエージェントへ直接パイプできるよう、常に stdout へ出力されます。`--print` はその意図を明示して対話的な stderr hint を抑制しますが、stdout の内容は変えません。

```bash
shunt add upstream kimi --print | claude
```

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

## `shunt login claude`

次の 3 つのモードのいずれかで、shunt 管理の Anthropic プールアカウントを作成します。

```bash
# Full OAuth: shunt が新しいリフレッシュ可能なログインを取得して保存します（推奨）。
shunt login claude --name primary --mode oauth

# 現在のリフレッシュ可能な Claude Code ログインをインポートします。
shunt login claude --name imported --mode import

# Claude の 1 年間・推論専用 setup-token フローを実行します。
shunt login claude --name ci --mode setup-token
```

TTY で `--mode` を省略すると、shunt は `oauth`、`import`、`setup-token` の選択を求め、OAuth をデフォルトの推奨値にします。非対話入力では従来の `import` デフォルトを維持します。`--long-lived` は `--mode setup-token` の deprecated alias として残ります。

`--mode oauth` は shunt の full-scope PKCE 認可フローを実行し、access token と refresh token の両方を保存します。デフォルトでは、shunt は `127.0.0.1` に一時リスナーをバインドして認可 URL を開き、ブラウザーが `http://127.0.0.1:<port>/callback` に戻ると完了します。ブラウザーを開けない、リスナーを開始できない、または 5 分以内に callback が届かない場合は、非表示入力の手動貼り付けフローへフォールバックします。SSH や headless 環境では `--manual` ですぐに手動フローを使えます。

```bash
shunt login claude --name remote --mode oauth --manual
```

`--mode import` は `~/.claude/.credentials.json`（または `CLAUDE_CREDENTIALS`）を `~/.shunt/accounts/claude/<name>.json` へコピーします。refresh token を保持し、Claude Code のグローバル設定にある現在のアカウント UUID と関連付けます。shunt は Claude Code の元ファイルを変更せず、このプライベートコピーをリフレッシュします。

`--mode setup-token` は `claude setup-token` と同じ 1 年間・推論専用の PKCE フローを実行します。ブラウザーで承認後、表示された認可コードを shunt の非表示入力プロンプトへ貼り付けます。shunt はコードを直接交換し、opaque token と発行元アカウント UUID の両方を、トークンを表示せずに保存します。

ファイルは Unix で `0700` ディレクトリ内に `0600` でアトミックに書き込まれます。`SHUNT_CLAUDE_ACCOUNTS_DIR` でストアディレクトリを上書きでき、同じ名前を再利用するとファイルを置き換えます。既存の外部 setup token を参照するには `token_env` を使います。`uuid` は必須ではなく、リクエストに埋め込まれたアカウント UUID を書き換えたい場合にのみ指定します — 発行後にアカウント UUID を復元する手段がないため、書き換えが必要なときは明示的に渡してください。

:::caution[リフレッシュ可能なログインごとに owner は 1 つ]
OAuth provider は、shunt が access token をリフレッシュするときに refresh token もローテーションする場合があります。同じリフレッシュ可能な credential ファイルを複数の shunt プロセスで実行したり、稼働中のストアファイルを別ホストへコピーして独立運用したりしないでください。一方で最初にリフレッシュすると、もう一方のコピーが無効になる可能性があります。プロセスごとに個別にプロビジョニングするか、共有する静的 credential が必要な場合はリフレッシュしない setup token を使ってください。
:::

結果は名前だけのプールエントリーで参照するか、provider のアカウントリストを空にしてすべてのストアファイルをスキャンできます。

```toml
[[providers.anthropic.accounts]]
name = "primary"
```

## `shunt login xai`

xAI の device-code OAuth フローを実行し、リフレッシュ可能な credential を保存します。

```bash
shunt login xai
```

## Anthropic アカウントプール認証

`auth = "claude_oauth"` の Anthropic provider では、アカウントに名前だけのストアエントリー、`credentials = "~/.claude/.credentials.json"`、または `token_env = "YOUR_ENV_NAME"` を使えます。ストアエントリーは、上記の Full OAuth、Claude Code ログインのインポート、setup-token フローのいずれかで作成できます。完全な設定と failover ルールは [Anthropic マルチアカウント](/ja/guides/anthropic-multi-account/) を参照してください。

## 環境変数

| 変数 | 効果 |
| :-- | :-- |
| `SHUNT_*`（例 `SHUNT_SERVER__BIND`） | 任意の設定キーをオーバーライド。`__` がネストしたキーを区切る |
| `RUST_LOG` | ログフィルター、例 `shunt=debug` |
| `SHUNT_CLIENT_TOKENS` | [`[server.auth]`](/ja/guides/shared-gateway/) 向けのクライアントトークン（名前は `tokens_env` で設定可能） |
| `SHUNT_GATEWAY_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN` | `shunt token` 向けの静的トークン |
| `CLAUDE_CREDENTIALS` | `shunt token` とリフレッシュ可能な `shunt login claude` インポート向けの代替 credential ファイルパス |
| `SHUNT_CLAUDE_ACCOUNTS_DIR` | shunt 管理の Claude アカウントストア用の代替ディレクトリ |
| `token_env` で指定するアカウント別変数 | Anthropic `claude_oauth` プールエントリーの setup token。値を変更せずに使用 |
| `OPENAI_API_KEY` | `openai` プロバイダーのデフォルトキー環境変数（プロバイダーごとに `api_key_env` で） |
