---
title: インバウンド Codex エンドポイント
description: OpenAI の Codex CLI 自身を shunt へ向け、ChatGPT/Codex OAuth アカウントプールで負荷分散する。
---

このサイトの他のガイドはすべて **Claude Code** を別のバックエンドへルーティングします。shunt は逆方向にも動けます。オプトインの生の OpenAI Responses パススルーによって、**Codex CLI** が自身の `base_url` を shunt へ向け、ChatGPT/Codex OAuth アカウントプールで負荷分散されるようにするものです。これはオプトインです。`[server.codex_endpoint]` がない場合、それらのルートはいずれも登録されず、shunt のデフォルトの HTTP サーフェスは変わりません。

これは [Codex マルチアカウント](/ja/guides/codex-multi-account/)と同じアカウントプールの上に構築されます — 選択、クールダウン、リフレッシュはそのまま共有されます。正確なフェイルオーバーの表とリロードのセマンティクスを含む完全な仕様は、[M11 の挙動仕様](https://github.com/pleaseai/shunt/blob/main/docs/m11-inbound-codex-endpoint.md)を参照してください。

エンドツーエンドのセットアップ — エンドポイントの有効化、Codex CLI を shunt へ向ける、クライアント認証、アカウントのプロビジョニング、entitle されたモデルの選択 — については [Codex CLI の接続](/ja/guides/connect-codex-cli/)に従ってください。このページは*エンドポイントが何をするか*に焦点を当てており、あちらのガイドが*どう接続するか*のチェックリストです。

## エンドポイントを有効にする

```toml
[server.codex_endpoint]   # all keys optional; default shown
provider = "codex"        # must be a chatgpt_oauth provider
```

```bash
shunt check
shunt run
```

起動時の検証は、未知の `provider` や `auth = "chatgpt_oauth"` を使わないプロバイダーを拒否します — このエンドポイントはオペレーターの Codex ベアラーを注入するため、`chatgpt_oauth` プロバイダーだけが要件を満たします。すべてのキーとデフォルトは[設定リファレンス](/ja/reference/configuration/)を、登録されるルートは [HTTP エンドポイント](/ja/reference/endpoints/)を参照してください。

このオプトインにより、Codex CLI のモデル検出も解析可能になります。`GET /models` と `GET /backend-api/codex/models` は有効なフォールバック `{"models":[]}` を返します。共有の `GET /v1/models` パスでは、`client_version` クエリフィールドが Anthropic 風のヘッダーより優先され、Codex 形式を選択します。このフィールドがなければ、既存の Anthropic 検出レスポンスは変わりません。これらのリクエストは通常のモデル検出認証ゲートを通り、shunt は不完全な Codex モデル行を生成しません。

## クライアント analytics のシンク

Codex CLI は base URL へプロダクト analytics も POST します。shunt は CLI が生成しうる両方のパスを受け付けます。

- `POST /backend-api/codex/analytics-events/events`
- `POST /codex/analytics-events/events`

これらのルートは Responses ルートと同じ `[server.auth]` ポリシーを使いますが、テレメトリーを上流へ転送することは決してありません。プールされたアカウントを 1 つ選ぶと、クライアントのイベントがそのアカウントへ誤って帰属されてしまうためです。認証後は、不正な形式・読み取り不能・サイズ超過のボディも含めて、常に `200 {}` を返します。

payload とイベントのプロパティは、ログにも記録されずエクスポートもされません。shunt が記録するのは、オプトインの `shunt.codex_client_events` カウンターの `event` 属性としてサニタイズ済みの `event_type` だけです。名前に使えるのは小文字の ASCII 英字、数字、`.`、`_`、`-` で、最大 64 バイトです。不正な名前は `other` に、認識できないバッチは `unparsed` になります。Sentry も OpenTelemetry のメトリクスも有効でなければ、これは純粋な破棄シンクです。

## Codex CLI を shunt へ向ける

Codex CLI は、使用する base URL が何であれ常に `/responses` を末尾に付けるため、`~/.codex/config.toml` はどちらの形状でも動作します。

**ChatGPT バックエンドの base URL をミラーする:**

```toml
chatgpt_base_url = "http://127.0.0.1:3001/backend-api/codex"
```

**またはカスタムモデルプロバイダー**（トップレベルの `model_provider` でそれを選択する必要があります。さもないと CLI は組み込みのプロバイダーを使い続けます）:

```toml
model_provider = "shunt"

[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
```

カスタムプロバイダーを使う場合（CLI がローカルログインを必要としないよう `requires_openai_auth = false` を追加してください）、shunt へ向けた時点で Codex CLI 自身の `~/.codex/auth.json` は無関係になります — アカウントはリクエストごとに shunt のプールから来ます。一方 `chatgpt_base_url` の形状は CLI を ChatGPT ログインモードのままにするため、引き続きローカルのログインが必要で、**ゲートされていない**エンドポイントに対してのみ動作します。その ChatGPT ベアラーは設定された shunt トークンではないため、`[server.auth]` はそれを拒否します。

## クライアント認証

shunt に [`[server.auth]`](/ja/guides/shared-gateway/) が設定されている場合 — ループバックを超えるものには推奨です — クライアントトークンを、OpenAI 形式の Bearer キー（`OPENAI_API_KEY` / カスタムプロバイダーの `env_key`、LiteLLM/llmgateway の作法）**または** `x-shunt-token` ヘッダーの**いずれか**で提示します。

```toml
# A. Bearer — built-in openai provider. Set the base URL in ~/.codex/config.toml,
#    NOT via the OPENAI_BASE_URL env var: the env var leaves the CLI's Responses
#    WebSocket pointed at wss://api.openai.com, so it bypasses shunt. See
#    "Point the Codex CLI at shunt" in the connect guide.
openai_base_url = "http://127.0.0.1:3001/v1"
```

```bash
export OPENAI_API_KEY="<shunt-token>"      # sent as Authorization: Bearer
```

```toml
# B. Header — a custom provider carries it (use env_http_headers to keep it out of the file):
[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
http_headers = { "x-shunt-token" = "<token>" }
```

`[server.auth]` がなければ、このエンドポイントはそこへ到達できる誰にでも開かれています — ループバックや個人利用なら許容できますが、共有ゲートウェイでは不可です。クライアントが提示した認証情報は shunt への認証に**のみ**使われ、それ（および CLI がたまたま送る `Authorization`）は取り除かれ、上流へ転送されることはありません。`[server.admin]` の認証情報ヘッダー（既定では `x-shunt-admin-token`、`[server.admin] header` で指定した名前）も取り除かれます — 管理サーフェスはそのスロットで認証し、管理用の認証情報はアップストリームアカウントをプロビジョニングできるためです。`cookie` ヘッダーもヘッダーごと取り除かれます: 管理サーフェスは書き込み権限のセッション Cookie もそこで受理し、shunt 自身は Cookie ジャーを持たないため、上流がそれに依存することはありません。`x-api-key` も無条件に取り除かれます — `[server.auth]` が設定されていない場合も同様です。対象のプロバイダーは起動時に `chatgpt_oauth` 専用であることが検証されるため、インバウンドの `x-api-key` の値がこのアップストリームに対して有効な認証情報になることは決してありません。Claude Code の `apiKeyHelper` のように `Authorization` と `x-api-key` の両方に同じキーを設定するクライアントであっても、2 つ目のスロット経由でそのキーが漏れることはありません。インバウンドのクライアントが実際の Codex CLI であるため、パススルーはそのリクエストヘッダーをそのまま転送し（`version`、`originator`、`OpenAI-Beta`、`x-codex-*`、…）、差し替えるのは選択されたプールアカウントの `Authorization` ベアラーと `chatgpt-account-id` **だけ**です。認証の詳しい手順は [Codex CLI の接続](/ja/guides/connect-codex-cli/#3-shunt-クライアントトークンを提示するserverauth-設定時)を参照してください。

## アカウントのプロビジョニング

[Codex マルチアカウント](/ja/guides/codex-multi-account/#プールを設定する)と同じプールを再利用します。

```bash
codex login
shunt login codex --name main
```

```toml
[[providers.codex.accounts]]
name = "main"
```

`[[providers.codex.accounts]]` が設定されておらず、**かつ shunt のアカウントストアが空**の場合、エンドポイントはデフォルトの `~/.codex/auth.json` 認証情報 1 つへフォールバックします — プーリングもフェイルオーバーもありません。そのため `[server.codex_endpoint]` を設定した時点で、Codex ログイン 1 つで動作します。（ハンドラーはまずアカウントストアをスキャンし、見つかったアカウントをプールするため、インポート済みのストアアカウントがあればプーリングは有効になります。）

## モデルを別のアップストリームへルーティングする

既定ではすべてのリクエストが `[server.codex_endpoint]` に指定した 1 つのプロバイダーへ送られます。任意の `[[server.codex_endpoint.routes]]` テーブルを使うと、Codex CLI がモデル id によって**別の** Responses 互換アップストリームを選べます。ルートのないモデルは従来どおり固定プロバイダーへ送られます。

複数のベンダーが Codex CLI 向けのネイティブ Responses エンドポイントを文書化しています: Z.ai GLM (`https://api.z.ai/api/v1`)、DeepSeek (`https://api.deepseek.com`)、Kimi Code (`https://api.kimi.com/coding/v1`)、MiniMax (`https://api.minimax.io/v1`)、Mimo (`https://api.xiaomimimo.com/v1`)、OpenRouter (`https://openrouter.ai/api/v1`)、Vercel AI Gateway (`https://ai-gateway.vercel.sh/codex/v1`)、そして純正の OpenAI。shunt はプロバイダーの `base_url` に `/responses` を付け足すため、ベンダーが Codex 用として案内しているものと同じ base URL をそのまま設定します。アップストリームは Responses API をネイティブに実装している必要があります — Responses → Chat Completions のアダプターはありません。

```toml
[providers.glm]
kind = "responses"
auth = "api_key"
api_key_env = "GLM_API_KEY"
base_url = "https://api.z.ai/api/v1"

[providers.deepseek]
kind = "responses"
auth = "api_key"
api_key_env = "DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"

[server.codex_endpoint]
provider = "codex"

[[server.codex_endpoint.routes]]
model = "glm-5.3"
provider = "glm"

[[server.codex_endpoint.routes]]
model = "deepseek-v4-flash"
provider = "deepseek"
```

`upstream_model` は省略可能で、既定値は `model` です。CLI に入力する id とベンダーが実際に提供する id が異なる場合に指定します。ルーティング先のプロバイダーは実際の資格情報を持つ必要があります — クライアント自身の `Authorization` は常に削除されるため、資格情報を持たない認証モード（`passthrough` または `none`）は起動時に拒否されます。shunt 内蔵の `kimi` プリセットは `kind = "anthropic"` なので、Kimi Code への Codex ルートには別途 `kind = "responses"` のプロバイダーが必要です — Anthropic 種別のプリセットへ Codex モデルをルーティングすると起動時に拒否されます。

CLI 側では Codex を **shunt** に向け、`model` でルートを選びます:

```toml
# ~/.codex/config.toml
model = "glm-5.3"
model_provider = "shunt"
model_catalog_json = "~/.codex/models.json"

[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
env_key = "SHUNT_TOKEN"
```

shunt は Codex CLI のディスカバリー要求に対して有効なフォールバック `{"models":[]}` で応答しますが、Codex のルートをディスカバリー一覧で公開しません。CLI はこれらのベンダーが案内するとおり、`model_catalog_json` が指す `~/.codex/models.json` カタログからスラッグのメタデータを取得します。shunt のルートを選ぶのは `model` の値だけです。

**ChatGPT 以外**のアップストリームへルーティングされたリクエストで変わる点:

- **ヘッダーの許可リスト。** クライアントから引き継ぐのは `content-type` と `accept` のみで、これに解決された資格情報と、ルーティング先のアップストリーム自身が要求する identity が加わります — `OpenAI-Beta: responses=experimental`(xAI/Grok では省略)、および `xai_oauth` ルートの場合は Grok CLI の identity ヘッダー。`authorization`、`x-api-key`、`chatgpt-account-id`、`originator`、`version`、`user-agent`、`session-id`、`x-codex-*`、`x-shunt-*` はいずれもサードパーティに届きません。
- **ボディの `model` 書き換え。** `upstream_model` が要求されたモデルと異なる場合、shunt はトップレベルの `model` だけを書き換え、他のフィールドはそのまま残します。JSON オブジェクトでないボディはそのまま送らず `400` で拒否します。
- **identity エンコーディング。** zstd のリクエストボディはまずデコードされ(純正の Responses API はそのエンコーディングを受け付けません)、`content-encoding` は転送されません。
- **資格情報は 1 つ、フェイルオーバーなし。** ルーティング先のサードパーティの背後にプールはないため、429 や 5xx はローテーションを起こさず `retry-after` とともにそのままリレーされます。

マッチングは完全一致で大文字小文字を区別し、文字種の制限もありません。そのため `MiniMax-M3`、`openai/gpt-5.6-sol`、`~openai/gpt-latest` といったベンダーのスラッグも書いたとおりにルーティングされます。別の `chatgpt_oauth` プロバイダーへのルートであれば、プールのパススルーがそのまま維持されます。ルートはライブ設定から読まれるため、リロードで反映されます。

## `/v1/messages` との違い

- **変換なし。** インバウンドの Responses ボディはバイト単位でそのまま上流へ転送され、上流のレスポンス — SSE でも JSON でも、成功でもエラーでも — はそのまま中継されます（ステータスと `content-type` は保たれます）。Anthropic Messages ⇄ Responses の変換ステップは一切ありません。
- **圧縮されたリクエストボディはそのまま通過。** 現行の Codex リリースは ChatGPT バックエンドと通信する際にリクエストボディを zstd 圧縮します。これには、このエンドポイントへ向けた `chatgpt_base_url` の形状も含まれます。バイト列とその `content-encoding: zstd` ヘッダーは変更されずに転送されます。shunt は加えて、メトリクス・ログ・スパン用にリクエストの `model` を読み取るためだけに、メモリ上でコピーをデコードします。shunt がデコードできないボディでも中継自体は問題なく行われ、劣化するのは `model` ラベルが `unknown` になることだけで、理由を示す警告が出ます。
- **モデルに基づくルーティングはオプトイン。** 既定ではすべてのリクエストが `[server.codex_endpoint]` で指定された 1 つのプロバイダーへ行き、ボディの `model` フィールドはそのまま転送されます。`[[server.codex_endpoint.routes]]` を設定すると、完全一致する `model` がそのエントリのプロバイダーを選びます — [モデルを別のアップストリームへルーティングする](#モデルを別のアップストリームへルーティングする)を参照。
- **枯渇時はそのまま中継。** プールされたすべてのアカウントを試行し、少なくとも 1 つの上流レスポンスが返っていた場合、shunt はその最後のレスポンスを Anthropic 形式のエラーへ作り直すのではなく、変更せずに中継します。Responses のクライアントは、実際の ChatGPT バックエンドから受け取るはずの生の形を期待するためです。
- **ゲートウェイ自身のエラーは OpenAI 形式。** 失敗が shunt 自身のものである場合 — 不正または欠落したクライアントトークン（`401`）、上流レスポンスのないプールの解決不能（`502`）、サイズ超過のリクエストボディ、未設定のエンドポイント — shunt は同じステータスコードのまま、OpenAI Responses のエラー形（`{"error":{"message":…,"type":…,"code":null}}`）で返します。これにより Codex CLI は、Anthropic の `{"type":"error",…}` エンベロープではなく自身のエラー経路でパースできます。中継される*上流*のエラー（バックエンドからの 429/4xx/5xx）は、引き続きそのまま通過します。
- **HTTP/SSE のみ。** 対象のプロバイダーが `websocket = true` であっても、このエンドポイントは常に HTTP トランスポートを使います。

## セキュリティ

- ループバックを超えるものでは、このエンドポイントを `[server.auth]` でゲートしてください — プロバイダーはリクエストごとに実際の Codex ベアラーを注入します。
- クライアント自身の認証情報が Codex バックエンドへ届くことはありません。パススルーは Codex CLI 自身のリクエストヘッダーをそのまま転送し、差し替えるのは選択されたプールアカウントのベアラーと `chatgpt-account-id` だけです（shunt のクライアントトークンヘッダー、`[server.admin]` の認証情報ヘッダー、`cookie` ヘッダー全体、内部用の `x-shunt-inbound-client` ラベル、クライアントの `Authorization`/`chatgpt-account-id`、そして `x-api-key` はすべて取り除かれ、転送されることはありません）。
- 起動時に一度だけ決まるのは、エンドポイントの **HTTP ルート登録**だけです。`[server.codex_endpoint]` の実行時のオン/オフ切り替えは、それらのパスを追加・削除するには再起動が必要である旨の警告をログに出力します。テーブルが*保持している*内容はすべてホットリロードされます — 対象の `provider` と `[[server.codex_endpoint.routes]]` のモデルテーブル全体はリクエストごとにライブ設定から読まれるため、ルートの追加・編集・削除はリロードで反映されます。
