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

**OpenAI**、**ChatGPT/Codex**（`codex login` でサブスクリプションを再利用）、**xAI**（API キー）、**Grok**（`shunt login xai` で SuperGrok / X Premium+ サブスクリプションを再利用）、**Cursor**（`shunt login cursor` でサブスクリプションを再利用）、**Kimi Code**（`shunt login kimi` でサブスクリプションを再利用）、そして **Anthropic** パススルーが標準搭載されており、さらに Anthropic Messages 互換のバックエンド（Kimi、DeepSeek、GLM、MiniMax、OpenRouter、Vercel AI Gateway、…）は TOML テーブルを 1 つ書くだけで、コード変更なしに追加できます。

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
shunt は処理中のリクエストを完了させてから終了します。その後に設定ファイルを編集しても再起動は不要です —
自動的に[ホットリロード](docs/config-reload.md)されます。詳細: [サービスとして実行](docs/running.md#run-as-a-background-service-homebrew)。

## クイックスタート

```toml
# shunt.toml — route a gpt-* id to your ChatGPT subscription
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

**標準搭載:**

| 名前 | Kind | 認証 | バックエンド |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough | `api.anthropic.com` — 呼び出し元自身の認証情報を転送 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — `~/.codex/auth.json`（`codex login`）を再利用 |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` — 開発者向け API、トークン単位の課金 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` — Grok CLI プロキシ。`~/.shunt/xai-auth.json` を再利用（SuperGrok / X Premium+ サブスクリプションで `shunt login xai`） |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` — `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用 |

xAI はサブスクリプションのティアによって OAuth アクセスを制限する場合があります。`grok` が 403 を返す場合は、代わりに `xai` API キープロバイダーを使用してください。詳細は [`docs/m6-xai-provider.md`](docs/m6-xai-provider.md) を参照してください。

**`antigravity-cli` は非推奨であり、任意コード実行に相当します。** ローカルの `agy` バイナリをエージェントモードで実行します。CLI が自身のツールを使って作業し、shunt はその進捗を Anthropic SSE としてストリーミングします。そのため `tool_use` ブロックを返すことはできず、実際にツール呼び出しを要求するリクエスト、つまり空でない `tools` 配列、または `any`/`tool` の `tool_choice` は、暗黙にテキストで回答するのではなく `400` で拒否されます。`tool_choice: none` は `tools` が指定されていても例外であり、ツールなしの `auto` もツール呼び出しを必須にしないため例外です。非対話実行では権限プロンプトに応答できないため、`agy` は `--dangerously-skip-permissions` 付きで実行されます。したがって、**このプロバイダーは shunt を実行しているユーザー権限での任意コード実行として扱ってください**。範囲を制限する設定は 2 つあります。`sandbox`（デフォルトは `true`）は `--sandbox` を渡して読み書きをワークスペース内に制限し、実際にエージェントを封じ込めます。`workspace_roots` はエージェントが*開始できる*場所だけを決め、リクエストのシステムプロンプトに含まれる `Working directory:` パス（クライアントが制御するテキスト）を、指定したルート配下の正規化済みパスに制限します。サンドボックスを有効のままにし、ループバックだけにバインドしてください。この問題がない `antigravity` プロバイダーを推奨します。詳細は[プロバイダーガイド](https://shunt.dev/guides/providers/)を参照してください。

**オプトインの Claude アプリ向けゲートウェイログインとポリシー。** `[server.gateway]` を設定すると、共有の静的トークンを 1 つ配布する代わりに、管理下の Claude Code クライアントが OAuth デバイスフロー（`forceLoginMethod: "gateway"` + `forceLoginGatewayUrl`）でサインインできるようになります。ブラウザーでの承認には、環境変数に基づく静的ユーザーか、`[server.gateway.oidc]` 経由で許可リストに登録した OIDC プロバイダー（Google など）を使えます。両方を同時に提供することもできます。shunt は OAuth discovery、ブラウザー承認、device/refresh グラント、HS256 のアクセス JWT、ローテーションする不透明な refresh token、そして `ETag` キャッシュ・テレメトリ環境変数のプッシュ・`availableModels` の強制を備えたユーザーごとの `GET /managed/settings` を提供します。発行される bearer は `/v1/models` と、選択されたプロバイダーがサーバー側の認証情報を注入する推論ルートを保護し、passthrough のプロバイダーは開いたままです。`[server.auth]` と組み合わせられます。この機能はデフォルトで無効です。refresh セッションはデフォルトで再起動をまたいで保持されます（issue #194）。`state_path`（デフォルトは `~/.shunt/gateway-sessions.json`）は refresh token を SHA-256 ハッシュとして、アトミックに書き込まれる所有者専用（Unix では 0600）のファイルに保存し、起動時に復元します。そのためユーザーはブラウザーフローをやり直すことなく、静かにリフレッシュを続けられます。メモリ専用のセッションにするには `state_path = ""` を設定します。この場合は再起動で refresh セッションが無効になります。発行済みのアクセス JWT は有効期限まで使えますが、その後はユーザーが再度サインインする必要があります。device グラントは常にメモリ専用です。クライアントは Claude Code の中からではなく、ターミナルからサインインすることもできます。`shunt gateway login <url>` は同じデバイスフローを実行してセッションをローカル（`~/.shunt/gateway/session.json`、所有者専用）に保存し、`shunt gateway token` は `apiKeyHelper` として使うアクセストークンを出力し、`shunt gateway claude` はその設定をひとつのプロセスだけに適用して Claude Code を起動します — `~/.claude/settings.json` を変更せず、クライアントをサインイン済みのゲートウェイセッションに入れることもないため、そのゲートに伴う機能上のトレードオフを負いません（あらゆる `apiKeyHelper` が引っかかる通常の認証情報タイプのゲートは引き続き適用されます）。`shunt login <provider>` と `shunt token` は変更されておらず、引き続き shunt をアップストリームに対して認証します。[セットアップガイド](https://shunt.dev/guides/gateway-login/)、[設定リファレンス](https://shunt.dev/reference/configuration/#servergateway-optional)、[M-A ログインノート](docs/gateway-login.md)、[M-B managed-settings ノート](docs/gateway-managed-settings.md)、[M-C テレメトリノート](docs/gateway-telemetry.md) を参照してください。

OpenAI の Thibault Sottiaux は、他のコーディングハーネスを通じて Codex を実行することを公に歓迎しています。

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([出典](https://x.com/thsottiaux/status/2075830097488249060))

彼は[その後の投稿](https://x.com/thsottiaux/status/2076119366647894371)で、Claude Code（「あなたのオレンジ色のカニ」）を GPT-5.6 Sol に向ける方法を自ら解説しています。これはまさに `shunt` が行う推論レイヤーの切り替えであり、別途アプリは不要です。

とはいえ、非公式なクライアントから ChatGPT/Codex や SuperGrok のサブスクリプション（あるいは Kimi、Cursor などの他のバックエンド）を再利用するかどうかは、あなた自身の判断です。公の歓迎は、将来のポリシーやアカウントに対する措置がないことを保証するものではありません。ご利用は自己責任でお願いします。

**Cursor** も同じ仕組みです。一度ログインすれば、`cursor:*` のモデル id をルーティングできます。

```bash
shunt login cursor                                  # OAuth -> ~/.shunt/cursor-auth.json
```

```toml
# shunt.toml — route a cursor:<id> to your Cursor subscription
[[routes]]
model = "cursor:gpt-5.5"                             # cursor-plan:<id> / cursor-ask:<id> select the agent mode
provider = "cursor"
```

`cursor:` / `cursor-agent:` / `cursor-plan:` / `cursor-ask:` プレフィックスが Cursor のエージェントモードを選択し、サフィックスが Cursor のモデル id です。詳細は [プロバイダー → Cursor](https://shunt.dev/ja/providers/cursor/) を参照してください。

**あらゆる Anthropic 互換バックエンド**が、テーブルを 1 つ書くだけで使えます。コード変更は不要です。

| プロバイダー | `base_url` | モデル ID の例 |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
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
model = "kimi-k2.7-code"
provider = "kimi"
```

全リストとプロバイダーごとの注意点は [プロバイダー](https://shunt.dev/guides/providers/) を参照してください。

## ドキュメント

すべては **[shunt.dev](https://shunt.dev)** にあります。

- [クイックスタート](https://shunt.dev/getting-started/quickstart/) · [なぜ shunt なのか？](https://shunt.dev/getting-started/why-shunt/) · [プロバイダー](https://shunt.dev/guides/providers/) · [設定](https://shunt.dev/guides/configuration/) · [トラブルシューティング](https://shunt.dev/reference/troubleshooting/)
- **エージェント向け:** すべてのページに Markdown の双子版があります（任意の URL に `.md` を付けるか、ページの *Copy Markdown* / *Open in AI* ボタンを使用）。またサイトは [llms.txt spec](https://llmstxt.org/) に従って [`/llms.txt`](https://shunt.dev/llms.txt)、[`/llms-small.txt`](https://shunt.dev/llms-small.txt)、[`/llms-full.txt`](https://shunt.dev/llms-full.txt) を公開しています。

設計ノートとマイルストーン仕様は [`docs/`](docs/) にあります（まずは [`docs/implementation-plan.md`](docs/implementation-plan.md) から）。Claude Code を ChatGPT/Codex サブスクリプションへルーティングするには、[Codex 設定リファレンス](docs/codex-configuration.md)を参照してください。

## なぜ

Claude Code はすべてのターンを Anthropic API へ送信します。`shunt` はその前段に（`ANTHROPIC_BASE_URL` を介して）位置し、マッピングしたモデルについてのみ、推論を別のプロバイダー（OpenAI、Codex/ChatGPT、…）へ振り分けます。ルーティングが HTTP/推論レイヤーで行われる — 別の CLI へタスクを引き渡すのではない — ため、セッションは Claude Code のハーネス内で走り続けます。同じツールループ、同じプリロード済みスキル、同じバンドルスクリプトのパス解決です。外部化されるのはトークン生成だけです。

代替アプローチ（`subagent_type` を Codex CLI のような別ランタイムへ引き渡す方式）と対比してください。そちらはスタックのより上層で切り替えるため、ペルソナとプリロード済みスキルが失われます。

### エージェント単位ではなくモデル単位 — そしてグローバルな一括切り替えでもない

選択性は**各リクエストの `model` id** によって駆動されます。Claude Code はこれをコンテキストごとに選べるようにすでにしています。メインセッション向けの `/model` ピッカー、サブエージェント定義の `model:` フロントマター、すべてのサブエージェント向けの `CLAUDE_CODE_SUBAGENT_MODEL`、あるいはピッカーにカスタムエントリを追加する `ANTHROPIC_CUSTOM_MODEL_OPTION` です。つまり「このエージェント／このセッションだけ振り分ける」は Claude Code 側で決まり、shunt は受け取ったモデル id を尊重するだけです。エージェントごとのシステムプロンプトの脆いフィンガープリンティングは不要です。グローバルなモデル一括切り替えプロキシとは異なり、メインセッションは Claude のまま残しつつ、あなたが指名したモデルだけを振り分けられます。

## Claude Code 統合（公式サーフェス）

Claude Code は `ANTHROPIC_BASE_URL` の背後に**ファーストクラスのゲートウェイ契約**を公開しています。`shunt` は、初期の Claude Code プロキシが頼っていた脆い「サブエージェントのシステムプロンプトをハッシュ化する」ヒューリスティックではなく、この契約を実装します。

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — API 契約。エンドポイント、転送すべき／消費すべきヘッダー・ボディフィールド、機能のパススルー、アトリビューションです。稼働中のゲートウェイは `GET /protocol` で機械可読の仕様を提供します。
  - [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code は起動時に `GET /v1/models?limit=1000` を照会し（`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` でオプトイン）、返されたモデルを `/model` ピッカーに追加します。**制約:** `id` が `claude`/`anthropic` で始まらないエントリは無視されます。非 Claude モデルはエイリアス化するか手動で追加する必要があります。
  - **システムプロンプトのアトリビューションブロック** — Claude Code はクライアントバージョン + 会話フィンガープリントをシステムプロンプトの先頭に付加します。これは会話のライフタイム中は安定です（v2.1.181+）。`shunt` はこれを変更せず転送します（決して除去しません。それは `CLAUDE_CODE_ATTRIBUTION_HEADER=0` による開発者の判断です）。
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` は、組み込みエイリアスを置き換えずにゲートウェイ経由のエントリを `/model` ピッカーへ追加します。この ID は検証をスキップするため、ゲートウェイが受け入れる任意の文字列が使えます。discovery は `claude`/`anthropic` で始まらない id を無視するため、**これが非 Claude モデル（例 `gpt-5.6-sol`）を選択する主な方法**です。

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
