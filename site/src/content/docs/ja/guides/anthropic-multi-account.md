---
title: Anthropic マルチアカウント
description: 複数の Claude サブスクリプション OAuth アカウントをプールし、セッションスティッキーでモデルを認識したプロアクティブなローテーションとリアクティブなフェイルオーバーで運用する。
---

shunt は、組み込みの `anthropic` プロバイダーの背後で複数の Claude サブスクリプション OAuth 認証情報をプールできます。Claude Code が `x-claude-code-session-id` を送る場合、リクエストはセッションスティッキーになります。ヘッダーがないリクエストはプロバイダーごとのラウンドロビンを使います。shunt は各アカウントの上流クォータヘッダーを追跡し、スティッキーなアカウントがモデルに関係するクォータに近づくとプロアクティブにローテーションします。クォータ拒否、認証失敗、上流障害に対しては、リアクティブなフェイルオーバーが安全網として残ります。

:::caution[サブスクリプション規約]
サブスクリプションの認証情報は、アカウント規約が許す範囲でのみ使用してください。shunt は非公式クライアントであり、Anthropic のアカウント/サブスクリプションポリシーを変えるものではありません。
:::

## プールを設定する

`auth = "claude_oauth"` を設定し、明示的なアカウントエントリーを追加します。

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

# Existing Claude Code credentials file. shunt refreshes and writes it back.
[[providers.anthropic.accounts]]
name = "primary"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000" # optional

# Long-lived `claude setup-token` value. Used verbatim; not refreshed.
[[providers.anthropic.accounts]]
name = "backup"
token_env = "CLAUDE_BACKUP_OAUTH_TOKEN"
uuid = "11111111-1111-1111-1111-111111111111" # optional
```

```bash
export CLAUDE_BACKUP_OAUTH_TOKEN='<value from claude setup-token>'
shunt check
shunt run
```

3 つの Claude ログインモードのいずれかでアカウントを保存できます。

```bash
# 新しいリフレッシュ可能なログインを作成（デフォルトは自動 localhost callback）。
shunt login claude --name primary --mode oauth

# 現在のリフレッシュ可能な Claude Code ログインをインポート。
shunt login claude --name imported --mode import

# 1 年間・推論専用の setup token を生成して保存。
shunt login claude --name backup --mode setup-token
```

TTY で `--mode` を省略すると、OAuth がデフォルト選択された 3 方式のプロンプトが開きます。非対話入力では従来の `import` デフォルトを維持します。`--long-lived` は `--mode setup-token` の deprecated alias です。Full OAuth は通常、一度限りの `127.0.0.1` callback で完了します。`<code>#<state>` を貼り付ける場合は `--manual` を使ってください。ブラウザー起動、callback の bind、または 5 分間の待機に失敗した場合も、shunt は手動貼り付けへフォールバックします。

その後は名前だけのエントリーを使います。

```toml
[[providers.anthropic.accounts]]
name = "primary"

[[providers.anthropic.accounts]]
name = "backup"
```

ストアファイルは `~/.shunt/accounts/claude/<name>.json` に置かれます。`SHUNT_CLAUDE_ACCOUNTS_DIR` でディレクトリを上書きできます。設定された `accounts` リストが空の場合、shunt はストアをスキャンし、有効な JSON アカウントファイルすべてをファイル名順に使います。ストアファイルはプライベートです（Unix では `0600`、ディレクトリは `0700`）。

リモートのオペレーターは、オプトインの[管理 Web サーフェス](/ja/guides/admin-remote-provisioning/)から、ブラウザーでリフレッシュ可能な Full OAuth アカウントまたは 1 年間の setup token アカウントをプロビジョニングし、プールの現在の健全性を表示できます。既存の credential ファイルのインポートは CLI 専用です。

Full OAuth は新しいリフレッシュ可能な credential を作成します。import は現在の `~/.claude/.credentials.json` credential を shunt のストアへコピーします。どちらもリフレッシュ能力を保持し、import は現在のアカウント UUID も記録します。setup-token モードは `claude setup-token` と同じ 1 年間・推論専用の PKCE フローを実行します。承認後、shunt は表示された認可コードを交換し、トークンとその発行元アカウントの UUID の両方を、トークンを表示せずに保存します。これにより、プールが別のアカウントを選んだときも `metadata.user_id.account_uuid` の整合が保たれます。同じ名前を再利用すると、そのアカウントのストアファイルは置き換えられます。既存の外部 setup token には、引き続き `token_env` と明示的な `uuid` が必要です。

:::caution[Refresh token のローテーション]
リフレッシュに成功すると、置き換え用の refresh token が返され、以前の値が無効になる場合があります。リフレッシュ可能なストアファイルごとに、稼働中の shunt owner は 1 つだけにしてください。複数プロセスで同じファイルを参照したり、別ホストでコピーを独立運用したりしないでください。プロセスごとに個別にプロビジョニングするか、リフレッシュしない credential を意図的に共有する場合は静的な setup token を使ってください。
:::

## アカウントのフィールド

| フィールド | 必須 | 意味 |
| :-- | :-- | :-- |
| `name` | はい | 小文字・数字・ハイフンのみからなる一意のラベル。他のソースフィールドがない場合、名前が一致する shunt ストアファイルを解決します。 |
| `credentials` | 使用可能なソースのいずれか 1 つ | Claude Code の `.credentials.json` 形式のファイル。`~/` は展開されます。shunt は期限が近づくとリフレッシュし、リフレッシュ済みトークンをアトミックに書き戻します。 |
| `token_env` | 使用可能なソースのいずれか 1 つ | setup トークンを含む環境変数。値はそのまま使われ、401 の後にリフレッシュできません。 |
| `uuid` | いいえ | 既存の `metadata.user_id.account_uuid` を書き換えるための、選択されたアカウントの Anthropic UUID。 |
| `threshold` | いいえ | ウィンドウ別の値を持たないすべてのウィンドウに適用される、アカウント単位のソフトなクォータしきい値（`[0.0, 1.0]`）。低い値を設定すると、早めにローテーションで外れるバックアップアカウントになります。 |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | いいえ | ウィンドウ別のソフトしきい値。それぞれ対応するウィンドウで `threshold` より優先されます。 |
| `priority` | いいえ | スティッキーなアカウントが不健全なときの選択優先度。値が小さいほど優先され、デフォルトは `100` です。 |
| `disabled` | いいえ | `true` にすると、設定と管理ダッシュボードには残したまま、アカウントを選択対象から完全に除外します。 |

1 つのアカウントに `credentials` と `token_env` の両方を設定しないでください。

## 選択とプロアクティブなローテーション

- `x-claude-code-session-id` がある場合：安定したハッシュがスティッキーなアカウントを選びます。そのアカウントが利用可能で切り替えしきい値未満なら、shunt はそれを先頭に保ちます。
- ヘッダーがない場合：プロバイダーごとに独自のラウンドロビンカウンターを持ちます。
- `claude_oauth` アカウントプールが処理するすべての上流レスポンスで、shunt は次のヘッダーが存在すれば記録します。
  - `anthropic-ratelimit-unified-5h-utilization`、`anthropic-ratelimit-unified-7d-utilization`、`anthropic-ratelimit-unified-7d_oi-utilization`
  - `anthropic-ratelimit-unified-5h-reset`、`anthropic-ratelimit-unified-7d-reset`、`anthropic-ratelimit-unified-7d_oi-reset`（Unix 秒）
  - `anthropic-ratelimit-unified-status`
- デフォルトの切り替えしきい値は `0.98` です。unified status が `rejected`、共有 5 時間の使用率がそのしきい値に達している、または適用される週次使用率がそのしきい値に達しているとき、アカウントはクォータに近い状態です。しきい値は、アカウント単位（上記の `threshold*` フィールド）またはプール全体（[選択のチューニング](#選択のチューニングserverpool)を参照）で下げられます。
- 5 時間バケットはすべてのモデルに適用されます。Fable のモデル id は、`7d_oi` 週次バケットの使用率があればそれを使い、なければ共有 `7d` にフォールバックします。それ以外のモデルファミリーは共有 `7d` を使います。Sonnet 専用のヘッダーが今のところ存在しないため、Sonnet も `7d` を使います。
- クォータに近い、クールダウン中、または `disabled` なスティッキーアカウントは、プロアクティブにローテーションで外されます。shunt は、しきい値未満で利用可能なアカウントを `priority`（値が小さい順）で優先し、次に適用される週次バケットのリセットが最も早い順で優先して、使わなければ失効するクォータから先に消費します。週次リセットが不明なアカウントが先頭に並びます。その後に利用可能なクォータ接近アカウント、さらに回復が最も早い順のクールダウン中アカウントが続きます。`[server.pool]` が設定されている場合、週次リセットによるタイブレークはバーンレートの余裕（headroom）に置き換わります（下記を参照）。
- shunt がローカルのクォータ状態を理由にフェイルクローズすることはありません。すべてのアカウントがクォータに近い、またはクールダウン中でも、`disabled` でない各アカウントは試行順序に残ります。
- クォータバケットは、リセットのタイムスタンプが過ぎると自動的にクリアされます。成功レスポンスは、選択されたアカウントのクールダウンを解除します。

プールの選択・クールダウン・クォータ状態は、プロセスが生きている限り、設定のホットリロードをまたいで維持されます。プロアクティブなローテーションで上流の制限を回避できない場合も、リアクティブなフェイルオーバーは有効なままです。

## 選択のチューニング（`[server.pool]`）

オプションの `[server.pool]` テーブル（issue #135）は、上記の挙動の上に、ウィンドウ別のソフトしきい値とバーンレートを考慮した順序付けを追加します。テーブルがない場合、選択はこれまでどおり、組み込みの単一しきい値 `0.98` のみを使います。

```toml
[server.pool]
# hard_threshold = 0.98      # (default) backstop; at/above always sorts last
default_threshold = 0.9      # soft default for every window
default_threshold_5h = 0.95  # per-window overrides
default_threshold_fable = 0.85
burn_rate_avoidance = true   # avoid accounts projected to hit a threshold before reset

[[providers.anthropic.accounts]]
name = "primary"
priority = 1                 # preferred whenever the sticky account is unhealthy

[[providers.anthropic.accounts]]
name = "backup"
threshold = 0.5              # backup: rotate out once half its quota is spent

[[providers.anthropic.accounts]]
name = "spare"
disabled = true              # kept configured, never selected
```

- **しきい値の解決。** 各ウィンドウ `X`（`5h`、`7d`、`fable`）について、有効なソフトしきい値は次の順で決まります: アカウントの `threshold_X` → アカウントの `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`（`hard_threshold` を上限としてクランプ）。すべての値は `[0.0, 1.0]` の使用率の割合で、範囲外の値は `shunt check` を失敗させます。
- **バーンレートの余裕（headroom）。** 各ウィンドウの使用率とリセット時刻から（ウィンドウ長は 5 時間と 7 日に固定されています）、shunt は観測された平均ペースでソフトしきい値に達するまでの時間から、ウィンドウがリセットされるまでの時間を差し引いて予測します。余裕がプラスであれば、そのアカウントは現在のペースでもリセットまで持ちこたえます。同じ `priority` の利用可能なアカウントは、余裕が大きい順に並びます。観測されていないウィンドウは無制限の余裕として扱われます。
- **予測的な回避。** `burn_rate_avoidance = true` にすると、予測された余裕がマイナスのアカウントはクォータに近いものとして扱われ、しきい値に達する*前に*ローテーションで外されます。デフォルトはオフです — 余裕による順序付け自体は、この設定に関係なく行われます。
- **全アカウント接近時のガード。** すべてのアカウントがソフトしきい値を超えている（または使い切ると予測される）場合でも、プールが空になることはありません。接近しているアカウントは余裕が大きい順に提供され、`hard_threshold` 以上のアカウントは引き続き最後にソートされ、その後にクールダウン中のアカウントだけが続きます。
- **適用範囲。** クォータ関連のノブは Claude（Anthropic）プールにのみ作用します — Codex バックエンドはクォータヘッダーを送らないため、[Codex プール](/ja/guides/codex-multi-account/)ではこれらは無効ですが、`priority` と `disabled` は引き続き適用されます。
- 管理プールエンドポイント（`GET /admin/pool`）は、各アカウントの `priority`、`disabled` フラグ、そして `[server.pool]` が設定されている場合は現在の余裕予測（秒単位）を報告します。ダッシュボードの状態列は無効化されたアカウントを示します。

## フェイルオーバーのルール

| レスポンス | 挙動 |
| :-- | :-- |
| 2xx | 中継し、健全としてマークします。 |
| 429 かつ `anthropic-ratelimit-unified-5h-status`、`-7d-status`、`-7d_oi-status` のいずれかが `rejected` | クォータ枯渇：数値の `retry-after` でクールダウン（デフォルト 60 秒、1〜3600 秒にクランプ）し、その後ローテーションします。 |
| 単なる 429 | 一時的なスロットル：数値の `retry-after` の分だけ待機（デフォルト 1 秒、上限 300 秒）し、**同じ**アカウントを 1 回リトライして、そのリトライのレスポンスを中継します。 |
| `credentials` での 401 | 強制リフレッシュして同じアカウントを 1 回リトライ。まだ 401 なら 5 分クールダウンしてローテーションします。 |
| `token_env` またはストア管理の setup トークンでの 401 | リフレッシュ不可：5 分クールダウンしてローテーションします。 |
| 5xx またはトランスポート障害 | 30 秒クールダウンしてローテーションします。 |
| その他のステータス | フェイルオーバーせずに中継します。 |

分類はレスポンスボディがストリーミングされる前に行われるため、ストリーム途中の失敗が再送されることはありません。プールがレスポンスを受け取った後に試行を使い切った場合、クライアントは最後の実際の上流ステータスとボディを受け取ります。どの上流レスポンスも受け取る前にすべてのアカウントが失敗した場合、shunt はゲートウェイ自身のエラーを返します。

Anthropic にルーティングされる `POST /v1/messages/count_tokens` リクエストも同じプールを使います。

## リクエストとレスポンスの変更

選択されたアカウントに対し、shunt はクライアントの認証を次で置き換えます。

```http
Authorization: Bearer <selected OAuth token>
anthropic-beta: ...,oauth-2025-04-20
```

受信した `authorization` と `x-api-key` の両方を取り除き、`oauth-2025-04-20` は存在しないときにのみ追加し、その他のエンドツーエンドのヘッダーは保持します。

プール経由のレスポンスはアカウントを識別します。

```http
x-shunt-account: backup
```

共有ゲートウェイでは中立的なアカウント名を使ってください。このヘッダーは、レスポンスを受け取るすべての認可済みクライアントに、設定されたラベルを公開します。プール枯渇後の最後の上流レスポンスの中継では `x-shunt-account` は省略されます。

### `account_uuid`

Claude Code は、文字列値の `metadata.user_id` の中にアカウントメタデータを JSON としてエンコードすることがあります。選択されたアカウントに `uuid` があれば、shunt は**既存の**内側の `account_uuid` をその値で置き換えます。メタデータが存在しない、不正である、`account_uuid` を欠く、または選択されたアカウントに UUID がない場合は、ボディに手を付けません。欠けているメタデータを注入することはありません。

## セキュリティ上の制約

`claude_oauth` は次の場合にのみ受け入れられます。

- プロバイダーが `kind = "anthropic"` である。
- `base_url` が HTTPS を使っている。
- ホストが `anthropic.com`、または `api.anthropic.com` のようなそのサブドメインである。

これらの起動時チェックは、OAuth ベアラーがオリジン外へ、あるいは平文で送られるのを防ぎます。HTTPS とホストのチェックは**ループバックホストでは緩和**されます（`localhost`、`127.0.0.1`、`[::1]` など）。ループバックの `base_url` は平文 HTTP と任意のホストを使えるため、ローカルのデバッグプロキシやモックがトラフィックを受け取れます — ベアラーがオペレーターのマシンから出ることはありません。非ループバックのホストには常に HTTPS + `anthropic.com` が求められます。共有デプロイでは、`claude_oauth` がゲートウェイ所有の認証情報を消費するため、[`[server.auth]`](/ja/guides/shared-gateway/#インバウンドのクライアントトークン) も設定してください。クライアントは、すでに送っている `ANTHROPIC_AUTH_TOKEN` で認証されます（クライアントトークンは `x-shunt-token`、`x-api-key` と並んで `Authorization: Bearer` でも受け付けられます）— プール専用ゲートウェイなら `ANTHROPIC_CUSTOM_HEADERS` の行は不要です。

## 残っているフォローアップ

- **ストーム制御（storm-control）:** 切り替え直後のアカウントの並行度を徐々に上げる処理は今後のフォローアップであり、未実装です。

実装の挙動は [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) と、出荷されている Claude Code バイナリを参考にしています。shunt は teamclaude へのランタイム依存を持ちません。
