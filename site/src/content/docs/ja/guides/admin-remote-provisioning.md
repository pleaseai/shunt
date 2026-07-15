---
title: 管理とリモートプロビジョニング
description: shunt の管理 Web サーフェスを有効化し、Claude と Codex のアカウントをリモートでプロビジョニングして、アカウントプールの状態を確認する。
---

shunt は、上流の Claude と Codex/ChatGPT のアカウントをプロビジョニングし、`claude_oauth` と `chatgpt_oauth` のアカウントプール状態を表示する、管理者認証付きの Web サーフェスを公開できます。これはオプトインです。`[server.admin]` がなければ `/admin*` ルートは一切登録されず、shunt のデフォルトの HTTP サーフェスは変わりません。

これは [Anthropic マルチアカウント](/ja/guides/anthropic-multi-account/)のストアと選択の挙動の上に成り立っています。ブラウザーフォームでは、リフレッシュ可能な Full OAuth アカウントまたは 1 年間・推論専用の setup token アカウントを作成できます。既存の Claude Code credential ファイルのインポートは CLI 専用です。

## 管理サーフェスを有効化する

オプションのテーブルを追加し、設定した環境変数から管理者の認証情報を 1 つ以上提供します。

```toml
[server.admin]                        # all keys optional; defaults shown
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

認証情報は `SHUNT_CLIENT_TOKENS` と同じカンマ区切りの `name:token` 形式を使いますが、別個のセキュリティ境界です。`[server.auth]` のクライアントトークンを管理トークンとして再利用しないでください。`[server.admin]` が存在するのにそのトークン環境変数が未設定・空・不正な場合、起動はフェイルクローズします。

すべてのキーとデフォルトは[設定リファレンス](/ja/reference/configuration/#serveradminオプション)を参照してください。ブラウザーおよび JSON のルートは[エンドポイントリファレンス](/ja/reference/endpoints/)に一覧があります。

## ブラウザーで Claude アカウントをプロビジョニングする

1. `/admin` を開き、管理トークンでサインインします。
2. 小文字・数字・ハイフンのみからなるアカウント名を入力します。
3. **Full OAuth (refreshable)**（ダッシュボードのデフォルト）または **Setup token (1-year, inference-only)** を選び、**Start** を選択します。
4. 表示された認可 URL を別のタブで開きます。対象の Claude アカウントにサインインし、アクセスを承認します。
5. 得られた `<code>#<state>` の値を管理ページに貼り付け、**Complete** を選択します。
6. shunt がアカウントを保存します。`accounts` リストが空の provider は、再起動なしで次のリクエスト時にそれを拾います。そうでなければ、名前だけのエントリーを追加して reload します。

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

開始されたフローは `pending_ttl_secs`（デフォルト 10 分）の間有効で、オペレーターが認可ページを開いて結果を貼り付ける時間を確保できます。サーバーは選択された mode を pending attempt とともに記録するため、completion リクエストで token の種類を切り替えることはできません。Full OAuth は access token と refresh token を保存し、credential kind は `imported` と表示されます。setup-token mode は kind が `setup_token` の静的 credential を保存します。完了レスポンスは、アカウントが保存されたか、そして現在の provider 設定でそれが有効（live）になるかを報告します。

アカウントストアの変更はリクエストごとに検出されるため、スキャンモードのプロバイダーはアカウントの追加・削除後に再起動する必要がありません。

## ブラウザーで Codex アカウントをプロビジョニングする

1. **Add Codex account** で小文字のアカウント名を入力し、**Start Codex login** を選択します。
2. 認可 URL を開き、対象の ChatGPT アカウントにサインインしてアクセスを承認します。
3. ブラウザーは `http://localhost:1455/auth/callback` へ移動します。ローカルページが読み込めなくても正常です。
4. ブラウザーのアドレスバーから **URL 全体**をコピーして管理ページへ貼り付け、**Complete Codex login** を選択します。JSON API では `<code>#<state>` も使用できます。
5. shunt が code を交換し、リフレッシュ可能な Codex credential を非公開のアカウントファイルへ保存します。

`accounts` が空の `chatgpt_oauth` provider（デフォルトの `codex` を含む）は、次のリクエストで新しいアカウントを検出します。明示的なアカウント一覧を使う場合は、名前だけの entry を追加してください。`SHUNT_CODEX_TOKEN_URL` はローカル統合テスト用の token endpoint override です。production では設定しないでください。

## プールの健全性を確認する

ダッシュボードは、`auth = "claude_oauth"` または `auth = "chatgpt_oauth"` の provider について、アカウントストアのメタデータと現在の状態を表示します。Claude の行には上流から観測したクォータ使用率が表示されます。Codex はクォータヘッダーを送らないため、使用率の列は `—` のままです。shunt は Codex の使用量を推測・解析しません。

アカウント一覧が公開するのはメタデータのみです。アカウント名、認証情報の種類（`setup_token` または `imported`）、有効期限、UUID。トークン本体を返すことはありません。shunt がアカウント選択時にクォータ状態・クールダウン・モデルを認識した週次バケットをどう使うかは [Anthropic マルチアカウント](/ja/guides/anthropic-multi-account/#選択とプロアクティブなローテーション)を参照してください。

アカウントの metadata、プールの健全性、プロビジョニング、またはアカウント削除に API/curl でアクセスするには、設定したヘッダー（デフォルト `x-shunt-admin-token`）で管理トークンを送り、[HTTP エンドポイント](/ja/reference/endpoints/)に記載の JSON route を使ってください。ヘッダー認証されたリクエストはブラウザーセッションを使わず、CSRF チェックの対象外です。プロビジョニングの開始時には `{ "name": "backup", "mode": "oauth" }` または `mode: "setup_token"` を送ります。`mode` を省略した場合は、API の後方互換性のため `setup_token` がデフォルトです。

## CLI と SSH のフォールバック

shunt ホストにブラウザーで到達できない場合は CLI を使ってください。Full OAuth は通常、ブラウザーを開き、一時的な `127.0.0.1` callback で完了します。SSH または headless 環境では、管理ページと同じ手動貼り付け redirect を強制します。

```bash
shunt login claude --name backup --mode oauth --manual
```

代わりにホストの現在のリフレッシュ可能な Claude Code ログインをインポートするには、次を実行します。

```bash
shunt login claude --name primary --mode import
```

1 年間・推論専用の credential を作成するには、次を実行します。

```bash
shunt login claude --name ci --mode setup-token
```

`--long-lived` は `--mode setup-token` の deprecated alias です。管理サーフェスは Claude の Full OAuth/setup-token と Codex の ChatGPT OAuth プロビジョニングをサポートします。既存 credential ファイルの import だけはホストへのアクセスが必要なため CLI 専用です。

:::caution[Refresh token のローテーション]
リフレッシュ可能なアカウントには、稼働中の owner を 1 つだけ割り当ててください。OAuth のリフレッシュは refresh token を置き換えて古いコピーを無効にする場合があるため、1 つのストアファイルをプロセス間で共有したり、別ホストへコピーして独立運用したりしないでください。プロセスごとに個別にプロビジョニングするか、リフレッシュしない静的 credential が適切な場合は setup-token mode を選んでください。
:::

## セキュリティ

- 管理サーフェスは HTTPS、または WireGuard や Tailscale のような信頼できるトンネルの背後に置いてください。shunt 自身は平文 HTTP を提供します。リモートに公開する場合は前段で TLS 終端を使ってください。
- 強力な管理トークンを生成し、`[server.auth]` のクライアント認証情報とは分けて管理してください。管理アクセスは上流アカウントを追加・削除できます。
- ブラウザーログインは HttpOnly かつ SameSite=Strict のセッションクッキーを作成します。クッキーはループバックホストを除いて Secure なので、ローカルの HTTP 開発は引き続き動作します。
- 変更を伴うブラウザーリクエストにはセッションごとの `x-csrf-token` が必要で、同一オリジンチェックを通過します。API/curl 呼び出しは代わりに管理ヘッダーで認証し、アンビエントなクッキー権限を持ちません。
- プロビジョニングの完了はレート制限されます。shunt はトークン本体をログにも応答にも出さず、アカウントの追加と削除はアカウント名で監査ログに記録されます。

`[server.admin]` がなければ、これらのルートは存在しません。これは未使用のダッシュボードを認証なしで放置するより強力です。明示的に有効化しない限り、管理サーフェスは存在しません。
