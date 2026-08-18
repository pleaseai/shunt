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

`[server.auth]` がなければ、このエンドポイントはそこへ到達できる誰にでも開かれています — ループバックや個人利用なら許容できますが、共有ゲートウェイでは不可です。クライアントが提示した認証情報は shunt への認証に**のみ**使われ、それ（および CLI がたまたま送る `Authorization`）は取り除かれ、上流へ転送されることはありません。`[server.admin]` の認証情報ヘッダー（既定では `x-shunt-admin-token`、`[server.admin] header` で指定した名前）も取り除かれます — 管理サーフェスはそのスロットで認証し、管理用の認証情報はアップストリームアカウントをプロビジョニングできるためです。`x-api-key` も無条件に取り除かれます — `[server.auth]` が設定されていない場合も同様です。対象のプロバイダーは起動時に `chatgpt_oauth` 専用であることが検証されるため、インバウンドの `x-api-key` の値がこのアップストリームに対して有効な認証情報になることは決してありません。Claude Code の `apiKeyHelper` のように `Authorization` と `x-api-key` の両方に同じキーを設定するクライアントであっても、2 つ目のスロット経由でそのキーが漏れることはありません。インバウンドのクライアントが実際の Codex CLI であるため、パススルーはそのリクエストヘッダーをそのまま転送し（`version`、`originator`、`OpenAI-Beta`、`x-codex-*`、…）、差し替えるのは選択されたプールアカウントの `Authorization` ベアラーと `chatgpt-account-id` **だけ**です。認証の詳しい手順は [Codex CLI の接続](/ja/guides/connect-codex-cli/#3-shunt-クライアントトークンを提示するserverauth-設定時)を参照してください。

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

## `/v1/messages` との違い

- **変換なし。** インバウンドの Responses ボディはバイト単位でそのまま上流へ転送され、上流のレスポンス — SSE でも JSON でも、成功でもエラーでも — はそのまま中継されます（ステータスと `content-type` は保たれます）。Anthropic Messages ⇄ Responses の変換ステップは一切ありません。
- **圧縮されたリクエストボディはそのまま通過。** 現行の Codex リリースは ChatGPT バックエンドと通信する際にリクエストボディを zstd 圧縮します。これには、このエンドポイントへ向けた `chatgpt_base_url` の形状も含まれます。バイト列とその `content-encoding: zstd` ヘッダーは変更されずに転送されます。shunt は加えて、メトリクス・ログ・スパン用にリクエストの `model` を読み取るためだけに、メモリ上でコピーをデコードします。shunt がデコードできないボディでも中継自体は問題なく行われ、劣化するのは `model` ラベルが `unknown` になることだけで、理由を示す警告が出ます。
- **モデルに基づくルーティングなし。** すべてのリクエストは `[server.codex_endpoint]` で指定された 1 つのプロバイダーへ行き、ボディの `model` フィールドはそのまま転送され、プロバイダーを選択することはありません。
- **枯渇時はそのまま中継。** プールされたすべてのアカウントを試行し、少なくとも 1 つの上流レスポンスが返っていた場合、shunt はその最後のレスポンスを Anthropic 形式のエラーへ作り直すのではなく、変更せずに中継します。Responses のクライアントは、実際の ChatGPT バックエンドから受け取るはずの生の形を期待するためです。
- **ゲートウェイ自身のエラーは OpenAI 形式。** 失敗が shunt 自身のものである場合 — 不正または欠落したクライアントトークン（`401`）、上流レスポンスのないプールの解決不能（`502`）、サイズ超過のリクエストボディ、未設定のエンドポイント — shunt は同じステータスコードのまま、OpenAI Responses のエラー形（`{"error":{"message":…,"type":…,"code":null}}`）で返します。これにより Codex CLI は、Anthropic の `{"type":"error",…}` エンベロープではなく自身のエラー経路でパースできます。中継される*上流*のエラー（バックエンドからの 429/4xx/5xx）は、引き続きそのまま通過します。
- **HTTP/SSE のみ。** 対象のプロバイダーが `websocket = true` であっても、このエンドポイントは常に HTTP トランスポートを使います。

## セキュリティ

- ループバックを超えるものでは、このエンドポイントを `[server.auth]` でゲートしてください — プロバイダーはリクエストごとに実際の Codex ベアラーを注入します。
- クライアント自身の認証情報が Codex バックエンドへ届くことはありません。パススルーは Codex CLI 自身のリクエストヘッダーをそのまま転送し、差し替えるのは選択されたプールアカウントのベアラーと `chatgpt-account-id` だけです（shunt のクライアントトークンヘッダー、`[server.admin]` の認証情報ヘッダー、内部用の `x-shunt-inbound-client` ラベル、クライアントの `Authorization`/`chatgpt-account-id`、そして `x-api-key` はすべて取り除かれ、転送されることはありません）。
- ルートの集合は起動時に一度だけ決まります。`[server.codex_endpoint]` の実行時のオン/オフ切り替えは、再起動が必要である旨の警告をログに出力します。リロードでも、どのプロバイダーを対象にするかは変更できます。
