# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

[English](README.md) · [한국어](README.ko.md) · **日本語** · [简体中文](README.zh-CN.md)

> Claude Code を任意のモデルへ shunt（分岐）する。

`shunt` は仕様準拠の [Claude Code LLM ゲートウェイ](https://code.claude.com/docs/en/llm-gateway-protocol)です。透過的なプロキシとして、**マッピングしたモデル**についてのみ、推論を**推論レイヤー**で別の LLM プロバイダーへ振り分けます。リクエストの `model` id に基づいてルーティングし、それ以外はすべて変更なしで Anthropic へパススルーします（これが「shunt」であり、フォールバック先は `server.default_provider` で設定可能です）。

この名前が仕組みそのものを表しています。電気回路や鉄道の *shunt*（分岐器）が、選んだ一部の流れを並行した経路へ振り分けるのと同じように、ここではマッピングされたモデルの推論を別のプロバイダーへ振り分けつつ、Claude Code のツールやスキルはそのまま保たれます。

OpenAI、ChatGPT/Codex、xAI、Grok、Cursor、Kimi Code、Zhipu、MiniMax 中国版、Gemini、Antigravity、Anthropic パススルーが標準搭載されており、その多くはすでに契約済みのサブスクリプションをそのまま再利用します。Anthropic Messages 互換のバックエンドであれば設定テーブルを 1 つ書くだけで、コード変更なしに追加できます。[プロバイダー](#プロバイダー)を参照してください。

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

上の `cargo install` は管理ダッシュボードなしでビルドされます。ダッシュボードのバンドルには Node.js 22.12+ が必要で、埋め込むのは `--features ui` だけですが、Homebrew とリリースバイナリはすでにこのフラグを有効にしてビルドしています。管理 JSON API を含むそれ以外はどちらでも同じです。ソースからの手順は [インストール](https://shunt.dev/getting-started/installation/) にあります。

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

### 標準搭載

以下のプロバイダーはデフォルトでシードされているため、独自の `[providers.*]` テーブルなしに `provider = "<name>"` だけでルーティングできます。**ただし `[[upstreams]]` を宣言していない場合に限ります。** 順序付きの `[[upstreams]]` はプロバイダーマップ全体を置き換えるため、その形式ではプリセットを含め、ルーティング先のプロバイダーをすべてそこに宣言する必要があります。

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

順序付きの `[[upstreams]]` エントリーはこれに加えて `kimi`、`kimi-code`、`zhipu`、`minimax-cn` のプリセットも受け付け、各バックエンドの `kind`、`base_url`、デフォルト認証を補完します。

プロバイダーごとのセットアップ、モデル id、注意点は[プロバイダー](https://shunt.dev/ja/guides/providers/)にまとまっています。xAI の OAuth ティア制限（[xAI / Grok](https://shunt.dev/ja/guides/xai/)）、Cursor のエージェントモードのプレフィックス（[Cursor](https://shunt.dev/ja/providers/cursor/)）、Antigravity の 2 つのトランスポートと `kind = "antigravity"` の移行（[Antigravity](https://shunt.dev/ja/providers/antigravity/)）もそこにあります。

> [!WARNING]
> `antigravity-cli` は非推奨であり、**任意コード実行**です。ローカルの `agy` バイナリを `--dangerously-skip-permissions` 付きでエージェントモードで、shunt を実行しているユーザーの権限で起動します。`sandbox` 設定は有効のままにし、バインドはループバックに保ってください。これらを一切必要としない上記の `antigravity` プロバイダーを推奨します。[非推奨のトランスポート](https://shunt.dev/ja/guides/providers/#非推奨の-antigravity_cli-転送)を参照してください。

### あらゆる Anthropic 互換バックエンド

テーブルを 1 つ書くだけで、コード変更は不要です。

| プロバイダー | `base_url` | モデル ID の例 |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| Kimi Code（サブスクリプション、OAuth） | `https://api.kimi.com/coding` | サブスクリプションが提供する ID を使用 |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| Zhipu（GLM 中国版） | `https://open.bigmodel.cn/api/anthropic` | `glm-5.3`, `glm-5.3-flash` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) を参照 |
| MiniMax 中国版 | `https://api.minimax.cn/anthropic` | `MiniMax-M3` |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"
```

上の表の行はほとんどが `auth = "api_key"` を使います。**Kimi Code** だけが例外です。従量課金の Moonshot API とは別のサブスクリプション課金サービスで、ホストが異なり、API キーではなく OAuth を使います。組み込みの `kimi-code` プリセットがあります。このプリセットは順序付きの `[[upstreams]]` エントリー内でのみ解決されるため（シードされたプロバイダーマップには含まれません）、そこで宣言したうえでログインしてください。[Kimi Code](https://shunt.dev/ja/providers/kimi/#kimi-codeoauth-サブスクリプション)を参照してください。

### サブスクリプションの再利用

OpenAI の Thibault Sottiaux は、他のコーディングハーネスを通じて Codex を実行することを公に歓迎しています。

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([出典](https://x.com/thsottiaux/status/2075830097488249060))

彼は[その後の投稿](https://x.com/thsottiaux/status/2076119366647894371)で、Claude Code（「あなたのオレンジ色のカニ」）を GPT-5.6 Sol に向ける方法を自ら解説しています。これはまさに `shunt` が行う推論レイヤーの切り替えであり、別途アプリは不要です。

とはいえ、非公式なクライアントから ChatGPT/Codex や SuperGrok のサブスクリプション（あるいは Kimi、Cursor などの他のバックエンド）を再利用するかどうかは、あなた自身の判断です。公の歓迎は、将来のポリシーやアカウントに対する措置がないことを保証するものではありません。ご利用は自己責任でお願いします。

**Antigravity は、規約がこれを明記している例外です。** Google の [Antigravity 利用規約](https://antigravity.google/terms)は、「サードパーティのソフトウェア、ツール、サービスを使ってサービスにアクセスすること（例：OpenClaw を Antigravity OAuth と組み合わせて使うこと）は本契約の違反」であり、そのような違反は「Antigravity および／または Gemini CLI アカウントの停止または解約の根拠となり得る」と述べています。shunt の `antigravity` プロバイダーはまさにそれ — Antigravity OAuth を使うサードパーティのソフトウェア — なので、このプロバイダーへのルーティングはその条項にそのまま該当します。`shunt login antigravity` を実行する前に、この点を踏まえて判断してください。

## オプション機能

行に明記がない限り**デフォルトで無効**です。該当する設定テーブルがなければ、ルートは登録されず、バックグラウンド処理も開始されません。

| 機能 | 有効化 | ドキュメント |
| :-- | :-- | :-- |
| Anthropic マルチアカウントプーリング — スティッキーセッション、クォータを考慮したローテーション、予測的回避 | アカウント 2 つ以上の `auth = "claude_oauth"`（`[server.pool]` は任意のチューニング） | [ガイド](https://shunt.dev/ja/guides/anthropic-multi-account/) |
| Codex マルチアカウントプーリング — `x-codex-*` ウィンドウの追跡、スロースタートのランプ、再プローブ | アカウント 2 つ以上の `auth = "chatgpt_oauth"`（`[server.pool]` は任意のチューニング） | [ガイド](https://shunt.dev/ja/guides/codex-multi-account/) |
| 受信 Codex エンドポイント — **Codex CLI** 自体を shunt に向けて同じプールに載せ、モデル単位のルーティングも選択可能 | `[server.codex_endpoint]` | [ガイド](https://shunt.dev/ja/guides/inbound-codex-endpoint/) |
| Claude アプリ向けゲートウェイログイン — OAuth デバイスフロー、managed settings、ユーザー単位のポリシー | `public_url`、32 バイト以上の JWT シークレット、静的ユーザーまたは `[server.gateway.oidc]` を備えた `[server.gateway]` | [ガイド](https://shunt.dev/ja/guides/gateway-login/) |
| ゲートウェイテレメトリの受信 — 管理対象クライアントの OTLP をそのままリレー | 構成済みの `[server.gateway]` と、`forward_to` が空でない `[server.gateway.telemetry]` | [リファレンス](https://shunt.dev/ja/reference/configuration/#servergatewaytelemetryオプション) |
| 管理 Web 画面 — アカウントと使用量のダッシュボード、ブラウザーからのプロビジョニング | `[server.admin]` に管理者資格情報（`tokens_env`、`tokens_file`、または `write_keys` エントリ）を自分で書くか、**または** `shunt dashboard setup` がテーブルの作成とトークンの発行をまとめて行います。ただしテーブルの作成とトークンの発行が起きるのは `[server.admin]` が存在しないときだけで、すでにある場合は既存の資格情報をそのままにし、欠けている `[server.oauth_usage]` だけを追加します。ダッシュボード本体は `--features ui` ビルドだけが埋め込むバンドルから配信されます — ビルド済みリリースバイナリと Homebrew formula には含まれ、素の `cargo build`/`cargo install` には含まれません | [ガイド](https://shunt.dev/ja/guides/admin-remote-provisioning/) |
| 支出上限 Admin API — 組織単位・ユーザー単位の上限（ステージ 1 は保存のみで、まだ適用しません） | `[server.admin]`（管理者資格情報が必須: `tokens_env`、`tokens_file`、または `write_keys`/`read_keys` エントリ — read 階層は GET のみ処理） + `[server.spend]` | [リファレンス](https://shunt.dev/ja/reference/configuration/#serverspendオプション) |
| クライアント向け使用量エンドポイント — `GET /usage` がサニタイズ・集計されたプールの余裕を返す | `[server.auth]`（`tokens_env` にクライアントトークンが必須、既定値 `SHUNT_CLIENT_TOKENS`） + `[server.usage]` | [リファレンス](https://shunt.dev/ja/reference/configuration/#serverusageオプション) |
| Claude Code CLI ネイティブ使用量バー — `GET /api/oauth/usage` を提供 | `[server.oauth_usage]`。ループバック以外の bind では `[server.auth]`（`tokens_env` にクライアントトークンが必須、既定値 `SHUNT_CLIENT_TOKENS`）または `[server.gateway]` も必要 | [リファレンス（英語）](https://shunt.dev/reference/configuration/#serveroauth_usage-optional) |
| アップストリームのステータスポーリング — Statuspage の指標をメトリクスとして、また `--features ui` ビルドではダッシュボードにも表示 | `[[server.status.sources]]` を 1 つ以上含む `[server.status]` | [リファレンス（英語）](https://shunt.dev/reference/configuration/#serverstatus-optional) |
| 上限付きのアップストリームリトライ — **デフォルトで有効**、保守的で、ストリーム途中では決してリトライしません | `[providers.<name>.retry]` | [リファレンス（英語）](https://shunt.dev/reference/configuration/#providersnameretry) |
| 共有デプロイの制限 — **デフォルトで有効**（同時 1024、ボディ 32 MiB、TTFB 120 秒、デバイスフローのレートリミット）。CIDR・ヘッダー・URL 制限はオプトイン | `[server] max_concurrent_requests`、`[server.access_control]`、`[server.limits]`、`[server.timeouts]`、`[server.rate_limits]` | [ガイド](https://shunt.dev/ja/guides/shared-gateway/) |
| シークレット参照 — 任意の文字列値で `${VAR}` または `${file:/abs/path}` を使え、ホットリロードごとに再解決（`[sentry]`・`[otel]` を除く。起動時に一度だけ構築されるため再起動が必要） | 設定内の任意の文字列（**常に有効**） | [リファレンス](https://shunt.dev/ja/reference/configuration/) |
| OpenTelemetry のメトリクスとトレース | `endpoint` が空でない `[otel]` | [ガイド](https://shunt.dev/ja/guides/opentelemetry/) |

## ドキュメント

ユーザー向けドキュメントはすべて **[shunt.dev](https://shunt.dev)** にあります。

- [クイックスタート](https://shunt.dev/getting-started/quickstart/) · [なぜ shunt なのか？](https://shunt.dev/getting-started/why-shunt/) · [プロバイダー](https://shunt.dev/guides/providers/) · [設定](https://shunt.dev/guides/configuration/) · [トラブルシューティング](https://shunt.dev/reference/troubleshooting/)
- **エージェント向け:** すべてのページに Markdown の双子版があります（任意の URL に `.md` を付けるか、ページの *Copy Markdown* / *Open in AI* ボタンを使用）。またサイトは [llms.txt spec](https://llmstxt.org/) に従って [`/llms.txt`](https://shunt.dev/llms.txt)、[`/llms-small.txt`](https://shunt.dev/llms-small.txt)、[`/llms-full.txt`](https://shunt.dev/llms-full.txt) を公開しています。

コントリビューター向けの設計ノートとマイルストーン仕様は [`docs/`](docs/) にあります。まずは [`docs/implementation-plan.md`](docs/implementation-plan.md) から読んでください。

## なぜ

Claude Code はすべてのターンを Anthropic API へ送信します。`shunt` はその前段に（`ANTHROPIC_BASE_URL` を介して）位置し、マッピングしたモデルについてのみ、推論を別のプロバイダー（OpenAI、Codex/ChatGPT、…）へ振り分けます。ルーティングが HTTP/推論レイヤーで行われる — 別の CLI へタスクを引き渡すのではない — ため、セッションは Claude Code のハーネス内で走り続けます。同じツールループ、同じプリロード済みスキル、同じバンドルスクリプトのパス解決です。外部化されるのはトークン生成だけです。

代替アプローチ（`subagent_type` を Codex CLI のような別ランタイムへ引き渡す方式）と対比してください。そちらはスタックのより上層で切り替えるため、ペルソナとプリロード済みスキルが失われます。

### エージェント単位ではなくモデル単位 — そしてグローバルな一括切り替えでもない

選択性は**各リクエストの `model` id** によって駆動されます。Claude Code はこれをコンテキストごとに選べるようにすでにしています。メインセッション向けの `/model` ピッカー、サブエージェント定義の `model:` フロントマター、すべてのサブエージェント向けの `CLAUDE_CODE_SUBAGENT_MODEL`、あるいはピッカーにカスタムエントリを追加する `ANTHROPIC_CUSTOM_MODEL_OPTION` です。つまり「このエージェント／このセッションだけ振り分ける」は Claude Code 側で決まり、shunt は受け取ったモデル id を尊重するだけです。エージェントごとのシステムプロンプトの脆いフィンガープリンティングは不要です。グローバルなモデル一括切り替えプロキシとは異なり、メインセッションは Claude のまま残しつつ、あなたが指名したモデルだけを振り分けられます。

## Claude Code 統合（公式サーフェス）

Claude Code は `ANTHROPIC_BASE_URL` の背後に**ファーストクラスのゲートウェイ契約**を公開しています。`shunt` は、これまでの Claude Code プロキシが頼ってきた「サブエージェントのシステムプロンプトをハッシュする」という脆いヒューリスティックではなく、この契約を実装します。

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — エンドポイント、転送すべきヘッダー・ボディフィールドと消費すべきフィールド、機能のパススルー、アトリビューションを定めた API 契約です。稼働中のゲートウェイは `GET /protocol` で機械可読な仕様を提供します。Claude Code はクライアントバージョンと会話のフィンガープリントをシステムプロンプトの先頭に付加しますが、それを抑制するかは `CLAUDE_CODE_ATTRIBUTION_HEADER=0` による開発者の判断であるため、shunt はそのアトリビューションブロックをそのまま転送します。
- [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code は起動時に `GET /v1/models?limit=1000` を照会し（`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` でオプトイン）、返されたモデルを `/model` ピッカーに追加します。shunt はキュレーションされた `[[models]]` エントリーに加え、`auto_include_builtin_models` が `true` の間は呼び出し元のライブカタログを返します。この取得は `server.default_provider` が Anthropic 種別のときのみ行われ、そうでない場合・認証情報がない場合・取得に失敗した場合は組み込みスナップショットにフォールバックします。**制約:** `id` が `claude`/`anthropic` で始まらないエントリーは無視されるため、Claude 系以外のモデルはエイリアスを作るか手動で追加する必要があります。[モデルディスカバリー](https://shunt.dev/ja/guides/model-discovery/)を参照してください。
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` は組み込みエイリアスを置き換えずに、ゲートウェイ経由のエントリーを `/model` ピッカーへ追加します。ID は検証を通らないため、ゲートウェイが受け付ける文字列なら何でも使えます。上記のディスカバリー制約があるため、これが **Claude 系以外のモデルを選ぶ主な方法**です（例: `gpt-5.6-sol`）。
- **ツール検索**（`ENABLE_TOOL_SEARCH`） — Claude Code は MCP/LSP のツールスキーマを遅延させ、必要になったときに開示してコンテキストを回収します。shunt は Anthropic のファーストパーティホストではないため、自分でオプトインしない限りこの機能は**無効**のままです。オプトイン後に遅延が維持されるかは設定だけでなくアップストリームが決めます。`claude*` と `anthropic/*` の id はプロトコルをバイト単位で維持し、それ以外の id はホストが拒否するため `defer_loading` マーカーが除去され、Responses 経路には独自の 3 状態の `tool_search` 設定があります。[ツール検索](https://shunt.dev/ja/guides/codex/#ツール検索)を参照してください。

**設計原則:** スペックに準拠した Anthropic-Messages ゲートウェイであること（`/v1/messages`、`/v1/models`、正しいヘッダー・アトリビューションのパススルー）、リクエストの `model` id でルーティングすること、マッピングされたモデルについて Anthropic Messages ⇄ OpenAI Responses API を変換すること。Claude Code のプロンプトが変わるたびに壊れるプロンプト形状のヒューリスティックは使いません。

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