# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

[English](README.md) · [한국어](README.ko.md) · **日本語** · [简体中文](README.zh-CN.md)

> Claude Code を任意のモデルへ shunt（分岐）する。

`shunt` は仕様準拠の [Claude Code LLM ゲートウェイ](https://code.claude.com/docs/en/llm-gateway-protocol)です。透過的なプロキシとして、**マッピングしたモデル**についてのみ、推論を**推論レイヤー**で別の LLM プロバイダーへ振り分けます。リクエストの `model` id に基づいてルーティングし、それ以外はすべて変更なしで Anthropic へパススルーします（これが「shunt」であり、フォールバック先は `server.default_provider` で設定可能です）。

この名前が仕組みそのものを表しています。電気回路や鉄道の *shunt*（分岐器）が、選んだ一部の流れを並行した経路へ振り分けるのと同じように、ここではマッピングされたモデルの推論を別のプロバイダーへ振り分けつつ、Claude Code のツールやスキルはそのまま保たれます。

**OpenAI**、**ChatGPT/Codex**（`codex login` でサブスクリプションを再利用）、**xAI**（API キー）、**Grok**（`shunt login xai` で SuperGrok / X Premium+ サブスクリプションを再利用）、**Cursor**（`shunt login cursor` でサブスクリプションを再利用）、**Kimi Code**（`shunt login kimi` でサブスクリプションを再利用）、**Gemini / Google Code Assist**（`~/.gemini/oauth_creds.json` でサブスクリプションを再利用。shunt は有効なアクセストークンをそのまま使用し、自前でのリフレッシュには `SHUNT_GOOGLE_CLIENT_ID` と `SHUNT_GOOGLE_CLIENT_SECRET` が必要です）、そして **Anthropic** パススルーが標準搭載されており、さらに Anthropic Messages 互換のバックエンド（Kimi、DeepSeek、GLM、MiniMax、OpenRouter、Vercel AI Gateway、…）は TOML テーブルまたは YAML マッピングを 1 つ書くだけで、コード変更なしに追加できます。

> [!NOTE]
> `shunt` は活発に開発中の 1.0 未満（pre-1.0）ソフトウェアです。[SemVer](https://semver.org/lang/ja/#spec) の慣例に従い、`0.x` リリースには設定キー・CLI・動作に対する破壊的変更（breaking change）が含まれる場合があります。アップグレード前に[リリースノート](https://github.com/pleaseai/shunt/releases)を確認してください。

## インストール

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo — ソースリポジトリから直接インストール
cargo install --git https://github.com/pleaseai/shunt
```

新しいバージョンは Homebrew と、各 [GitHub リリース](https://github.com/pleaseai/shunt/releases)に添付されるビルド済みバイナリ（macOS/Linux、arm64/x64）で配布されます。crates.io パッケージは、最後に公開されたバージョンで更新を停止します。ビルド済みバイナリおよびソースからのインストール手順は [インストール](https://shunt.dev/getting-started/installation/) を参照してください。

### サービスとして実行する (macOS/Homebrew)

```bash
brew services start shunt
```

ログは `$(brew --prefix)/var/log/shunt.log` に出力されます。`brew services stop` は `SIGTERM` を送信し、
shunt は処理中のリクエストを完了させてから終了します。Unix では、シャットダウンの開始時に Antigravity の
エージェントターンが終了させられるため、その分離されたプロセスグループがドレインを引き延ばすことはできません。
その後に設定ファイルを編集しても再起動は不要です —
自動的に[ホットリロード](docs/config-reload.md)されます。詳細: [サービスとして実行](docs/running.md#run-as-a-background-service-homebrew)。

## クイックスタート

```toml
# shunt.toml — route a gpt-* id to your ChatGPT subscription
# [[routes]] is legacy for exact ids; prefer [models.upstream_model].
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"        # reuses `codex login`; use `openai` for OPENAI_API_KEY
```

```bash
codex login                                        # provider credential
shunt run                                           # -> listening on 127.0.0.1:3001

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
claude                                              # /model -> pick gpt-5.6-sol
```

マッピングされていないモデル（あなたのすべての `claude-*` id）は、これまでとまったく同じように動作します。shunt はあなた自身の認証情報を使って Anthropic へ転送します。詳しい手順は [クイックスタート](https://shunt.dev/getting-started/quickstart/) を参照してください。

### スターター設定

`shunt init` は、既存のディレクトリにコメント付きの `shunt.toml` を作成します。デフォルトの passthrough starter をそのまま使うか、マッピングされていないモデルの fallback を変えずに順序付き upstream preset を scaffold できます。

```bash
shunt init
shunt init --upstream codex --upstream kimi
```

### エージェントネイティブなセットアップ blueprint

`shunt add` は、コーディングエージェント向けの組み込み Markdown 実装ガイドを取得します。`shunt add upstream` で利用可能な upstream blueprint を一覧表示するか、そのままエージェントへパイプできます。

```bash
shunt add upstream kimi --print | claude
shunt add upstream https://provider.example/docs --print | claude
```

このコマンドはオフラインかつ読み取り専用です。ガイドを出力するだけで、ファイルの編集、インストール、ネットワークアクセスは行いません。まったく新しい provider protocol のサポートに貢献する場合は `shunt add provider <absolute-url>` を使用してください。

共有デプロイでは、処理中の受信リクエスト数を `[server] max_concurrent_requests`（デフォルト `1024`）で
制限します。超過したリクエストは `503` と `Retry-After: 1` で即座に切り捨てられ、ストリーミングリクエストは
レスポンスボディが終わるかクライアントが切断するまでスロットを保持します。値を `0` にすると制限は無効に
なります。`/` と `/health` は liveness プローブのために常に利用可能なままです。共有ゲートウェイではさらに、
CIDR の許可／拒否ルール、リクエスト・ヘッダー・URL のサイズ上限、ストリーミングボディには上限を設けない
アップストリームのレスポンスヘッダータイムアウト、独立したデバイスフローのレート制限を、
`[server.access_control]`、`[server.limits]`、`[server.timeouts]`、`[server.rate_limits]` で設定できます。
Anthropic Messages と受信 Codex Responses のリクエストボディはデフォルトで 32 MiB の上限です。より大きな
ファイルや画像のリクエストには `max_request_bytes` を引き上げてください。その他のゲートウェイ、管理、
テレメトリ、分析のルートは、それぞれのエンドポイント固有のボディ上限を保ちます。
[設定リファレンス](https://shunt.dev/reference/configuration/#server)を参照してください。

シークレットに類する値を、設定ファイルにリテラルとして書く必要はありません。どの文字列も代わりに
`${VAR}`（環境変数。例 `"Bearer ${TOKEN}"`）または `${file:/abs/path}`（そのフィールドの値全体として、
ファイルの内容を trim したもの）として書けます。これらは設定を読み込むたびに新しく解決され、
[ホットリロード](docs/config-reload.md)も対象に含まれるため、`${file:}` に基づくシークレットは shunt を
再起動せずにローテーションできます。新しい値が実際に反映されるかどうかは、そのフィールド自身のリロード
動作に従います。`[sentry]` と `[otel]` は起動時に一度だけ構築されるため、この 2 つのセクションの
シークレットをローテーションする場合は依然として再起動が必要です。
[設定のシークレット参照](docs/config-secrets.md)を参照してください。

## プロバイダー

プロバイダーは、順序付き `[[upstreams]]` エントリまたはレガシーな `[providers.<name>]` TOML テーブルです（YAML では、それぞれ対応する sequence または mapping のエントリ）。2 種類のアダプターでほとんどの上流をカバーします。`kind = "anthropic"`（上流が Anthropic Messages を話す場合。別のキーを付けてパススルー可能）と `kind = "responses"`（上流が OpenAI Responses API を話す場合。shunt が Anthropic Messages ⇄ Responses をストリーミング込みで変換）です。3 つ目のネイティブな種類である `kind = "cursor"` は、Cursor の ConnectRPC/protobuf AgentService をブリッジし、Cursor サブスクリプションを同じ Anthropic Messages インターフェース経由で利用できるようにします。

順序付きアップストリームにより、プロバイダー間のフェイルオーバーが可能になります。宣言順が試行順となり、モデルの `upstream_model` マップが参加するエントリを選択して、公開 id を各バックエンドの id にマッピングします。

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic" # preset: kind, base_url, and default auth
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "codex-fallback"
provider = "codex" # defaults to chatgpt_oauth

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
codex-fallback = "gpt-5.6-sol"
```

このチェーンは `anthropic-primary`、次に `codex-fallback` を試行します。`auth` は mode 文字列またはマップを受け付け、`claude_oauth` と `chatgpt_oauth` のマップは `account = "name"` または `accounts = [...]` で認証情報の範囲を絞れます。レガシーな `[providers.<name>]` は引き続きサポートされ、名前順の暗黙的アップストリームになります。設定ファイル内で両方の形式を宣言しないでください。`[[upstreams]]` と `[providers.*]` の混在は設定エラーです。preset、失敗クラス、移行の詳細は [設定リファレンス](https://shunt.dev/reference/configuration/) を参照してください。

**標準搭載:**

| 名前 | Kind | 認証 | バックエンド |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough または Claude OAuth アカウントプール | `api.anthropic.com` — デフォルトでは呼び出し元自身の認証情報を転送。`auth = "claude_oauth"` でプールされたサブスクリプション認証情報を利用可能 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — `~/.codex/auth.json`（`codex login`）を再利用 |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` — 開発者向け API、トークン単位の課金 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` — Grok CLI プロキシ。`~/.shunt/xai-auth.json` を再利用（SuperGrok / X Premium+ サブスクリプションで `shunt login xai`） |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` — `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用 |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` — Google Code Assist バックエンド、`~/.gemini/oauth_creds.json` を再利用 |
| `antigravity` | `antigravity` | Antigravity OAuth | `daily-cloudcode-pa.googleapis.com` — HTTP 経由の Google Antigravity バックエンド、`~/.shunt/antigravity-auth.json`（`shunt login antigravity`）を使用 |
| `antigravity-cli` | `antigravity_cli` | なし（ローカル CLI） | **非推奨。** ローカルの `agy` バイナリ — サブプロセス経由で同じバックエンドを利用。上記の `antigravity` に置き換えられました |

xAI はサブスクリプションのティアによって OAuth アクセスを制限する場合があります。`grok` が 403 を返す場合は、代わりに `xai` API キープロバイダーを使用してください。詳細は [`docs/m6-xai-provider.md`](docs/m6-xai-provider.md) を参照してください。

**Antigravity には 2 つのトランスポートがあります。** `antigravity` プロバイダーは HTTP で Google Antigravity バックエンドと通信し、`shunt login antigravity` で認証します。これは Antigravity 独自の OAuth クライアントとスコープを使う Google の認可コードフローであるため、Gemini CLI のログインを再利用することはできません。`gemini` プロバイダーと同じ Code Assist プロトコルを話し、現在は Gemini 系の Antigravity モデルを提供します。Antigravity が併せて提供する Claude モデルには、まだ実装されていないリクエストの書き換えが必要です（#368）。ログインとスコープ、プロジェクトの検出、モデルスラッグ、thinking、アダプターが引き回す内容など、完全なセットアップは [プロバイダー → Antigravity](https://shunt.dev/ja/providers/antigravity/) を参照してください。

**`antigravity-cli` は非推奨であり、任意コード実行に相当します。** ローカルの `agy` バイナリをエージェントモードで実行します。CLI が自身のツールを使って作業し、shunt はその進捗を Anthropic SSE としてストリーミングします。そのため `tool_use` ブロックを返すことはできず、実際にツール呼び出しを要求するリクエスト、つまり空でない `tools` 配列、または `any`/`tool` の `tool_choice` は、暗黙にテキストで回答するのではなく `400` で拒否されます。`tool_choice: none` は `tools` が指定されていても例外であり、ツールなしの `auto` もツール呼び出しを必須にしないため例外です。非対話実行では権限プロンプトに応答できないため、`agy` は `--dangerously-skip-permissions` 付きで実行されます。したがって **このプロバイダーは shunt を実行しているユーザー権限での任意コード実行として扱ってください**。範囲を制限する設定は 2 つあります。`sandbox`（デフォルトは `true`）は `--sandbox` を渡して読み書きをワークスペース内に制限し、実際にエージェントを封じ込めます。`workspace_roots` はエージェントが*開始できる*場所だけを決め、リクエストのシステムプロンプトに含まれる `Working directory:` パス（クライアントが制御するテキスト）を、指定したルート配下の正規化済みパスに制限します。サンドボックスを有効のままにし、ループバックだけにバインドしてください。この問題がない `antigravity` プロバイダーを推奨します。詳細は[プロバイダーガイド](https://shunt.dev/ja/guides/providers/)を参照してください。

**旧 `antigravity` からの移行。** かつて `kind = "antigravity"` はローカル CLI を意味していました。その意味のまま残っている設定は、黙って別の対象に振り替えられるのではなく名前によって拒否され、認証情報のないまま `antigravity` プロバイダーにルーティングしていると起動を拒否します。起動が成功したように見えたまま、その裏でトランスポートと認証情報とエグレスが切り替わるほうが、失敗するより悪いからです。`shunt check` も同じガードを実行するため、CI やデプロイスクリプトは認証情報が保存されていない `antigravity` へのルートを、起動時ではなくロールアウト前に検出できます。このチェックは存在確認のみです。認証情報を開くわけではないため、空または古い認証情報でも通過し、後からリクエスト経路で失敗します。`shunt login antigravity` を実行するか、ルートを `antigravity-cli` に向けてください。

**Anthropic マルチアカウント。** `auth = "claude_oauth"` の Anthropic プロバイダーは、Claude Code の認証情報ファイルまたは setup-token 環境変数から明示的なアカウントを読み込むか、`shunt login claude --name <name>` で作成した非公開ストア管理アカウントを使用できます。Claude のログインには 3 つのモードがあります。`--mode oauth` は shunt 独自の更新可能な OAuth フローを実行し（TTY のデフォルト）、`--mode import` は現在の Claude Code ログインをコピーし、`--mode setup-token` は 1 年間有効な推論専用トークンを作成します（`--long-lived` は引き続き非推奨の別名です）。OAuth はまず自動の `127.0.0.1` コールバックを使い、非表示の手動貼り付けにフォールバックします。コールバックを省略するには `--manual` を使用してください。OAuth のスコープ動作は宣言形式によって異なります。レガシーな `[providers.*].accounts = []` はアカウントストアを走査しますが、順序付きの `[[upstreams]]` でストア全体を走査するには `account` と `accounts` の両方を省略する必要があり、明示的な `accounts = []` は拒否されます。shunt は健全な `x-claude-code-session-id` セッションを固定し、それ以外ではプロバイダーごとのラウンドロビンを使います。また、可能な場合はモデルを考慮した 5 時間／週次クォータの状態に基づき、クォータに近づいた固定アカウントを上限に達する前に積極的に切り替えます。選択はオプションの `[server.pool]` テーブルで調整できます（issue #135）。アカウントごとの上書きを備えたウィンドウ単位のソフトなクォータしきい値（低いしきい値はバックアップアカウントを示します）、バーンレートを考慮した順序付け、リセット前にウィンドウを使い切ると予測されるアカウントを任意で避ける機能、アカウントごとの `priority` と `disabled` の設定があります。`usage_refresh_seconds` を有効にすると、インポートした（更新可能な）アカウントについて Anthropic OAuth 使用量 API をポーリングし、外部で発生した使用量を突合できます。ポーリングはデフォルトで無効です。`state_path` を設定するとアカウントごとのクォータをディスクに保存し、再起動後も空のプールではなく最後に観測した使用率からウォームスタートします（最善努力のキャッシュであり、クォータはこれとは無関係にアップストリームから再導出されます）。リセット時刻のないウィンドウも、それ自体の最後の観測時刻によって存続期間が制限されるため、永久に残ることはありません。永続化はデフォルトで無効です。クォータによって拒否された 429、401、5xx へのリアクティブな対応は、引き続きフェイルオーバーの最低限の安全策です。ストーム制御は後続の課題です。[使い方](https://shunt.dev/guides/anthropic-multi-account/)、[設定リファレンス](https://shunt.dev/reference/configuration/)、[M8 動作仕様](docs/m8-anthropic-multi-account.md)を参照してください。

**Codex マルチアカウント。** `chatgpt_oauth` プロバイダー（組み込みの `codex` プロバイダー、またはその認証モードを使う任意の `responses` プロバイダー）も、複数の ChatGPT アカウントを同様にプールできます。アカウントは `codex login` の認証情報を `shunt login codex --name <name>` でインポートするか、[管理 Web 画面](https://shunt.dev/guides/admin-remote-provisioning/)で ChatGPT OAuth を実行するか、明示的な `credentials`／`token_env` アカウントエントリで用意します。OAuth のスコープ動作は宣言形式によって異なります。レガシーな `[providers.*].accounts = []` はアカウントストアを走査しますが、順序付きの `[[upstreams]]` でストア全体を走査するには `account` と `accounts` の両方を省略する必要があり、明示的な `accounts = []` は拒否されます。shunt はバックエンドの `x-codex-*` 5 時間／7 日間の使用量ウィンドウを記録し、Claude プールと同じ **クォータ対応の積極的な選択**に反映します。クォータに近い固定アカウントは 429 を返す前に譲り、アップストリームのリセットヘッダーが空でも観測時刻だけでマークの存続期間を制限できるため、`[server.pool]` のしきい値とバーンレート順序付けが適用されます。クールダウンベースのリアクティブなフェイルオーバー（429、401、5xx、認証情報の解決失敗）は安全策として残ります。オプションの `[server.pool] ramp_initial_concurrency` スロースタートゲートは、フェイルオーバー後に新しく選択されたアカウントへ同時実行中のリクエストが殺到することを防ぎます。`[server.pool]` を設定すると、`reprobe_seconds`（デフォルト 900 秒、`0` で無効）は陳腐化した近接クォータのアカウントを間隔ごとに 1 つ選択順の先頭へ昇格させて予約します。admission または認証情報の解決に失敗した場合は予約を取り消し、最初の実際の HTTP 送信時に再プローブ時刻をコミットします。鮮度を管理する観測スタンプは 5h、共有 7d、Fable 7d_oi、aggregate status の 4 つです。WebSocket を使わない outbound Responses 選択とオプションの inbound Codex HTTP エンドポイントは再プローブを維持します。WebSocket 転送を有効にしたプロバイダーの outbound Responses プールは、ストリーム内の rate-limit イベントを安全にローテーションできないため予約を作らず再プローブを抑止し、そのプロバイダーの `shunt.pool.reprobes` は inbound プローブだけを数えます。正の `usage_refresh_seconds` は、非公開で非公式な `wham/usage` エンドポイントをポーリングし、imported かつ refreshable な `chatgpt_oauth` アカウントの帯域外の消費を、Anthropic プールが独自の usage API で行うのと同じように突き合わせます。ポーリングはデフォルトで無効で、エンドポイントは予告なく変更される可能性があります。ポーラーはその imported かつ refreshable なアカウントだけを早期に復旧させます。報告されたウィンドウでは、リセットメタデータはヘッダー由来のままです。未来のヘッダーリセットは保持し、経過した保存済みリセットは新しい使用率を書き込む前にクリアします。wham の `reset_at` は実際のリセットメタデータとして採用しません。ポーラーがない場合や対象外のアカウントでは、除外された outbound マークは観測時刻に基づくウィンドウ寿命の上限で期限切れになるまで残ります。[使い方](https://shunt.dev/guides/codex-multi-account/)と [M10 動作仕様](docs/m10-codex-multi-account.md)を参照してください。

**受信 Codex エンドポイント。** shunt は逆方向にも動作します。オプトインの `[server.codex_endpoint]` テーブルは、生の OpenAI Responses パススルー（`/responses`、`/v1/responses`、`/backend-api/codex/responses`）を登録するため、**Codex CLI 自体**が `base_url` を shunt に向けて、同じ ChatGPT/Codex OAuth アカウントプールでロードバランスされるようにできます — これは上記の Anthropic へ変換するパスではなく、バイト単位のリレーです。また、CLI が使う 2 つの分析用パスをプライバシーを保つ破棄シンクとして受け付け、サニタイズされたイベント名のカウンターだけを記録し、ペイロードをアップストリームへ転送することはありません。デフォルトでは無効で、このテーブルがなければこれらのルートは一切登録されません。[使い方](https://shunt.dev/ja/guides/inbound-codex-endpoint/)と [M11 動作仕様](docs/m11-inbound-codex-endpoint.md)を参照してください。

**上限付きのアップストリームリトライ。** プロバイダーの単一認証情報パスにおける一時的なアップストリーム障害は、クライアントへ 1 バイトも届く前に（ストリームの途中では決して行わず）、指数バックオフとランダムなジッターを伴って再試行されます。接続レベルのトランスポートエラー（接続のリセット／拒否、タイムアウト）は常に再試行します — 解決前には何も受理されていないためです。一時的なレスポンス*ステータス*（`429`/`502`/`503`/`504`/`529`、Anthropic の "Overloaded"）が再試行されるのは冪等な Cursor パスのみです。冪等でない Anthropic Messages と単一認証情報の Responses POST では、レスポンスが返ってきた時点でアップストリームが課金対象の生成をすでに受理している可能性があるため、そのまま呼び出し元へ返します（issue #126）。その他の `4xx` は決して再試行しません。`Retry-After`（delta-seconds と HTTP-date の両形式）を尊重し、`count_tokens` では行わず、プロバイダーごとに `[providers.<name>.retry]` で設定できます（デフォルトで有効、保守的な設定。無効にするには `max_retries = 0`）。`claude_oauth`／`chatgpt_oauth` のアカウントプールは、代わりに独自のアカウントローテーションによるフェイルオーバーを使用します。[設定リファレンス](https://shunt.dev/reference/configuration/#providersnameretry)を参照してください。

**オプトインの Claude アプリ向けゲートウェイログインとポリシー。** `[server.gateway]` を設定すると、共有の静的トークンを 1 つ配布する代わりに、管理下の Claude Code クライアントが OAuth デバイスフロー（`forceLoginMethod: "gateway"` + `forceLoginGatewayUrl`）でサインインできるようになります。ブラウザーでの承認には、環境変数に基づく静的ユーザーか、`[server.gateway.oidc]` 経由で許可リストに登録した OIDC プロバイダー（Google など）を使えます。両方を同時に提供することもできます。shunt は OAuth discovery、ブラウザー承認、device/refresh グラント、HS256 のアクセス JWT、ローテーションする不透明な refresh token、そして `ETag` キャッシュ・テレメトリ環境変数のプッシュ・`availableModels` の強制を備えたユーザーごとの `GET /managed/settings` を提供します。発行される bearer は `/v1/models` と、選択されたプロバイダーがサーバー側の認証情報を注入する推論ルートを保護し、passthrough のプロバイダーは開いたままです。`[server.auth]` と組み合わせられます。この機能はデフォルトで無効です。refresh セッションはデフォルトで再起動をまたいで保持されます（issue #194）。`state_path`（デフォルトは `~/.shunt/gateway-sessions.json`）は refresh token を SHA-256 ハッシュとして、アトミックに書き込まれる所有者専用（Unix では 0600）のファイルに保存し、起動時に復元します。そのためユーザーはブラウザーフローをやり直すことなく、静かにリフレッシュを続けられます。メモリ専用のセッションにするには `state_path = ""` を設定します。この場合は再起動で refresh セッションが無効になります。発行済みのアクセス JWT は有効期限まで使えますが、その後はユーザーが再度サインインする必要があります。device グラントは常にメモリ専用です。クライアントは Claude Code の中からではなく、ターミナルからサインインすることもできます。`shunt gateway login <url>` は同じデバイスフローを実行してセッションをローカル（`~/.shunt/gateway/session.json`、所有者専用）に保存し、`shunt gateway token` は `apiKeyHelper` として使うアクセストークンを出力し、`shunt gateway claude` はその設定をひとつのプロセスだけに適用して Claude Code を起動します — `~/.claude/settings.json` を変更せず、クライアントをサインイン済みのゲートウェイセッションに入れることもないため、そのゲートに伴う機能上のトレードオフを負いません（あらゆる `apiKeyHelper` が引っかかる通常の認証情報タイプのゲートは引き続き適用されます）。`shunt login <provider>` と `shunt token` は変更されておらず、引き続き shunt をアップストリームに対して認証します。[セットアップガイド](https://shunt.dev/guides/gateway-login/)、[設定リファレンス](https://shunt.dev/reference/configuration/#servergateway-optional)、[M-A ログインノート](docs/gateway-login.md)、[M-B managed-settings ノート](docs/gateway-managed-settings.md)、[M-C テレメトリノート](docs/gateway-telemetry.md) を参照してください。

**オプトインの支出上限 Admin API。** `[server.spend]` を設定すると、組織単位およびユーザー単位の上限を扱う認証付き CRUD ルートが `/v1/organizations/spend_limits` 配下に登録されます。ステージ 1 では上限と監査証跡を保存しますが、推論トラフィックに対する上限の強制はまだ行いません。これらのルートは `[server.admin]` の認証情報で認証します — `[server.spend]` は鍵素材を持たないトップレベルのポリシーセクションであるため、支出上限を有効にしても `[server.gateway]` のログインは必要ありません — また状態はデフォルトでアトミックな非公開 JSON ファイルへ永続化されます。セットアップ、API の挙動、先送りされた機能は [ステージ 1 ガイド](docs/gateway-spend-limits.md)を参照してください。

**オプトインのゲートウェイテレメトリ取り込み。** 空でない `[server.gateway.telemetry].forward_to` リストは 2 つのことを行います。テレメトリの有効化フラグと 5 つの `OTEL_*` 環境変数値を managed settings 経由でプッシュし（管理下のすべてのクライアントのエクスポーターを shunt に向けます）、そのうえで、クライアントが送信してくる受信 OTLP/HTTP ルート — `POST /v1/metrics`、`POST /v1/logs`、`POST /v1/traces` — のそのままのリレーを有効にします。これらは `[server.gateway]` の他の機能と共に登録され、`GET /managed/settings` と同じゲートウェイ bearer で保護されます。ペイロードは**そのまま**リレーされます — リクエストのバイト列そのままで、受信時の `content-type` と `content-encoding` を引き継ぎ、クライアントの `Authorization` ヘッダーは決して転送しません — そのため `application/x-protobuf` と `application/json` のどちらのエクスポーターでも動作し、Claude Code のクライアント側アトリビューション属性も保たれます。宛先ごとにシグナル単位でオプトインします。`metrics` はデフォルトで有効ですが、`logs` と `traces` は無効です。Claude Code のログレコードとスパンにはコマンドライン、プロンプト、ファイルパスが含まれ得るためです。オプトインした宛先のないシグナルは受け付けたうえで破棄され、リレーは切り離して実行されるため、コレクターが遅い場合や停止している場合でもクライアントには常に即座に `200` が返ります。デフォルトでは無効で、宛先がなければルートは受け付けて破棄します。[Claude Code のモニタリング](https://code.claude.com/docs/en/monitoring-usage)、[設定リファレンス](https://shunt.dev/reference/configuration/#servergatewaytelemetry-optional)、[M-C テレメトリノート](docs/gateway-telemetry.md)を参照してください。

**オプトインの管理 Web 画面。** `[server.admin]` を設定すると、管理者認証付きの**アカウントと使用量**ビューが追加されます。このビューは、対応するホストのログインを自動的に観測し、識別可能なマスク済み ID とプロバイダーネイティブなクォータウィンドウを表示します。対象は Claude Code（認証情報ファイルまたは macOS キーチェーン）、Codex CLI（レスポンス由来の `x-codex-*` ウィンドウ）、Gemini CLI（Code Assist の全モデルバケット）、Kimi Code（週次および 5 時間の上限）、Grok CLI（クレジット／プロダクトの使用量）、Cursor.app（請求サイクル、Auto + Composer、名前付きモデルの使用量）です。観測は読み取り専用です。shunt がそれらのソース認証情報を更新・複製・書き込みすることは一切ありません。Cursor.app の状態は読み取り専用で開き、メモリ上のファーストパーティ Web セッションを導出するためだけに使われます。Claude の使用量は 60 秒キャッシュされ、他のプロバイダーのリーダーはダッシュボードのデータが要求されたときに実行されます。管理プールへのプロビジョニングは、Claude と Codex のアカウント向けに折りたたまれた詳細セクションで引き続き利用できます。別途保存されるそれらのアカウントこそ、shunt がロードバランシングのために所有し更新する認証情報です。オペレーターの介入なしにはどのリトライでも回復しない管理対象アカウントは、`cooling` ではなく **Needs re-login** と表示されるため、恒久的に死んだログインをクォータ一時停止と区別できます — 5 分ごとに永久にリトライし続ける代わりに、です。`imported` の行には **Refresh** ボタン（`POST /admin/accounts/claude/{name}/refresh`）もあり、そのアカウントの refresh グラントをその場で実行してログインがまだ生きているかを報告します。管理プールの `/admin/pool` ビューでは、アカウントごとに任意の `plan`（サブスクリプションのティア）も表示します。可能な場合はファイル由来の値を使い、Claude アカウントについては上限付きでキャッシュされたライブ補完を行います — これは欠けている plan を埋めることも、乗数の情報を欠くファイル由来の値をより正確な値へ精緻化することもできます。plan を判別できなかった場合、そのキーは単に存在しません。この画面はデフォルトでは無効で（テーブルがなければ `/admin*` ルートは一切登録されません）、`[server.auth]` とは別の認証情報を使用します。ワンステップで有効にするには `shunt dashboard setup` を実行してください。管理トークンを `~/.shunt/admin-token`（所有者専用）に生成し、`[server.admin].tokens_file` 経由で紐付けるため起動時の環境変数に秘密情報が載らず、`[server.oauth_usage]` を有効にし、ダッシュボードの URL を表示します — その後 shunt を再起動してください。管理トークンは `SHUNT_ADMIN_TOKENS` から与えることもできます。アクセス階層は 2 つあります。`[[server.admin.write_keys]]` エントリと `tokens_env`／`tokens_file` の `name:token` ペアはフルアクセスを持ち、`[[server.admin.read_keys]]` エントリは — 管理画面と支出上限 API の両方で — すべての `GET` を通し、ブラウザーのログインフォームを含むあらゆる変更操作では拒否されます。配列形式のキーは `${VAR}` / `${file:...}` または `SHUNT_*` の上書きで与える必要があり、設定ファイル内のリテラルは読み込み時に拒否されます。[使い方](https://shunt.dev/ja/guides/admin-remote-provisioning/)と [M9 設計ノート](docs/m9-admin-surface.md)を参照してください。

**オプトインのクライアント向け使用量エンドポイント。** `[server.usage]` を設定すると読み取り専用の `GET /usage` が登録され、共有アカウントプールのクォータを**サニタイズして集計した**ビュー — ウィンドウごとの残りの余裕、リセット時刻、粗い `ok`／`degraded`／`exhausted` ステータス — として返します。これにより、管理画面を使わずに非管理クライアントがスロットリングを予期できます。認証は `/v1/messages` と同じ `[server.auth]` のクライアントトークンで行い（そのテーブルが必要です）、アカウント名、件数、優先度、`disabled` フラグ、しきい値を公開することはありません。アカウントごとの完全な詳細は管理者専用の `/admin/pool` に留まります。ウィンドウが `null` になるのは、無効化されていないアカウントがどれもそのウィンドウを報告していない場合だけです。Codex の `x-codex-*` レスポンスヘッダーとオプションの `wham/usage` ポーリングが、観測済みの 5 時間と共有週次ウィンドウを埋めます。観測されていないウィンドウだけが `null` です。Codex には Fable スコープ（`7d_oi`）のシグナルがありませんが、混在プロバイダーのプールでは別のプロバイダーが集約 Fable 値を提供できます。デフォルトでは無効で、テーブルがなければルートは登録されません。[設定リファレンス](https://shunt.dev/reference/configuration/#serverusage-optional)と [M12 設計ノート](docs/m12-client-usage-endpoint.md)を参照してください。

**オプトインの Claude Code CLI ネイティブ使用量バー。** `[server.oauth_usage]` を設定すると `GET /api/oauth/usage` が登録されます。これは Claude Code CLI 自身の `Current session`／`Current week` 使用量バーが取得する、まさにそのパスです。そのため `ANTHROPIC_BASE_URL` で CLI を shunt に向けたとき、変更を加えていない CLI の UI が 404 になって空のバーを表示する代わりに、Claude のみを対象とした、優先ティアを考慮した最悪ケースの実際のプール値を描画できます。**前提条件、一部のみ検証済み:** `claude setup-token` や共有ゲートウェイのクライアントトークンから `ANTHROPIC_AUTH_TOKEN` を設定した場合 — shunt で文書化されている他の 2 つの認証情報構成 — には取得**されない**ことを確認済みです。一方、完全な対話的 `claude login`（サブスクリプション）セッションでは取得*される*という点は、CLI バイナリの静的解析と UI 上の状況証拠からの推定であり、直接観測したものではありません（調査環境では実際のサブスクリプションログインを安全にスクリプト化できませんでした）。これはすべての構成で「そのまま動く」わけではありません。前提条件の根拠と、その唯一の未検証部分については [M14 設計ノート](docs/m14-oauth-usage-endpoint.md)を参照してください。認証はバインドのトポロジーによって決まります（ループバックでは認証なし。ループバック以外のバインドでは有効なクライアントトークンまたはゲートウェイ JWT が必要で、単なるヘッダーの存在ではなく `/v1/messages` とまったく同じ基準で判定され、その場合は `[server.auth]` または `[server.gateway]` の設定も必要になります）。デフォルトでは無効で、テーブルがなければルートは登録されません。[設定リファレンス](https://shunt.dev/reference/configuration/#serveroauth_usage-optional)を参照してください。

**オプトインのアップストリームステータスポーリング。** `[server.status]` に 1 つ以上の Statuspage `summary.json` ソースを設定すると、各プロバイダーの公開ステータスフィードを一定間隔（デフォルトは 5 分）でポーリングし、最後に観測したインジケーターを管理ダッシュボードの「Upstream status」ストリップと `shunt.upstream.status` ゲージメトリクスとして表示します。これは厳密に観測専用です。ここで読み取った内容がルーティング、フェイルオーバー、プールやクールダウンの判断に影響することは一切ありません。取得に失敗した、2xx 以外のレスポンスを返した、あるいは shunt が認識できないインジケーターを報告したソースは、黙って正常とみなされるのではなく `unknown` として保存・報告されます。デフォルトでは無効で、テーブルがなければバックグラウンドのポーリングは開始されません。[設定リファレンス](https://shunt.dev/reference/configuration/#serverstatus-optional)と[設計ノート](docs/upstream-status.md)を参照してください。

OpenAI の Thibault Sottiaux は、他のコーディングハーネスを通じて Codex を実行することを公に歓迎しています。

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([出典](https://x.com/thsottiaux/status/2075830097488249060))

彼は[その後の投稿](https://x.com/thsottiaux/status/2076119366647894371)で、Claude Code（「あなたのオレンジ色のカニ」）を GPT-5.6 Sol に向ける方法を自ら解説しています。これはまさに `shunt` が行う推論レイヤーの切り替えであり、別途アプリは不要です。

とはいえ、非公式なクライアントから ChatGPT/Codex や SuperGrok のサブスクリプション（あるいは Kimi、Cursor などの他のバックエンド）を再利用するかどうかは、あなた自身の判断です。公の歓迎は、将来のポリシーやアカウントに対する措置がないことを保証するものではありません。ご利用は自己責任でお願いします。

**Antigravity は、規約がこれを明記している例外です。** Google の [Antigravity 利用規約](https://antigravity.google/terms)は、「サードパーティのソフトウェア、ツール、サービスを使ってサービスにアクセスすること（例：OpenClaw を Antigravity OAuth と組み合わせて使うこと）は本契約の違反」であり、そのような違反は「Antigravity および／または Gemini CLI アカウントの停止または解約の根拠となり得る」と述べています。shunt の `antigravity` プロバイダーはまさにそれ — Antigravity OAuth を使うサードパーティのソフトウェア — なので、このプロバイダーへのルーティングはその条項にそのまま該当します。`shunt login antigravity` を実行する前に、この点を踏まえて判断してください。

**Cursor** も同じ仕組みです。一度ログインすれば、`cursor:*` のモデル id をルーティングできます。

```bash
shunt login cursor                                  # OAuth -> ~/.shunt/cursor-auth.json
```

```toml
# shunt.toml — route a cursor:<id> to your Cursor subscription
[[routes]]
model = "cursor:default"                             # "default" is the wire id for Auto; paid plans can use named ids
provider = "cursor"
```

`cursor:` / `cursor-agent:` / `cursor-plan:` / `cursor-ask:` プレフィックスが Cursor のエージェントモード（Agent / Plan / Ask）を選択し、サフィックスが Cursor の**ワイヤー**モデル id です（Auto は `auto` ではなく `default`）。アダプターはアシスタントのテキストと reasoning をストリーミングし、クライアントのツールをネイティブな Cursor MCP ツール呼び出しとしてブリッジし、インライン画像を転送します（issue #170）。詳細は [プロバイダー → Cursor](https://shunt.dev/ja/providers/cursor/) を参照してください。

**あらゆる Anthropic 互換バックエンド**が、テーブルを 1 つ書くだけで使えます。コード変更は不要です。

| プロバイダー | `base_url` | モデル ID の例 |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| Kimi Code（サブスクリプション、OAuth） | `https://api.kimi.com/coding` | サブスクリプションが提供する ID を使用 |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) を参照 |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

上の表の行はほとんどが `auth = "api_key"` を使います。例外は **Kimi Code** で、すぐ上の行にある従量課金の Moonshot API とは別の、サブスクリプション課金の Kimi サービスです — ホストが異なり、API キーではなく OAuth を使います。専用の組み込み `kimi-code` プリセット（`kind = "anthropic"`、`base_url = "https://api.kimi.com/coding"`、`auth = "kimi_oauth"`）が用意されているため、`provider = "kimi-code"` とログイン済みアカウントさえあれば、手動の `[providers.*]`/`[[upstreams]]` テーブルは不要です:

```bash
shunt login kimi --name <account-name>                # RFC 8628 デバイスフロー -> ~/.shunt/accounts/kimi/<account-name>.json
```

```toml
# shunt.toml — Kimi Code サブスクリプションへルーティング
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# [[upstreams]] を宣言すると組み込みの provider セットが置き換わるため、末尾に
# anthropic passthrough を残します。これがないと `shunt check` がデフォルトの
# server.default_provider を解決できずに失敗します。`shunt init` が追加するものと同じです。
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

`kimi_oauth` は `claude_oauth`/`chatgpt_oauth` と同様にプール可能です — `account` の代わりに `accounts = [...]` を使うと、複数の保存済み Kimi アカウントに負荷を分散できます。admin/`/usage` のプール画面を含む詳しい手順は [Kimi → Kimi Code (OAuth subscription)](https://shunt.dev/providers/kimi/#kimi-code-oauth-subscription) を参照してください。デバイスフロー・トークンストア・検証の内部詳細は [M15 設計ノート](docs/m15-kimi-oauth.md) を参照してください。

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

全リストとプロバイダーごとの注意点は [プロバイダー](https://shunt.dev/guides/providers/) を参照してください。

## ドキュメント

すべては **[shunt.dev](https://shunt.dev)** にあります。

- [クイックスタート](https://shunt.dev/getting-started/quickstart/) · [なぜ shunt なのか？](https://shunt.dev/getting-started/why-shunt/) · [プロバイダー](https://shunt.dev/guides/providers/) · [設定](https://shunt.dev/guides/configuration/) · [トラブルシューティング](https://shunt.dev/reference/troubleshooting/)
- **エージェント向け:** すべてのページに Markdown の双子版があります（任意の URL に `.md` を付けるか、ページの *Copy Markdown* / *Open in AI* ボタンを使用）。またサイトは [llms.txt spec](https://llmstxt.org/) に従って [`/llms.txt`](https://shunt.dev/llms.txt)、[`/llms-small.txt`](https://shunt.dev/llms-small.txt)、[`/llms-full.txt`](https://shunt.dev/llms-full.txt) を公開しています。

設計ノートとマイルストーン仕様は [`docs/`](docs/) にあります（まずは [`docs/implementation-plan.md`](docs/implementation-plan.md) から）。Claude Code を ChatGPT/Codex サブスクリプションへルーティングするには、[Codex 設定リファレンス](docs/codex-configuration.md)を参照してください。

### 可観測性メトリクス

| 系列 | 種別 | 属性 | 意味 |
| :-- | :-- | :-- | :-- |
| `shunt.failover` | Counter | `provider`, `state` | 順序付きアップストリームのフェイルオーバー遷移: `attempted`、`advanced`、`exhausted`。 |

メトリクスの完全な表とエクスポート設定は [OpenTelemetry ガイド](https://shunt.dev/ja/guides/opentelemetry/)を参照してください。

## なぜ

Claude Code はすべてのターンを Anthropic API へ送信します。`shunt` はその前段に（`ANTHROPIC_BASE_URL` を介して）位置し、マッピングしたモデルについてのみ、推論を別のプロバイダー（OpenAI、Codex/ChatGPT、…）へ振り分けます。ルーティングが HTTP/推論レイヤーで行われる — 別の CLI へタスクを引き渡すのではない — ため、セッションは Claude Code のハーネス内で走り続けます。同じツールループ、同じプリロード済みスキル、同じバンドルスクリプトのパス解決です。外部化されるのはトークン生成だけです。

代替アプローチ（`subagent_type` を Codex CLI のような別ランタイムへ引き渡す方式）と対比してください。そちらはスタックのより上層で切り替えるため、ペルソナとプリロード済みスキルが失われます。

### エージェント単位ではなくモデル単位 — そしてグローバルな一括切り替えでもない

選択性は**各リクエストの `model` id** によって駆動されます。Claude Code はこれをコンテキストごとに選べるようにすでにしています。メインセッション向けの `/model` ピッカー、サブエージェント定義の `model:` フロントマター、すべてのサブエージェント向けの `CLAUDE_CODE_SUBAGENT_MODEL`、あるいはピッカーにカスタムエントリを追加する `ANTHROPIC_CUSTOM_MODEL_OPTION` です。つまり「このエージェント／このセッションだけ振り分ける」は Claude Code 側で決まり、shunt は受け取ったモデル id を尊重するだけです。エージェントごとのシステムプロンプトの脆いフィンガープリンティングは不要です。グローバルなモデル一括切り替えプロキシとは異なり、メインセッションは Claude のまま残しつつ、あなたが指名したモデルだけを振り分けられます。

## Claude Code 統合（公式サーフェス）

Claude Code は `ANTHROPIC_BASE_URL` の背後に**ファーストクラスのゲートウェイ契約**を公開しています。`shunt` は、初期の Claude Code プロキシが頼っていた脆い「サブエージェントのシステムプロンプトをハッシュ化する」ヒューリスティックではなく、この契約を実装します。

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — API 契約。エンドポイント、転送すべき／消費すべきヘッダー・ボディフィールド、機能のパススルー、アトリビューションです。稼働中のゲートウェイは `GET /protocol` で機械可読の仕様を提供します。
  - [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code は起動時に `GET /v1/models?limit=1000` を照会し（`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` でオプトイン）、返されたモデルを `/model` ピッカーに追加します。デフォルトでは `auto_include_builtin_models = true` により、キュレーションされた `[[models]]` エントリの後に自動検出されたモデルが id で重複排除されたうえで追加されます。厳密にキュレーションしたリストにするには `false` を設定してください。これらのモデルは、`server.default_provider` が Anthropic 系の場合にそのプロバイダーへの実際の `GET /v1/models` から取得され、そのプロバイダーの認証モードを使います。`passthrough` は呼び出し元の認証情報を転送するため、呼び出し元ごとに自身が利用可能なリストが見えます。`api_key` は設定されたキーを使います。`claude_oauth` は、推論と同じ実効アカウント集合（`account_scope` の順でストアを走査したアカウントを含む）から、解決可能で無効化されていない最初のアカウントを、プール選択・クールダウン・クォータ計上なしで使います。後者の 2 つのモードは、ゲートウェイの認証情報に紐づく共有カタログを公開します。shunt は何もキャッシュせず、デフォルトプロバイダーが Anthropic 系でない場合、認証情報がない場合、または呼び出しが失敗した場合（2 秒の上限）は、組み込みの Claude カタログのスナップショットにフォールバックします。キュレーションされたエントリには `[models.upstream_model]` マップを含めることもでき（順序付き `[[upstreams]]` では複数エントリ、レガシーな providers では 1 エントリ）、これにより公開される id が対応するアップストリーム経由でルーティング可能になり、別途 `[[routes]]` エントリを書かずに各バックエンドの id へ変換されます。**制約:** `id` が `claude`/`anthropic` で始まらないエントリは無視されます。非 Claude モデルはエイリアス化するか手動で追加する必要があります。
  - **システムプロンプトのアトリビューションブロック** — Claude Code はクライアントバージョン + 会話フィンガープリントをシステムプロンプトの先頭に付加します。これは会話のライフタイム中は安定です（v2.1.181+）。`shunt` はこれを変更せず転送します（決して除去しません。それは `CLAUDE_CODE_ATTRIBUTION_HEADER=0` による開発者の判断です）。
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` は、組み込みエイリアスを置き換えずにゲートウェイ経由のエントリを `/model` ピッカーへ追加します。この ID は検証をスキップするため、ゲートウェイが受け入れる任意の文字列が使えます。discovery は `claude`/`anthropic` で始まらない id を無視するため、**これが非 Claude モデル（例 `gpt-5.6-sol`）を選択する主な方法**です。
- **ツール検索**（`ENABLE_TOOL_SEARCH`） — Claude Code は MCP/LSP のツールスキーマを遅延させ、`ToolSearch` ツールを通じて必要になったときに開示します。これにより、モデルが呼び出しもしないツールに費やすはずだったコンテキストを取り戻せます。shunt はファーストパーティの Anthropic ホストではないため、Claude Code は `ENABLE_TOOL_SEARCH=true` でオプトインしない限りこれを**無効**のままにします。Messages パスでは、遅延が維持されるかどうかは設定ではなくアップストリームのモデルによって決まります。`claude*` と `anthropic/*` の id ではプロトコルがバイト単位でそのまま保たれますが、非 Anthropic の id（OpenRouter のステルススラッグ、Kimi、…）では `defer_loading` マーカーと `tool_search_tool_*` エントリが除去されます。これらのホストがそれらを明確に拒否するためです（`400 Deferred custom tools are only supported on Anthropic models...`）。ツール自体は届きますが、完全なスキーマを伴って一括で届くため、それらのモデルではツール検索によるコンテキストの節約はありません。Codex/Responses パスでは、`[providers.<name>]` 配下の `tool_search` は 3 状態の設定です。未設定（デフォルトの「auto」）は、すでに実装が確認されているアップストリーム — ChatGPT/Codex バックエンドと `api.openai.com` — に限って Responses API 自身のネイティブなクライアント実行型 `tool_search` プロトコルへ対応付け、その他の OpenAI 互換エンドポイント（LiteLLM、vLLM、OpenRouter、自前のプロキシ、…）には #43 のテキストシムを使い続けます。`tool_search = true` は、アップストリームのフレーバーとモデルが条件を満たす場合（xAI/Grok 以外、gpt-5.4 以降）にネイティブを強制するため、検証済みのカスタムエンドポイントを任意でオプトインできます。`tool_search = false` は常にシムを強制し、開示された各ツールをキャッシュ済みの `tools` プレフィックスへ追加して、開示のたびにそれを無効化します。[ツール検索](https://shunt.dev/ja/guides/codex/#tool-search)ガイドを参照してください。

**設計原則:** 仕様準拠の Anthropic Messages ゲートウェイ（`/v1/messages`、`/v1/models`、正しいヘッダー／アトリビューションのパススルー）であること、リクエストの `model` id でルーティングすること、そしてマッピングされたモデルについて Anthropic Messages ⇄ OpenAI Responses API を変換すること。Claude Code のプロンプトが変わるたびに壊れるようなプロンプト形状ヒューリスティックは使いません。

## 関連研究 / 先行事例

**Claude Code 特化のルーター & プロキシ**

- [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) — このニッチで最大規模。Claude Code を基盤として使い、リクエストがどのように異なるモデル／プロバイダーへ到達するかを決めます。
- [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) — Claude Code を OpenAI モデルで動かす。
- [fuergaosi233/claude-code-proxy](https://github.com/fuergaosi233/claude-code-proxy) — Claude Code → OpenAI API プロキシ。
- [seifghazi/claude-code-proxy](https://github.com/seifghazi/claude-code-proxy) — 実行中の Claude Code リクエストをキャプチャ／可視化し、オプションで**エージェント単位**の他プロバイダーへのルーティングを行う（`shunt` のサブエージェントルーティングのアイデアを直接触発した）。
- [luohy15/y-router](https://github.com/luohy15/y-router) — Claude Code を OpenRouter で動かせるようにするシンプルなプロキシ。
- [tingxifa/claude_proxy](https://github.com/tingxifa/claude_proxy) — Claude API リクエストを OpenAI 形式（Gemini、Groq、Ollama）へ変換する Cloudflare Workers プロキシ。
- [badlogic/claude-bridge](https://github.com/badlogic/claude-bridge) — Claude Code で任意のモデルプロバイダーを使う。
- [jimmc414/claude_n_codex_api_proxy](https://github.com/jimmc414/claude_n_codex_api_proxy) — クロスランタイムルーター。Anthropic **または** OpenAI の API 呼び出しをローカルの **Claude Code または Codex** CLI へプロキシする（API キーがすべて 9 のときはローカル CLI へ、そうでなければ本物のクラウド API へルーティング）。方向が逆である点に注意 — Claude Code エージェントをクラウドプロバイダーへ*送り出す*のではなく、クラウド API 呼び出しをローカル CLI *へ*ルーティングします。
- [insightflo/chatgpt-codex-proxy](https://github.com/insightflo/chatgpt-codex-proxy) — Claude Code の推論を **ChatGPT Codex バックエンド**から提供する Anthropic 互換の `/v1/messages` プロキシ（API キーの代わりに ChatGPT Plus/Pro サブスクリプションを使用）。`shunt` と同じ推論レイヤーの切り替えで、Claude Code の UI と MCP ツールを保ちつつ Codex/GPT サブスクリプションバックエンドを対象とします。

**汎用 AI ゲートウェイ（隣接インフラ — バックエンド候補）**

- [BerriAI/litellm](https://github.com/BerriAI/litellm) — 100 以上の LLM API を OpenAI 形式で呼び出す SDK + プロキシ/AI ゲートウェイ。コスト追跡、ガードレール、ロードバランシング付き。
- [Portkey-AI/gateway](https://github.com/Portkey-AI/gateway) — 1,600 以上の LLM へルーティングする高速 AI ゲートウェイ。ガードレール統合。
- [maximhq/bifrost](https://github.com/maximhq/bifrost) — 適応的ロードバランシングと 1000 以上のモデルサポートを備えた高性能 AI ゲートウェイ。
- [mazori-ai/modelgate](https://github.com/mazori-ai/modelgate) — オープンソースの LLM ゲートウェイ + MCP サーバー（Go）。RBAC/ポリシー適用、マルチプロバイダー（OpenAI、Anthropic、Gemini、Bedrock、Azure、ローカルの Ollama）、セマンティックなツール検索を備えた MCP ゲートウェイ、セマンティックなレスポンスキャッシュ。

### `shunt` はどう違うのか

上記のほとんどの Claude Code プロキシは、**すべての**トラフィックを 1 つの代替プロバイダーへルーティングします（グローバルなモデル一括切り替え）。`shunt` の焦点は、リクエストの `model` id によって駆動される**選択的でモデル単位**の振り分けです。メインセッションは Claude のまま残し、あなたが指名したモデルだけを他プロバイダーへ shunt する — 交換機／パッチベイのユースケースです。Claude Code はすでにコンテキストごと（メインセッション、サブエージェントの `model:` フロントマター、`CLAUDE_CODE_SUBAGENT_MODEL`）にモデルをバインドできるため、shunt が呼び出し元を一切詮索することなく、その同じ選択性が個々のエージェントにまで届きます。

## コントリビュート

Issue と PR を歓迎します。ビルド／テストコマンドと規約については [`CONTRIBUTING.md`](CONTRIBUTING.md) と [`AGENTS.md`](AGENTS.md) を、脆弱性の報告については [`SECURITY.md`](SECURITY.md) を参照してください。

### コードレビュー

`shunt` へのプルリクエストは 2 つの AI コードレビュアーによってレビューされ、いずれもオープンソースでは無料です。

- [Greptile](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source) — OSS プログラムのもと、非商用の MIT/Apache プロジェクトで無料。
- [cubic](https://cubic.dev/) — 公開リポジトリで無料。

## ライセンス

[Apache License, Version 2.0](LICENSE-APACHE) または [MIT license](LICENSE-MIT) のいずれか、お好きな方の下でライセンスされます。あなたが明示的に別途表明しない限り、Apache-2.0 ライセンスで定義されるとおり、あなたがこのクレートへの包含を意図的に提出したいかなるコントリビューションも、追加の条項や条件なく上記のとおりデュアルライセンスされるものとします。

---

Made with Orca 🐋

- https://github.com/stablyai/orca
- https://www.onorca.dev/
