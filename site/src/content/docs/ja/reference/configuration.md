---
title: 設定リファレンス
description: すべての shunt.toml キー — server、providers、routes、models。
---

ファイルの場所、優先順位、注釈付きの例については [Configuration](/ja/guides/configuration/) を参照してください。完全なテンプレート: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)。

## Secret 参照

設定ファイルの文字列値は、リテラルの代わりに `${VAR}` または `${file:/絶対/パス}` として書けます。`${VAR}` は環境変数 `VAR` の値に置き換わり、`"Bearer ${TOKEN}"` のようにより長い文字列に埋め込むこともできます(変数が未定義だと設定の読み込みは失敗します)。`${file:/絶対/パス}` は指定したファイルの内容(トリム済み)に置き換わり、パスは絶対パスでなければならず、フィールドの値全体でなければなりません — 他の文字列に埋め込むことはできません(ファイルが読み取れない、パスが相対パスである、または他の文字列に埋め込まれている場合、設定の読み込みは失敗します)。`$${` はリテラルの `${` にエスケープされます。解決は再帰的ではありません — 解決済みの値は再スキャンされません。この置換は設定ファイルにのみ適用され、`SHUNT_*` 環境変数オーバーライドはそのまま使われます。起動時、`shunt check`、[ホットリロード](https://github.com/pleaseai/shunt/blob/main/docs/config-reload.md)(SIGHUP とファイル監視)を含む設定の読み込みのたびに再実行されるため、`${file:}` で参照したシークレットはファイルを書き換えてリロードをトリガーするだけで、再起動なしにローテーションできます。ただし、ローテーションした値が実際に反映されるかどうかは、そのフィールド自身のリロード動作に従います。`[sentry]` と `[otel]` は起動時に一度だけ初期化されるため、この 2 つのセクションのシークレットをローテーションしても設定が更新されるだけで、反映するには再起動が必要です。

`[sentry] dsn`、`[otel.headers]` の値、`[server.gateway.telemetry] forward_to[].headers` の値、`[server.gateway.session] jwt_secret`、そして `[[server.admin.write_keys]]`・`[[server.admin.read_keys]]` 各エントリーの `key` — この 6 つのフィールドパスは redacting secret 型として扱われ、診断出力では `[redacted]` と表示されます。前の 4 つはリテラル値を書いても以前とまったく同じように動作し、リテラルを保持している場合、shunt は起動時に該当するフィールドパスのみを(値は決して含めずに)知らせる勧告的な警告を 1 回記録します。管理キー配列の 2 つは例外で、リテラルを書くと警告ではなく**設定の読み込み自体が失敗**します。

既存の `tokens_env`、`jwt_secret_env`、`client_secret_env`、`api_key_env`、`users_env`、`token_env`、`tokens_file` フィールドはこの変更の影響を受けず、引き続き環境変数(`tokens_file` の場合はファイルパス)を指します(`jwt_secret_env` は別途 [`session.jwt_secret`](#servergatewaysessionオプション) に置き換えられ deprecated です)。

## `[server]`

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt がリッスンするアドレス |
| `default_provider` | `anthropic` | マッチするルートがないモデルのプロバイダー |
| `shutdown_timeout_seconds` | `30` | 最初の SIGTERM/SIGINT 後、実行中の HTTP/SSE/WebSocket をドレインしてから残りをキャンセルするまでの秒数。`1`–`3600` が必須で、変更後は再起動が必要です |
| `max_concurrent_requests` | `1024` | レスポンスボディの完了まで実行中として数えるインバウンドリクエストの最大数。超過したリクエストはキューに入れず、即座に `503` と `Retry-After: 1` で拒否します。`0` で制限を無効化でき、`/` と `/health` は対象外です。このキーを変更した後は再起動が必要です |
| `sse_keepalive_seconds` | `30` | SSE `ping` が注入されるまでのアイドル秒数。`0` で無効化（[詳細](/ja/guides/shared-gateway/#sse-キープアライブ-ping)） |

## HTTP チューニングテーブル

`[server.access_control]` は `allow_cidrs = []`、`deny_cidrs = []`、`trust_forwarded_for = false` を提供します。deny が先に評価され、`/` と `/health` にも適用されます。allow リストが空でなければデフォルト拒否になりますが、この 2 つのヘルスパスは allow チェックだけを免除されます。転送ヘッダーは、クライアント指定値を上書きする信頼済みプロキシの背後でのみ信頼してください。変更には再起動が必要です。

この `trust_forwarded_for` 設定は `[server.gateway] trust_forwarded_for` とは独立しています。access-control の設定は CIDR の許可・拒否ルールだけに適用され、gateway の設定はデバイスフローのレート制限だけに適用されます。両方のサーフェスを信頼済みリバースプロキシの背後で運用する場合は、両方の設定を有効にしてください。一方だけを設定すると、もう一方のサーフェスは引き続きソケットのピアアドレスを使用します。

`[server.limits]` の `max_request_bytes` は Anthropic Messages とインバウンド Codex Responses のリクエストボディに適用され、デフォルトは `33554432`（32 MiB）です。超過時は `413` を返します。その他のゲートウェイ、管理、テレメトリ、分析ルートでは、エンドポイント固有のボディ制限が維持されます。`max_request_header_bytes` と `max_url_length` はデフォルト未設定で、それぞれ `431` と `414` を返します。ヘッダーサイズは、解析済みの全ヘッダーについて名前と値の長さを合計した値です。ボディ制限はホットリロードされますが、ヘッダーと URL の制限には再起動が必要です。

`[server.timeouts] upstream_ttfb_ms` はデフォルト `120000` で、`0` で無効化します。推論アップストリームの HTTP レスポンスヘッダー待ちだけを制限するため、レスポンスボディと長時間の SSE ストリームには全体時間制限を設定しません。SSE レスポンスをまだコミットしていないリクエストは `504 timeout_error` を返し、コミット済みストリーミングリクエストは同じエンベロープを持つ 1 つのターミナル SSE `error` イベントとしてタイムアウトを通知します — タイムアウトがチェーンを進めることは決してありません。Anthropic Messages、OpenAI Responses HTTP（WebSocket フォールバックを含む）、Gemini HTTP、インバウンド Codex Responses パススルーを対象とし、Codex WebSocket、Cursor、Antigravity、補助 HTTP 呼び出しは対象外です。

`[server.rate_limits.device_authorization]` のデフォルトは `max = 30`、`window_seconds = 600`、`[server.rate_limits.device_verify]` は `max = 10`、`window_seconds = 600` です。2 つの per-IP 制限は独立し、`[server.gateway]` がなければ無効です。変更には再起動が必要です。

## `[server.auth]`（オプション）

このテーブルの存在がインバウンドのクライアントトークン認証を有効化します（[詳細](/ja/guides/shared-gateway/)）。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | クライアントトークンを運ぶヘッダー |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | カンマ区切りの `name:token` ペアを保持する環境変数 |

指定された環境変数には 1 つ以上の認証情報が必要です。例: `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`。テーブルが存在するのに変数が未設定・空・不正な場合、起動はフェイルクローズします。ゲートされるルート（マッピングされた `/v1/messages` 推論と `GET /v1/models` discovery）は、設定されたヘッダー、`Authorization: Bearer`、`x-api-key` のいずれでもトークンを受け付けます — 複数のスロットに有効なトークンがある場合は専用ヘッダーが優先されます。

`tokens_env` の値も、他の設定ファイル文字列と同様に `${VAR}` / `${file:...}` で書けます([Secret 参照](#secret-参照)を参照)。shunt がトークンを読み取る環境変数名を指す点は変わりません。

## `[server.admin]`（オプション）

このテーブルの存在が、ブラウザーでのアカウントプロビジョニングとアカウントプールの健全性のための管理 Web サーフェスを有効化します（[詳細](/ja/guides/admin-remote-provisioning/)）。テーブルがない場合、`/admin*` ルートは一切登録されません。同じ認証情報が [`[server.spend]`](#serverspendオプション) の spend-limit API も認証します。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | API/curl 呼び出し用の管理認証情報を運ぶヘッダー。管理ルーターと spend-limit ルーターでは `x-api-key` も併せて受け付けます |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | カンマ区切りの `name:token` ペアを保持する環境変数。これは **write** ティアです |
| `tokens_file` | _(未設定)_ | `name:token` ペアを保持するファイルのパス（1 行に 1 つ、またはカンマ区切り）。`tokens_env` が未設定または空のときに使われます。これも **write** ティアです |
| `session_ttl_secs` | `3600` | ログイン後のブラウザーセッションの寿命（秒） |
| `pending_ttl_secs` | `600` | 開始したプロビジョニングフローを完了できる時間（秒） |
| `hide_observed` | `false` | `true` の場合、shunt はホストの CLI/アプリのログインを一切読み取りません。`GET /admin/api/observed` は引き続き管理認証を必要としますが空のリストを返し、ダッシュボードの **Accounts and usage** 表には管理対象のプールアカウントのみが表示されます。設定のホットリロードで適用され、すでに開いているダッシュボードはページを再読み込みすると新しい値に従います |

管理トークンは環境変数からもファイルからも与えられます。指定された環境変数には 1 つ以上の認証情報が必要です。例: `SHUNT_ADMIN_TOKENS="ops:<token>"`。あるいは `tokens_file` にパス（`~` は展開されます）を設定し、そのファイルにペアを置くこともできます — これは `shunt dashboard setup` が `~/.shunt/admin-token` に書き込むファイルそのもので、起動環境に秘密を置かずに済みます。両方が設定されている場合は、空でない `tokens_env` が優先されます。テーブルが存在するのに 3 つの認証情報ソース（`tokens_env`/`tokens_file`、`write_keys`、`read_keys`）が**すべて**未設定・空・不正な場合、起動はフェイルクローズします。`tokens_env` を設定せずキー配列だけを使う構成は正常に起動します。

管理認証情報は `[server.auth]` の下で設定されるクライアントトークンとは別個の認証情報です。1 つの認証情報を両方のサーフェスで再利用しないでください。管理認証情報が認証するのは `/admin*` と spend-limit ルートだけで、推論ルートを認証することはありません — そちらの `x-api-key` は呼び出し元自身の Anthropic 認証情報スロットです。またこれらのルーターがあるスロットで受け付けた値は、上流へのリクエスト前に同じスロットから取り除かれるため、管理認証情報が provider に転送されることはありません。

`[server.auth]` の `tokens_env` と同様、この `tokens_env` と `tokens_file` の値も `${VAR}` / `${file:...}` で書けます([Secret 参照](#secret-参照)を参照)。

### `[[server.admin.write_keys]]` / `[[server.admin.read_keys]]`（オプション）

`{ id, key }` テーブルを要素とする 2 つのキー配列です。`id` はログに出しても安全で、spend-limit の監査証跡には `admin-key:<id>` として記録されます。`tokens_env`/`tokens_file` のペアは代わりに `admin-token:<name>` として記録されます。

```toml
[[server.admin.write_keys]]
id = "terraform"
key = "${SHUNT_ADMIN_KEY_TERRAFORM}"

[[server.admin.read_keys]]
id = "reporting"
key = "${file:/run/secrets/shunt-reporting-key}"
```

| 配列 | アクセス権 | 意味 |
| :-- | :-- | :-- |
| `write_keys` | `write` | フルアクセス。`write` は `read` を含みます。`tokens_env`/`tokens_file` と同じティアです |
| `read_keys` | `read` | 管理サーフェスと spend-limit API のすべての `GET` を通過し、すべての変更操作では `403 permission_error` で拒否されます。ダッシュボードには読み取り専用セッションとしてサインインできます: `POST /admin/login` はこれを受け入れ、セッションが `read` 階層を記録し、その Cookie で送る変更操作は引き続き `403` で拒否されます |

認証情報の権限は一致したすべての集合に対する**最大値**なので、集合を走査する順序が権限を変えることはありません。各 `id` は空であってはならず、各キーは 32 文字以上である必要があります。id とキー値はそれぞれ 3 つの認証情報集合（`tokens_env`/`tokens_file`、`write_keys`、`read_keys`）全体で一意でなければならず、衝突した場合はキー値をログに出さずに衝突した id だけを報告します。32 文字未満の既存 `tokens_env` トークンは、このルールより前から存在するため失敗ではなく警告になります。

各 `key` は redacting secret であり（[Secret 参照](#secret-参照)を参照）、リテラルが警告ではなく**設定ロードの失敗**になる唯一のフィールドです。`${VAR}`、`${file:/絶対/パス}`、または `SHUNT_*` 環境変数オーバーライドで供給してください。

## `[server.spend]`（オプション）

このテーブルの存在が、`/v1/organizations/spend_limits` 配下の spend-limit Admin API を登録します。**ポリシーのみ**を保持するトップレベルのセクションで、キー材料は一切持ちません。ルートは [`[server.admin]`](#serveradminオプション) の認証情報で認証するため、spend limit を有効にしても gateway ログインサーフェスは不要です。`[server.admin]` のない `[server.spend]` は設定検証に失敗します。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `blocked_message` | 未設定 | 将来の上限エラー用。ステージ 1 では使用しません |
| `audit_retention_days` | `365` | 将来の監査レコード保持日数 |
| `spend_retention_months` | `13` | 将来の支出データ保持月数 |
| `identity_retention_days` | `90` | 将来のアイデンティティ保持日数 |
| `group_limit_mode` | `min` | `min` または `max`。将来のグループ上限解決用 |
| `state_path` | `~/.shunt/gateway-spend.json` | 上限と監査レコードを保存するバージョン付き JSON。`""` はメモリのみ |

管理認証情報は設定された `[server.admin] header` または `x-api-key` で送信します。`read_keys` の認証情報は `GET` のみ使用できます。状態ファイルは変更のたびに非公開の一時ファイルを使ってアトミックに置換されます。ホームディレクトリを解決できない場合、デフォルトはメモリのみです。テーブルの追加・削除と状態パスはどちらも起動時に固定され、設定のリロードでは適用されず警告が記録されます。

### `[server.spend.enforcement]`（オプション）

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `fail_closed_on_error` | `false` | 将来の上限適用ステージ用。ステージ 1 では読み取りません |

ステージ 1 はこれらの保持設定、`blocked_message`、`group_limit_mode`、`fail_closed_on_error` を受け付けますが、推論への上限適用、使用量計測、`/effective`、`/audit`、保持スイープ、group scope はまだ実装していません。

## `[server.gateway]`（オプション）

このテーブルの存在が、Claude Code の managed `forceLoginMethod: "gateway"` で使う [OAuth device-flow gateway ログイン](/ja/guides/gateway-login/)を有効化します。テーブルがなければ、shunt は `/.well-known/oauth-authorization-server`、`/oauth/device_authorization`、`/oauth/token`、`/device`、`/managed/settings` を登録しません。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `public_url` | 必須 | JWT issuer および OAuth endpoint の基点となる外部公開 HTTPS origin。`http` は loopback のみ許可 |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | 32 bytes 以上の HS256 signing secret を保持する env 変数。**Deprecated**。単独使用では引き続き完全にサポートされる — [`session.jwt_secret`](#servergatewaysessionオプション) に置き換えられた |
| `users_env` | `SHUNT_GATEWAY_USERS` | カンマ区切りの `email:secret` approval user を保持する env 変数 |
| `token_ttl_seconds` | `3600` | access token の寿命。`expires_in` として返される。**Deprecated**。単独使用では引き続き完全にサポートされる — [`session.ttl_hours`](#servergatewaysessionオプション) に置き換えられたが、1 時間未満の寿命を指定できる唯一の方法として残る |
| `trust_forwarded_for` | `false` | `/device` の rate-limit identity として `X-Forwarded-For`／`X-Real-IP` を信頼する。client 提供値を置換する trusted proxy の背後でのみ有効化 |

URL が path 等を含まない HTTPS origin でない場合（`http` は loopback のみ許可）、TTL が 0 の場合、secret がないか 32 bytes 未満の場合、または user list が空・不正な場合、起動は fail closed します。secret には `:` を含められ、最初の colon だけが email と secret を分けます。`jwt_secret_env` と `users_env` の値も、他の設定ファイル文字列と同様に `${VAR}` / `${file:...}` で書けます([Secret 参照](#secret-参照)を参照)。env-backed secret と user の変更は config reload で反映されますが、route tree は boot 時に固定されるため、テーブルの追加・削除には restart が必要です。

Deprecated なキーと、それに対応する `[server.gateway.session]` の置き換えキーを両方設定すると、キーごとに起動が失敗します: `jwt_secret_env` と `session.jwt_secret` を併用するとエラー、`token_ttl_seconds` と `session.ttl_hours` を併用するとエラーです。2 つのペアをまたいで組み合わせる(例: `session.jwt_secret` と `token_ttl_seconds` の併用)のは問題ありません。shunt は、deprecated なキーが設定ファイルであれ `SHUNT_*` 環境変数 override であれ明示的に設定されるたびに deprecation 警告を 1 回記録し、そのキー自体が一切設定されずデフォルトが適用される場合にのみ警告なしのままです — `jwt_secret_env` を設定せず `SHUNT_GATEWAY_JWT_SECRET` env 変数に secret の値だけを入れておく設定は、その変数が deprecated なキー自体ではなく secret の値を保持しているだけなので、引き続き警告しません。ペアの片方だけが設定されている場合、`session.*` があればそちらが優先され、なければ deprecated なキー、どちらもなければデフォルトが使われます。

発行された bearer は、選択された provider が server-side credential を注入する場合に `/v1/models`、`/v1/messages`、`/v1/messages/count_tokens` を認証します。passthrough provider は open のままです。`[server.auth]` もある場合は、どちらかの credential で access できます。device grant と rotating refresh token は process-lifetime の in-memory state です。config reload では維持されますが、restart では無効になります。

### `[server.gateway.session]`（オプション）

upstream の Claude apps gateway の `session:` ブロックに対応します:

```toml
[server.gateway.session]
jwt_secret = "${SHUNT_GATEWAY_JWT_SECRET}"
ttl_hours = 1
```

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `jwt_secret` | このテーブルがある場合は必須 | HS256 signing secret。32 bytes 以上の entropy が必要(例: `openssl rand -base64 32`)。単一の文字列、またはローテーション用の array も指定可能 — index 0 が新しいトークンに署名し、すべてのエントリが検証に使われる |
| `ttl_hours` | `1` | access token の寿命(時間単位) |

`jwt_secret` は `Secret` 型のフィールドです: 他の設定ファイル文字列と同様に `${VAR}` / `${file:/絶対/パス}` を使え([Secret 参照](#secret-参照)を参照)、診断出力では redact されます。既存のセッションを無効化せずにローテーションするには、新しい secret を array の先頭に追加し、`ttl_hours` の間だけ待って未完了の access token を失効させてから、古いエントリを削除します:

```toml
[server.gateway.session]
jwt_secret = ["new-secret-value", "old-secret-value"]
```

### `[[server.gateway.policies]]`（オプション）

`[server.gateway]` が存在すると、認証済み `GET /managed/settings` が登録されます。順序付きの空でない policy list は、その managed document を提供します。各 policy は任意の `[server.gateway.policies.match]` と、必須の open-schema `[server.gateway.policies.cli]` object を持ちます。`match` の省略、`match = {}`、または `emails` なしは catch-all です。明示的な空の `emails` list または空白 entry は起動エラーです。

すべての catch-all policy を順番に merge し、その上に最初の完全一致（case-sensitive）email policy を merge します。object は再帰的に merge し、array は置換します。ただし key に `deny` を含む array は重複なしの union になります。既知の key は起動時と hot reload 時に検証されます。`availableModels` は string のみの array、`env` は string・number・boolean の scalar value のみを含む table でなければなりません。未知の key は open-schema のままですが、すべての value は JSON で表現可能でなければならず、非有限 float は拒否されます。

`policies` がなければ endpoint は `404` を返します。policy が設定されていても user-specific または catch-all settings が一致しない場合、telemetry が有効なら telemetry のみの `settings.env` を、無効なら `settings: {}` を含む `200` を返します。response は `uuid`、`checksum`、checksum を含む quoted `ETag` を持ち、一致する `If-None-Match` には `304` を返します。

解決された `cli.availableModels` は gateway JWT request の `/v1/messages` と `/v1/messages/count_tokens` に適用されます。top-level `model` から末尾の Claude Code context-window hint（`[1m]` または `[1M]`）を 1 つ取り除いてから比較し、list にない場合は `400 invalid_request_error` になります。static `[server.auth]` credential は gateway policy user を識別しないため、この制限の対象外です。

### `[server.gateway.telemetry]`（オプション）

`forward_to` は、必須の base OTLP/HTTP `url`、任意の string `headers` map、signal ごとの opt-in boolean（`metrics` は既定で `true`、`logs`／`traces` は既定で `false`）を持つ destination の array です。`headers` の各値は redacting secret 型として扱われ、診断出力では `[redacted]` と表示されます([Secret 参照](#secret-参照)を参照)。いずれかの signal を opt-in した list は managed `settings.env` に 6 つの値を注入します。`CLAUDE_CODE_ENABLE_TELEMETRY=1`、各 `OTEL_METRICS_EXPORTER`／`OTEL_LOGS_EXPORTER`／`OTEL_TRACES_EXPORTER` はその signal を opt-in した destination があれば `otlp`、なければ `none`、`OTEL_EXPORTER_OTLP_ENDPOINT=public_url`、`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` です。どの signal も opt-in されていない場合は何も注入しません。競合時は policy の env value が優先します。同じ list は inbound ingest も駆動します（M-C、#189）。`[server.gateway]` があれば常に登録される `POST /v1/{metrics,logs,traces}` route がクライアントの OTLP payload を受け取り、その signal を opt-in したすべての destination に verbatim で relay し、opt-in した destination がない signal は受理後に破棄します。`logs`／`traces` が既定で off なのは、Claude Code の log record と span に command line、prompt、ファイルパスが含まれ得るためです。

```toml
[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
headers = { "x-api-key" = "..." }
```

デフォルトでは `/device` は forwarding header を無視し、socket peer を rate limit します。shunt が、client 提供の forwarding header を削除して自分の値を設定する trusted reverse proxy からのみ到達可能な場合に限り、`trust_forwarded_for = true` を設定してください。直接公開された gateway では有効化しないでください。

## `[server.codex_endpoint]`（オプション）

このテーブルは inbound の OpenAI Responses passthrough を有効にし、**Codex CLI** が `base_url` を shunt に向けて ChatGPT/Codex OAuth アカウントプール間で load balancing できるようにします（[詳細](/ja/guides/inbound-codex-endpoint/)）。テーブルがなければ、ルートは登録されません。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `provider` | `codex` | どの route にも `model` が一致しない inbound request を処理する `[providers.<name>]` テーブル名。`auth = "chatgpt_oauth"` を使う必要があります |
| `routes` | `[]` | オプションのモデル単位ルーティング（下記参照） |

`POST /backend-api/codex/responses`、`POST /responses`、`POST /v1/responses` を登録し、いずれも指定した provider のアカウントプールが処理します。`[server.auth]` があれば、他のサーバー側 credential ルートと同様に有効なクライアントトークンを要求します。`[server.auth]` がなければ、オペレーターの Codex credential を注入しつつ到達可能な誰にでも**開放**された状態になるため、loopback 以外の環境では必ず保護してください。`/v1/messages` と異なり、request は Anthropic Messages へ変換したりその逆を行ったりせず、アップストリームへそのまま relay されます。

### `[[server.codex_endpoint.routes]]`（オプション）

各エントリは、上記の固定 `provider` の代わりに、特定のモデル 1 つを別の Responses 互換アップストリームへ送ります。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `model` | *(必須)* | Codex クライアントが Responses 本文で送る公開モデル id。**完全一致**かつ**大文字小文字を区別**します — prefix マッチも `[1m]` の除去も文字集合の制限もないため、`MiniMax-M3`、`openai/gpt-5.6-sol`、`~openai/gpt-latest` のようなベンダーのスラッグも書いたとおりにルーティングされます |
| `provider` | *(必須)* | このモデルを提供する provider。`kind = "responses"` でなければならず、credential を持たない auth モード（`passthrough` または `none`）は使えません |
| `upstream_model` | `model` | アップストリームへ送るモデル id。`model` と異なる場合、shunt は本文トップレベルの `model` だけを書き換え、他のフィールドはそのまま残します |

未知の provider、`responses` 以外の provider、credential を持たない auth モード（`passthrough` または `none`）の provider へ向かう route は検証で拒否され、重複した `model` や空のフィールドも拒否されます。route はライブの設定スナップショットから読み込まれるため、追加・編集・削除は**リロード**時に反映されます。再起動が必要なのは `[server.codex_endpoint]` テーブル自体を有効化・無効化するときだけです。ChatGPT 以外の provider へルーティングされた request は、新しく組み立てたヘッダー許可リスト（`content-type`、`accept`、flavor ゲートを通過した `OpenAI-Beta`、そして `xai_oauth` route の場合は Grok CLI の identity ヘッダー）と identity エンコードの本文、credential 1 つだけを使い、プールもフェイルオーバーもありません。
同じオプトインで `GET /models` と `GET /backend-api/codex/models` も登録され、通常のモデル検出認証ゲートの後に有効な Codex フォールバック `{"models":[]}` を返します。共有の `GET /v1/models` でも、`client_version` クエリがある場合は Anthropic 風のヘッダーより優先して Codex の空形式を選択します。`client_version` がなければ、既存の Anthropic 検出レスポンスは変わりません。shunt は不完全な Codex `ModelInfo` 行を生成しません。

## `[server.usage]`（オプション）

このテーブルの存在により、共有アカウントプールのクォータ状態をサニタイズして集約した `GET /usage` が登録されます。管理サーフェスを使わずに、クライアントがスロットリングを予測するためのエンドポイントです（[エンドポイントの詳細](/ja/reference/endpoints/)）。テーブルがなければ、ルートは登録されません。

現在このテーブルにキーはなく、存在だけで有効になります。[`[server.auth]`](#serverauthオプション) が必須です。呼び出し元をクライアントトークンで識別するため、`[server.auth]` なしで `[server.usage]` を設定すると起動に失敗し、プールのテレメトリーを未認証で提供することはありません。

`GET /usage` は `/v1/messages` と同じクライアントトークン（設定されたヘッダー、`x-api-key`、または `Authorization: Bearer`）で認証し、ウィンドウごとの残り余裕（そのウィンドウを報告した無効化されていないアカウントの `mean(1 - utilization)`、つまりプール全体の容量のうちまだ使われていない割合 — 使い切ったアカウント 9 つと新しいアカウント 1 つなら `0.1` — プール全体の集約値であり、次のリクエストが受け付けられるかの予測ではありません）、それらのアカウントが報告した最も早いリセット時刻、`ok`／`degraded`／`exhausted` のステータスを返します。アカウント名、件数、priority、`disabled`、しきい値、アカウント単位の数値は返しません。ウィンドウが `null` になるのは、無効化されていないアカウントがそのウィンドウを一度も報告していない場合だけです。Codex の `x-codex-*` レスポンスヘッダーは 5 時間と共有週次ウィンドウを埋めます。Codex 自体には Fable スコープ（`7d_oi`）のシグナルはありませんが、混在したプロバイダープールでは別のプロバイダーが集約 Fable 値を提供できます。正の `usage_refresh_seconds` を設定すると、オプションの `wham/usage` ポーラーも imported かつ更新可能な `chatgpt_oauth` アカウントのそのウィンドウを埋めます。ポーリングはデフォルトで無効です。

レスポンスはプール全体の集計を `pool` に持ち、`providers` にはプールされるプロバイダーごとの同じサニタイズ済み集計を、設定されたプロバイダー名（`[providers.<name>]` の `<name>`、または `[[upstreams]]` エントリの `name` であり、アカウントの身元ではありません）をキーとして持ちます。モデルをそのキーに対応付けるのはクライアントの役割です。[`GET /routes`](/ja/reference/endpoints/) は `[[routes]]` に明示されたモデルだけを扱い、`[[models]].upstream_model`、`[[route_prefixes]]`、`server.default_provider` の対応付けを公開するエンドポイントはなく、`GET /v1/models` のエントリにはプロバイダーのフィールドがありません。混在プールでは `pool` がすべてのプロバイダーのアカウントを 1 つの平均にまとめて報告するため、特定のプロバイダーにルーティングするクライアントは、そのプロバイダーの余裕とステータスを `providers.<name>` から読み取ってください。プールされない認証モードのプロバイダーは省略され、Fable スコープのシグナルを持たないプロバイダーの `fable` ウィンドウは、`pool` が値を報告していても `null` です。完全な形は[エンドポイントリファレンス](/ja/reference/endpoints/)を参照してください。

## `[server.pool]`（オプション）

バージョン2の移行では、`observed_at_status` のない集約 `status` が、保存された `reset_5h`、`reset_7d`、`reset_7d_oi` のうち最も早いリセットを不変の期限として捕捉します。そのリセットがすでに過ぎている場合は、期限切れのリセット、スタンプのない集約 `status`、およびそのために合成したスタンプを同じ import で削除します。7 日という妥当な範囲を超える未来のリセットは、起動時刻から 7 日後を上限にします。リセットがなければ起動時刻から 7 日の上限を開始します。既存の v2 スタンプはリセットから再解釈しませんが、通常の import は孤立したメタデータを正規化し、経過したシグナルを失効させ、未来の時刻を起動時刻に補正し、残ったスタンプのない集約には必要に応じて起動時刻を設定します。後続の reset-only または usage 更新は捕捉した期限を延長せず、v3 への書き換えと二回目の復元後も同じ状態を保ちます。

アカウントプール向けの、クォータを考慮した負荷分散のチューニングです — Claude（Anthropic）（[詳細](/ja/guides/anthropic-multi-account/#選択のチューニングserverpool)）と、issue #195 以降は Codex/ChatGPT（[詳細](/ja/guides/codex-multi-account/)）が対象です。テーブルが存在しない場合、選択はこのテーブルが導入される前と同じ、組み込みの単一しきい値 `0.98` を使います。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | すべてのクォータウィンドウに対する安全策のバックストップ。これ以上のアカウントは、利用可能なアカウントの中で常に最後にソートされます |
| `default_threshold` | 未設定 | より具体的な値を持たないウィンドウに対するソフトなデフォルトしきい値 |
| `default_threshold_5h` | 未設定 | 5 時間ウィンドウのソフトなデフォルト |
| `default_threshold_7d` | 未設定 | 共有の週次（`7d`）ウィンドウのソフトなデフォルト |
| `default_threshold_fable` | 未設定 | fable 専用の週次（`7d_oi`）ウィンドウのソフトなデフォルト |
| `burn_rate_avoidance` | `false` | ウィンドウのリセット前にソフトしきい値を使い切ると予測されるアカウントも回避する |
| `usage_refresh_seconds` | 無効（`0`/未設定） | Claude `GET /api/oauth/usage` と Codex `GET /wham/usage` のポーリング間隔（秒）。60 未満の正の値は 60 秒の下限に切り上げられます |
| `state_path` | 未設定 | プールのアカウント単位のクォータ状態を保存するファイル。再起動時に空のプールではなく、最後に観測された使用率からウォームスタートします。未設定で永続化は無効（デフォルト） |
| `ramp_initial_concurrency` | 無効（`0`/未設定） | ストーム制御: トラフィックを受け始めたばかりのアカウントアイデンティティに対する初期の並行受け入れ許容量。`0` または未設定で受け入れゲーティングは無効 |
| `reprobe_seconds` | このテーブルが存在すれば `900`。`0` で無効 | 陳腐化した近接クォータの Codex/ChatGPT アカウントに対する日和見的な再プローブ間隔（秒）。60 未満の正の値は 60 秒の下限に切り上げられます。`0` で再プローブは無効。`[server.pool]` 自体が存在しない場合、この値に関係なく再プローブは無効（issue #135 以前の挙動）。WebSocket を使わない outbound Responses 選択とオプションの inbound Codex HTTP エンドポイントは再プローブを維持し、WebSocket 有効時の outbound 選択では無効 |

各ウィンドウ `X` について、有効なソフトしきい値は次の順で解決されます: アカウントの `threshold_X` → アカウントの `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`。これは `hard_threshold` を上限としてクランプされます。すべてのしきい値は `[0.0, 1.0]` の使用率の割合であり、範囲外の値は起動時にエラーになります。しきい値とバーンレートのノブは両方のプールファミリーを制御します: Anthropic プールは `anthropic-ratelimit-unified-*` ヘッダーから、Codex/ChatGPT プールは `x-codex-*` の 5 時間／週次ウィンドウから制御されます（Codex には Fable スコープの `7d_oi` ウィンドウがないため、そこでは `default_threshold_fable` は無効です）。`usage_refresh_seconds` は `claude_oauth` アカウントだけでなく、非公式の `wham/usage` エンドポイント経由で Codex/ChatGPT バックエンドの `chatgpt_oauth` アカウントもポーリングします。

正の `usage_refresh_seconds` は追加でバックグラウンドポーラーを起動し、各ファミリーの usage API と突き合わせてアカウントプールのクォータ状態を補正します: `claude_oauth` アカウントは公式の Anthropic OAuth usage API と、Codex/ChatGPT バックエンドの `chatgpt_oauth` アカウントは非公式の `wham/usage` エンドポイントと突き合わせます。未設定または `0` で無効（デフォルト）です。ポーリングされるのはどちらのファミリーも imported（更新可能）なアカウントのみで、長期の `claude setup-token` や、どちらのファミリーであれ `token_env` アカウントは、usage エンドポイントが更新不可トークンを拒否するためスキップされます。Claude のポーラーは報告されたウィンドウの使用率、ウィンドウ固有のリセット時刻、使用率の観測時刻を更新します。ウィンドウ別および集約 status の鮮度と、status の観測時にキャプチャしたリセット境界だけがヘッダー由来のままで、shunt の外での同一アカウントの消費まで含む権威ある使用量と突き合わせても status の寿命は延長しません。Codex のポーラーは使用率と使用率の観測時刻を更新し、リセットメタデータはレスポンス由来（`x-codex-*` ヘッダーと WebSocket の `codex.rate_limits` イベント）、status メタデータはヘッダー由来のままです。報告されたウィンドウでは、未来の保存済みリセットを保持し、経過した保存済みリセットだけを新しい使用率を書き込む前にクリアします。wham の `reset_at` は実際のリセットメタデータとして採用しません。非公開スキーマは lenient かつ fail-soft に解析され、間隔は起動時に固定され、設定のリロードではポーラーの起動・停止・再調整は行われません。

`state_path` はプールのクォータ状態（すべてのプロバイダーのアカウントについて、ウィンドウごとの使用率と各ウィンドウ固有のリセット時刻、使用率と status の独立した観測時刻およびキャプチャ済み status のリセット境界）をディスクに保存します。設定しない場合、再起動は空のプールから始まり、各アカウントは再起動後の最初のレスポンスまで未観測に見えるため、burn-rate 回避が無効になり、トラフィックでプールが再充填されるまで `GET /usage` は空を返します。このファイルは権威あるソースではなくベストエフォートのキャッシュです — クォータはいずれにせよアップストリームのレスポンスから再導出されるため、ファイルが欠落・陳腐化・破損していてもコールドスタートになるだけで、起動失敗にはなりません。書き込みは非公開の temp ファイル（Unix では `0600`）を対象にアトミックにリネームする方式で、クォータが変化したときだけバックグラウンドタイマーで行われます。書き込みに失敗した場合は次の tick で再試行します。クールダウンは保存されず（再起動で失効）、復元されたウィンドウのうちすでにリセットを過ぎたものは、復元時の import 中に最初の選択または snapshot より前に破棄されます。使用率は自身の観測時刻による上限と、そのウィンドウのリセットの早い方で失効し、上限だけが過ぎた場合はそのウィンドウの未来のリセットが残ります。status は自身の観測時刻による上限と観測時にキャプチャした status リセット境界の早い方で失効し、キャプチャした境界も status とともに消去されます。バージョン2のファイルは明示的な移行経路でバージョン3に書き直され、バージョン3のリセットなし status はリセットのみの更新後もリセットなしのままです。パスは起動時に固定され、設定のリロードでは永続化の開始・停止・パス変更は行われません。

正の `ramp_initial_concurrency` は、すべてのアカウントプールで**ストーム制御**（storm control）を有効にします。フェイルオーバーの切り替え後、そうしなければ進行中の並行リクエストがすべて切り替え直後のアカウントに一度に着地してしまいます。ゲートを有効にすると、トラフィックを受け始めたばかりのアイデンティティ（新規、クールダウンから復帰、または 60 秒アイドル）は、設定された数までの並行リクエストしか受け入れません。成功レスポンスごとに許容量が倍増し（スロースタート）、フェイルオーバーに値する失敗はランプをリセットし、拒否されたリクエストは選択順で次のアカウントに回されます。最後に残った候補はゲートに関係なく常に試行されるため、ゲーティングはリクエストを遅延させることはあっても、ゲートなしのプールなら処理できたリクエストを失敗させることは決してありません。これは、プールのすべてのアカウントが単一のアップストリームアイデンティティに解決される場合、実質的にゲートなしと同じであることも意味します。唯一の候補は常に最後の候補でもあるため、この設定は異なるアカウントアイデンティティが 2 つ以上あるときにのみ効果を持ちます。

`reprobe_seconds` は、帯域外の usage ポーラーが無効または次のポーリングを待つ Codex/ChatGPT プールのための安全網です。rotation の代表アカウントが Codex/ChatGPT ファミリーで、近接クォータで、クールダウン中でなく、最新の観測がこの間隔より古い場合、間隔ごとに 1 回だけ選択順の先頭に昇格され予約されます。鮮度は 4 つの論理値で判定します。5h、共有 7d、Fable 7d_oi では、それぞれ使用量観測と status 観測の新しい方を使い、4 つ目には独立した aggregate status 観測を使います。使用量だけのポーリングは使用量の鮮度だけを更新し、各ウィンドウの status 鮮度は更新しません。admission または認証情報の解決に失敗すると予約を取り消し、最初の実際の HTTP 送信時にプローブ時刻と `shunt.pool.reprobes` をコミットします。次の実際のリクエストがそのアカウントのクォータを更新するため、遠い将来の週次リセットまでアカウントが除外されたままになることを防ぎます。対象は Codex/ChatGPT アカウントのみです。Claude と Kimi は一般的な 429 拒否に対してより遅いクールダウン復帰（`PauseSame`、最大 5 分）を使うため、日和見的なプローブは実際のリクエストを停滞させるリスクがあり、Claude アカウントには代わりに上記の `usage_refresh_seconds` があります。設定されたポーラーが早期復旧を提供するのは imported かつ更新可能な `chatgpt_oauth` アカウントだけで、ポーラーがない場合や対象外のアカウントでは outbound マークは観測時刻に基づくウィンドウ寿命の上限で期限切れになります。再プローブは、帯域外のメタデータポーリングである `usage_refresh_seconds` と異なり、昇格のたびに実際のアップストリームリクエスト 1 回分のトラフィックコストがかかります。プロバイダーの WebSocket 転送が有効な場合、outbound Responses プールは予約を作らず再プローブを抑止します。オプションの inbound Codex HTTP エンドポイントは引き続きプローブし、そのプロバイダーの `shunt.pool.reprobes` は inbound プローブだけを数えます。

## `[[upstreams]]`（順序付きフェイルオーバー）

`[[upstreams]]` は、名前付きアップストリームの順序付き配列です。宣言順がグローバルなフェイルオーバー順となり、モデルの `[models.upstream_model]` マップが参加するエントリを選択します。マップ内の記述順はルーティングに影響しません。

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "kimi-overflow"
provider = "kimi"

[[upstreams]]
name = "codex-fallback"
provider = "codex"

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
kimi-overflow = "kimi-k2"
codex-fallback = "gpt-5.2"
```

この例では `anthropic-primary`、`kimi-overflow`、`codex-fallback` の順に試行します。モデルマップにないアップストリームは参加しません。

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `name` | はい | 空でない一意のアップストリーム名。ルート、モデルマップ、`server.default_provider`、メトリクス、管理画面で使われます。 |
| `provider` | `kind` と `base_url` を設定しない場合 | 組み込み preset。`kind`、`base_url`、デフォルト auth を提供します。明示したフィールドは preset 値を上書きします。 |
| `kind` | preset がない場合 | `anthropic`、`responses`、`cursor`、`gemini`、`antigravity`、`antigravity_cli`。後者 3 つは下記の preset 表に項目がないため（組み込みの `[providers.gemini]`、`[providers.antigravity]`、`[providers.antigravity-cli]` テーブルは preset ではなく、別建てのレガシー方式です）、順序付き upstream では `kind` を明示的に指定する必要があります。CLI provider のテーブル名はハイフンの `antigravity-cli` ですが、`kind` 値はアンダースコアの `antigravity_cli` です。 |
| `base_url` | preset がない場合 | アップストリームの base URL。`kind = "cursor"` ではログイン／トークン更新用エンドポイントにのみ使われます。推論は固定のエージェントホスト `https://agentn.global.api5.cursor.sh` を使用し、`SHUNT_CURSOR_AGENT_BASE_URL` でのみ上書きできます。 |
| `auth` | いいえ | auth mode の文字列、または mode 固有のマップ。デフォルトは preset の auth、preset もなければ `passthrough`。 |
| `effort`, `classifier_model`, `count_tokens`, `websocket`, `tool_search`, `request_compression`, `retry` | いいえ | レガシー provider と同じアップストリーム単位の設定。preset は `count_tokens` を上書きしません。Cursor アップストリームでも `retry` は正規化されますが、Cursor のストリーミングターンには適用されません。 |

利用可能な preset は次のとおりです。

| Preset | Kind | Base URL | デフォルト auth |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `https://api.anthropic.com` | `passthrough` |
| `codex` | `responses` | `https://chatgpt.com/backend-api` | `chatgpt_oauth` |
| `openai` | `responses` | `https://api.openai.com/v1` | `api_key`, env `OPENAI_API_KEY` |
| `xai` | `responses` | `https://api.x.ai/v1` | `api_key`, env `XAI_API_KEY` |
| `grok` | `responses` | `https://cli-chat-proxy.grok.com/v1` | `xai_oauth` |
| `kimi` | `anthropic` | `https://api.moonshot.ai/anthropic` | `api_key`, env `MOONSHOT_API_KEY` |
| `cursor` | `cursor` | `https://api2.cursor.sh` | `cursor_oauth` |
| `kimi-code` | `anthropic` | `https://api.kimi.com/coding` | `kimi_oauth` |
| `zhipu` | `anthropic` | `https://open.bigmodel.cn/api/anthropic` | `api_key`, env `ZHIPUAI_API_KEY` |
| `minimax-cn` | `anthropic` | `https://api.minimax.cn/anthropic` | `api_key`, env `MINIMAX_API_KEY` |
| `opencode` | `anthropic` | `https://opencode.ai/zen` | `api_key`, env `OPENCODE_API_KEY`, ヘッダー `x_api_key` |

`auth = "claude_oauth"` のような文字列は `auth = { mode = "claude_oauth" }` の省略形です。`api_key` マップは `env`（preset が提供しない場合は必須）と `header` を受け取ります。`header` を省略するとデフォルト（`bearer`、または `opencode` preset の `x_api_key`）がそのまま使われます。`claude_oauth` と `chatgpt_oauth` のマップは `account = "name"` または `accounts = [...]` で範囲を絞れますが、両方は指定できません。`accounts` にはストアエントリ名の文字列と完全なアカウントテーブルを指定できます。明示的な `accounts = []` は拒否され、両方のスコープフィールドを省略するとストア全体を走査します。ChatGPT ストアが空の場合、`chatgpt_oauth` は従来どおり `~/.codex/auth.json` にフォールバックします。`passthrough`、`xai_oauth`、`cursor_oauth`、`antigravity_oauth` のマップは `mode` のみを受け付け、mode 固有の未知のキーはエラーです。

設定ファイル内で `[[upstreams]]` と `[providers.*]` を混在させないでください。ファイル層に両方の宣言形式があると起動に失敗します。環境変数はどちらの形式でも、正規化後のアップストリーム／provider 名を指定する `SHUNT_PROVIDERS__<name>__<field>` により個々のフィールドを上書きできます。順序付き `[[upstreams]]` 配列そのものは、1 つの環境変数で合成しようとせず、設定ファイルで宣言してください。レガシー `[providers.<name>]` は引き続きサポートされ、名前順の暗黙的アップストリームに正規化されます。この形式はフェイルオーバー順を宣言しないため、モデルマップは 0 または 1 エントリだけをサポートします。モデルマップに複数エントリを追加する前に `[[upstreams]]` へ移行してください。

### フェイルオーバー動作

複数エントリのモデルマップでは、宣言済みアップストリーム列からマップ内の名前だけを残してチェーンを構成します。アップストリームのステータスが `429`、`401`、`403`、`404`、任意の `5xx` の場合、またはアップストリームのレスポンスヘッダーを受け取る前に失敗した場合は、次のエントリへ進みます。auth の設定不備やアダプター自身の検証・ヘッダー構築エラーなど、アップストリーム試行を表さないゲートウェイローカルエラーは直ちに返し、設定問題をフェイルオーバーで隠しません。`2xx` ヘッダーを返した後は、その後ストリーミング本文が失敗してもフェイルオーバーしません。Responses アダプターのストリーミング経路はアップストリームのバイトを受信する前にレスポンスをコミットします。`Anthropic`/`Responses` 要素のみで WebSocket トランスポートを使わないチェーンは、コミット済みストリーム内でフェイルオーバーを実行します（ヘッダー前のトランスポート失敗と前進ステータスは合成開始が送られる前に次のアップストリームを試行）が、前進できないルート（終端の非 2xx、Anthropic 種の勝者からの SSE ではない成功ボディ）は失敗を 1 つのターミナル SSE `error` イベントとして通知します。勝者のターミナルフレームが中継された後のストリーミング本文の失敗は、代わりにストリームを静かに終了します — ターンはすでに完了しており、後から付く error イベントは完了済みの応答を壊すためです。TTFB タイムアウトは決して前進しません。設定済みタイムアウトは回答であり、ターミナルの `504 timeout_error` イベントとして通知されます。このコミット済み経路のレスポンスには `content-type` と `x-gateway-model` が載り、`[models.router]` エントリがルーティングしたリクエスト、または `[models.subagents]` オーバーレイが振り向けたリクエストならルーターの 2 つのヘッダー（`x-gateway-routed-model`/`x-gateway-route-source`）も載ります — 最初の試行より前に決まる値なので、どのアップストリームが勝つかには依存しません。勝者に依存する `x-gateway-upstream` と `x-gateway-upstream-model` は省略され — ヘッダーがコミット時に送信される時点では勝者が不明だからです — アップストリームのレスポンスヘッダー（リクエスト id や `anthropic-ratelimit-*` のクォータメタデータを含む）は、Anthropic 種の勝者であってもクライアントに届きません。`x-gateway-model` は残ります（クライアントが要求した id を示します）。リクエストメトリクスは試行ごとに分類済みステータスを記録し、ストリームの帰属はストリームが勝者を知った時点で勝者に従います。

チェーンを使い切ると、`429` → `401`/`403` → `404` → その他の `5xx` の優先順位で、最適な中継済み失敗を返します。ヘッダー前の失敗は最終候補として記憶しません。記憶した中継レスポンスがなければ、`all upstreams failed (N attempted)` というメッセージの `502 api_error` を返します。

`passthrough` アップストリームでは、クライアント自身の `authorization` / `x-api-key` がフェイルオーバー試行で転送されるのは、**プライマリ**ルート自体が `passthrough` であり、かつ試行先のオリジンがそのプライマリと一致する場合に限られます。このときの資格情報はプライマリにオリジン固有なクライアント自身のアップストリーム資格情報であるため、**異なる**オリジンへの `passthrough` フェイルオーバー試行ではこれを削除してフェイルクローズし、ホスト固有のトークンを別のオリジンへ再送しません。同一オリジンのフォールバック（例：1 つのホスト上の 2 つの passthrough エントリ）は引き続き資格情報を保持します。プライマリが自前の資格情報を注入する場合、クライアントのヘッダーはアップストリーム資格情報ではなくゲートウェイ／クライアントのシークレットであるため、すべての `passthrough` フォールバックはオリジンに関係なくこれを削除します。`api_key`／OAuth アップストリームは位置に関係なく自前のサーバーサイド資格情報を注入します。

origin に関係なく、保持された各スロットはそのスロットが実際に保持している値でもチェックされます。`authorization` と `x-api-key` は、そのスロット自身の値が shunt 自身が発行した JWT と**形が一致する**場合 — 3 セグメント構造で、ペイロードの `aud` クレームが `"shunt"` であるか、`iss` クレームがこのゲートウェイのアイデンティティと一致するか、`shunt_token_use` クレームが `"gateway-session"`（shunt だけが発行する専用マーカー）である場合 — または設定済みの `[server.auth]` クライアントトークンと一致する場合にのみクリアされます。この JWT チェックは意図的に「今このトークンが認証されるか」ではなく「形が一致するか」で判定します: 期限切れのトークン、別の `public_url` を持つ兄弟インスタンスが発行したトークン、`jwt_secret` のローテーション後に検証できなくなったトークンも、依然として shunt 自身の認証情報であるため引き続きクリアされます。このマーカーは形状チェックに追加された分岐であり、必須条件ではありません: マーカー導入前に発行されたトークンも `aud`/`iss` で引き続き一致し、`verify` 自体もマーカーを要求しないため、古いバージョンの shunt が発行したトークンは TTL 内であれば引き続き認証されます。`apiKeyHelper` は両方のスロットを同じ値で埋めるため、どちらの認証情報も一方または両方のスロットに入り得ます。もう一方のスロットがゲートウェイ JWT や静的なクライアントトークンを保持していても、本物のアップストリーム認証情報を保持しているスロットはそのまま転送されます。クリアされるのはゲート用認証情報を保持しているスロットだけです。`[server.auth] header` には `authorization` 自体を含め任意のヘッダー名を指定でき、そう設定した場合クライアントはプレフィックスなしの `Authorization: <token>` で認証します。そのためこのスロットは `Bearer` ペイロードだけでなく値全体としてもチェックされ、そうしたトークンがアップストリームへ転送されることはありません。 この設定には注意点があります: 推論リクエストでは shunt がルーティング前に設定されたヘッダーを無条件に除去するため、そのスロットは上流へ何も運びません — ゲートトークンだけでなく、呼び出し元自身の認証情報も落ちます。`header` を既定の専用 `x-shunt-token` のままにすればこの衝突を避けられます。

プロキシされた成功レスポンスと最終失敗には、`x-gateway-upstream`（選択したアップストリーム名）、`x-gateway-model`（クライアントが要求した id）、`x-gateway-upstream-model`（マッピング後のバックエンド id）が必ず含まれます — コミット済みストリーミングチェーン経路は例外で、レスポンスには `content-type` と `x-gateway-model`、そしてルーターがルーティングしたリクエスト、またはオーバーレイが振り向けたリクエストなら後述のルーターの 2 つのヘッダーが載り、勝者に依存する `x-gateway-upstream` と `x-gateway-upstream-model` は省略され、アップストリームのレスポンスヘッダーはクライアントに届きません。[`[models.router]`](#modelsrouterオプション) エントリがルーティングしたレスポンスには、さらに `x-gateway-routed-model`（ルーターが選んだターゲット）と `x-gateway-route-source`（それを選んだ理由）が付きます。[ステージルーター](/ja/guides/stage-router/)だけでなくすべてのルーター `type` に付きます。[`[models.subagents]`](#modelssubagentsオプション) オーバーレイが振り向けた委譲ターンにも同じ 2 つが付き、その `x-gateway-route-source` は `subagent_type` または `subagent` です。両方とも付かないのは、ルーターもオーバーレイもそのターンを決定しなかった場合だけです。`count_tokens` はチェーンの最初の要素だけを使い、フェイルオーバーせず、この 2 つのヘッダーも付けません。`[server.codex_endpoint]` は `[[server.codex_endpoint.routes]]` のエントリがないモデルについては設定された単一アップストリームに固定され、いずれにせよこのチェーンには参加しません。

### 既存設定の移行

既存設定に**変更は不要です**。レガシー provider のルーティングと名前順の選択動作は維持されます。アップグレード時には、次の 3 つの追加または意図された動作変更があります。

1. 同じ物理 OAuth アカウントへ解決されるレガシー provider は、クォータウィンドウ、health、cooldown、refresh lock、in-flight admission 状態を共有するようになります。プール永続化キーのスキーマバージョンが上がり、バージョン2のクォータキャッシュは使用率と status の鮮度を分離したバージョン3へ一度移行されます。
2. すべてのプロキシレスポンスに、上記 3 つの `x-gateway-*` metadata ヘッダーが追加されます。
3. Anthropic Messages ルート（`/v1/messages`）では、Claude または Codex OAuth プールのサイズにかかわらず、すべての試行がレスポンスヘッダー前に失敗すると、プール固有の `all Claude OAuth accounts failed before receiving an upstream response` または `all Codex OAuth accounts failed before receiving an upstream response` の代わりに `all upstreams failed (N attempted)` を返すようになりました。別の `[server.codex_endpoint]` インバウンド経路は影響を受けず、Codex 固有のメッセージを維持します。

順序付きフェイルオーバーを採用するには、各 `[providers.<name>]` テーブルを同名の `[[upstreams]]` エントリへ書き換え、`api_key_env`、`api_key_header`、OAuth `accounts` を `auth` マップへ移し、優先順に並べ、モデルの `upstream_model` マップへ参加する各名前を追加します。

`kimi` preset は `MOONSHOT_API_KEY` を読み取ります。`api_key_env = "KIMI_API_KEY"` を明示していた古い例はレガシー形式で引き続き動作し、アップストリームでも `auth = { mode = "api_key", env = "KIMI_API_KEY" }` と明示すれば従来の名前を維持できます。preset のデフォルトに依存するユーザーだけが `MOONSHOT_API_KEY` を export する必要があります。

## `[providers.<name>]`（レガシー）

各プロバイダーは、あなたが選んだ名前の下のテーブルです。組み込み（`anthropic`、`openai`、`codex`、`xai`、`grok`、`cursor`、`gemini`、`antigravity`、`antigravity-cli`）は部分的にオーバーライドできます — 設定マップはディープマージします。

| キー | 値 | 意味 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` \| `gemini` \| `antigravity` \| `antigravity_cli` | 上流プロトコル / アダプター。`anthropic` = Messages API（パススルー、オプションで再キー付け）。`responses` = Anthropic Messages を OpenAI Responses API へ変換（Responses API には `stop` パラメーターがないため、`stop_sequences` は黙って捨てられるのではなく、変換内でゲートウェイ側からエミュレートされます）。`cursor` = ネイティブな Cursor ConnectRPC/protobuf AgentService アダプター。`gemini` = Anthropic Messages を Google Code Assist バックエンドの Gemini `generateContent`/`streamGenerateContent` へ変換。`antigravity` = Google Antigravity バックエンドに HTTP で接続。`gemini` と同じ Code Assist プロトコルを話しますが、Antigravity のサブスクリプショントークンで認証し、プロジェクト探索では `ideType: ANTIGRAVITY` として自身を識別します。`antigravity_cli` = **非推奨** — 上流を持たず、ローカルの Antigravity CLI バイナリ（`agy`）をサブプロセスとして実行。`agy` が自身のツール呼び出しを解決し、`tool_use` ブロックを返せないため、実際にツール呼び出しを要求するリクエスト（空でない `tools` 配列、または `any`・`tool` の `tool_choice`）は、テキストとして黙って応答するのではなく `400 invalid_request_error` で拒否されます。`tool_choice: none`（`tools` と併用していても）、ツールのない `tool_choice: auto`、空の `tools: []` はいずれもツール呼び出しを強制しないため受け付けられます。 |
| `base_url` | URL | 上流のベース。shunt がエンドポイントパスを追加します。`kind = "cursor"` ではログイン／トークン更新用エンドポイントにのみ使われ、エージェント／推論ホストは選択しません。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` \| `google_oauth` \| `antigravity_oauth` \| `none` | `passthrough` はクライアント自身の credential を転送。`api_key` は `api_key_env` からキーを注入。`chatgpt_oauth` は `~/.codex/auth.json` を再利用。`claude_oauth` は明示的な Anthropic アカウントから選択。`xai_oauth` は `shunt login xai` からの `~/.shunt/xai-auth.json` を再利用（HTTPS 上の x.ai/grok.com ホストへのみ送信）。`cursor_oauth` は `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用。`google_oauth` は gemini CLI ログインの `~/.gemini/oauth_creds.json` を再利用し、`kind = "gemini"` でのみ有効。`antigravity_oauth` は `shunt login antigravity` からの `~/.shunt/antigravity-auth.json` を再利用し、`kind = "antigravity"` でのみ有効で、`google_oauth` とは**互換性がありません** — Antigravity は Gemini CLI のトークンには含まれない 2 つのスコープ（`cclog`、`experimentsandconfigs`）を要求します。`none` は認証すべき上流を持たないアダプター（`kind = "antigravity_cli"`）向けに、credential を一切送信しません。 |
| `api_key_env` | 環境変数名 | `auth = "api_key"` のとき、キーを読み取る場所。この値自体も `${VAR}` / `${file:...}` で書けます([Secret 参照](#secret-参照)を参照)。 |
| `api_key_header` | `bearer`（デフォルト） \| `x_api_key` | 注入されたキーを送るヘッダー。 |
| `effort` | `low` … `max` | オプションのデフォルト reasoning エフォート（`responses` プロバイダー）。`kind = "antigravity"` にも適用され、サフィックスのない `gemini-*` の `upstream_model` にカタログの effort サフィックスとして付与されます。 |
| `count_tokens` | `tiktoken`（デフォルト） \| `estimate` | `responses` および `cursor` provider: ローカルの tiktoken カウント vs. `501 not_supported` フォールバック（[詳細](/ja/guides/effort-and-context/#トークンカウントcount_tokens)）。 |
| `classifier_model` | モデル id | `anthropic` provider 専用。Claude Code のオートモード権限分類器リクエストが使う上流モデル。対象はリクエストの形だけで判定され、それ以外のリクエストはクライアントが要求したモデルのままです。**この provider 内での**差し替えであって、別の provider へのルートではありません — このキーは `anthropic` の上流でのみ受け付けられます。デフォルトは未設定。[Anthropic → オートモードの分類器](/ja/providers/anthropic/#オートモードの分類器) を参照。 |
| `tool_search` | 未設定（「auto」、デフォルト） \| `true` \| `false` | gpt-5.4+ モデルかつフレーバーが xAI/Grok でない場合に、Claude Code のツール検索へネイティブなクライアント実行 `tool_search` プロトコルを使う。未設定時は、すでに動作確認済みのホスト — ChatGPT/Codex バックエンドと `api.openai.com` — でのみネイティブがデフォルトになり、LiteLLM・vLLM・OpenRouter・自前ホストのプロキシなど他のすべての OpenAI 互換エンドポイントはテキストベースのシムのまま。検証済みのカスタムエンドポイントをネイティブへオプトインするには `true`、常にシムを強制するには `false` を設定する。[Codex → ツール検索](/ja/guides/codex/#ネイティブプロトコル) を参照。 |

名前だけのエントリーは、`shunt login claude --name <name> --mode oauth|import|setup-token` で作成した `~/.shunt/accounts/claude/<name>.json` を読み取ります。対話型 CLI はこの 3 つの mode を提示し、リフレッシュ可能な OAuth を推奨します。`--long-lived` は `--mode setup-token` の deprecated alias です。`SHUNT_CLAUDE_ACCOUNTS_DIR` でストアディレクトリを上書きできます。リフレッシュ可能な OAuth/import ファイルは provider が refresh token をローテーションすると同じ場所に更新されるため、ファイルごとに稼働中の owner は 1 つだけにしてください。複数の shunt プロセスで共有したり、独立してコピーしたりしないでください。プロセスごとに個別にプロビジョニングするか、適切な場合は静的な setup token を使ってください。

## `[[routes]]`

レガシーな厳密一致ルーティングエントリ — 一致する `[models.upstream_model]` エントリの後にチェックされます。

> **レガシー:** 厳密なモデル id には、`[[models]]` エントリと `[models.upstream_model]` の使用を推奨します。1つの信頼できる情報源で id のルーティングと公開を同時に行えます。`[[routes]]` は今後もサポートされますが、推奨する厳密ルーティング形式ではありません。

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `model` | ✅ | Claude Code が送る正確な `model` id |
| `provider` | ✅ | 設定済みアップストリーム名 |
| `upstream_model` | — | 上流へ転送するモデル id を書き換える |
| `effort` | — | ルート単位の reasoning エフォートオーバーライド。`antigravity` のルートでは、サフィックスのない `gemini-*` の `upstream_model` に合成される effort サフィックスを固定します。 |

## `[[route_prefixes]]`

プレフィックス一致のルーティングエントリ — 厳密ルートの後にチェックされます。

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `prefix` | ✅ | モデル id のプレフィックス、例 `gpt-` |
| `provider` | ✅ | 設定済みアップストリーム名 |

## `[[models]]`

[model discovery](/ja/guides/model-discovery/) 向けに `GET /v1/models` が返すエントリ。id は `claude` または `anthropic` で始まる必要があります。さもないと Claude Code が無視します。

トップレベルの `auto_include_builtin_models` キーはデフォルトで `true` です。有効な場合、shunt は管理者が選定した `[[models]]` エントリを先に返し、その後に shunt 自身が検出したモデルを追加します。同一 id は選定したエントリを優先して重複を除きます。`[[models]]` リストだけを公開するには `false` に設定してください — 下記のアップストリーム呼び出しも同時に無効になります。

検出されるモデルは、shunt が実際のアップストリーム一覧を取得できる場合はそこから得られます。`server.default_provider` が Anthropic 種別の場合に、そのアップストリームへ `GET /v1/models` を発行し、認証モードに応じた認証情報を使います。`auth = "passthrough"` では呼び出し元が転送した認証情報を使うため、呼び出し元ごとにその認証情報で利用できる一覧が返ります。ただし、あるスロットに実際のアップストリーム認証情報ではなく shunt 自身の `[server.gateway]` JWT または設定済みの `[server.auth]` クライアントトークンが入っている場合、そのスロットは転送されません。`authorization` と `x-api-key` は個別にフィルタされるため、もう一方のスロットにある本物の認証情報はそのまま転送され、両方のスロットに転送できる認証情報が残らない場合にのみ Discovery は組み込みのスナップショットへフォールバックします。`api_key` では設定済みのキーを使います。`claude_oauth` では、推論と同じ実効アカウントセットから、解決可能かつ無効化されていない最初のアカウントを使います。このセットにはストアから検出されたアカウントが含まれ、`account_scope` の順序が適用されます。Discovery はプール選択、クールダウン、クォータの記録を行いません。そのため、ゲートウェイ所有の認証情報を使う後者 2 つのモードでは、すべての呼び出し元がその認証情報にスコープされたカタログを共有します。shunt はキャッシュしません。`server.default_provider` が Anthropic 種別ではない、認証情報がない、あるいは呼び出しが失敗・タイムアウト（2 秒上限）した場合は、組み込みの Claude カタログのスナップショットにフォールバックします。いずれの場合もこれらの id は専用の `[[routes]]` エントリを必要としません。通常のルーティング規則で解決され、`[[routes]]` と `[[route_prefixes]]` のいずれにも一致しない場合は `server.default_provider` にフォールバックします。

選定したエントリに `[models.upstream_model]` を追加すると、1つの宣言で id の公開、ルーティング、上流 id への変換を行えます。厳密な id のルーティングには、`[[routes]]` の代わりにこの形式を推奨します。順序付き `[[upstreams]]` では、マップに 1 つ以上の `upstream = "backend-id"` ペアを含めることができ、`[[upstreams]]` の宣言順でフェイルオーバーチェーンになります。レガシー `[providers.*]` には宣言済み順序がないため、正確に 1 ペアだけを許可します。その id ではマップが `[[routes]]`、`[[route_prefixes]]`、`server.default_provider` より優先され、各アップストリームのデフォルト `effort` がそのチェーン要素に適用されます。空のマップ、空または空白文字のみのアップストリーム名またはバックエンド id、未知のアップストリーム、同じ id の `[[routes]]` エントリ、`[1m]` または `[1M]` で終わるマップ付き id、あるいはいずれか一方がマップ付きである重複 `[[models]]` id は起動エラーです。client はマッチング前に context-window hint を取り除くため、マップ付き id にこの suffix を含めると、そのエントリには到達できません。マップなしエントリ同士の重複は従来の動作を維持しますが、いずれかが `[models.router]` テーブルを持つ場合は例外です — 下記を参照してください。

```toml
[[models]]
id = "claude-opus-4-8"
display_name = "Claude Opus 4.8"

[models.upstream_model]
codex = "gpt-5.2"
```

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `id` | ✅ | Claude Code に公開されるモデル id |
| `display_name` | — | `/model` ピッカーに表示されるラベル |
| `upstream_model` | — | 設定済みアップストリーム名からバックエンドモデル id へのマップ。順序付き `[[upstreams]]` は複数エントリのフェイルオーバーチェーンを許可し、レガシー provider は 1 エントリだけを許可 |

### `[models.router]`（オプション）

広告する id ひとつに対するリクエスト単位のルーティングです。宛先をひとつ指定する代わりに
`[models.router]` テーブルを置き、その `type` キーがルーティングアルゴリズムを選び、
アルゴリズムが宛先を選びます。このテーブルがなければ `[[models]]` エントリは従来どおりに
動作し、どこにもルーターを設定しなければルーティングは変わりません。

shunt が通常使う `kind` や `mode` ではなく `type` を使うのは、**shunt 自身の命名規約に対する
意図的な例外**であり、リファレンスでそう明記するのはこの 1 か所だけです。ルーティング
アルゴリズムは [NVIDIA-NeMo/Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) 由来
で、キー名をそのままにしておけば、上流のスキーマ文書と `type` の値を訳し直さずにそのまま
持ち込めます。

どのルーターが指定するターゲットも通常の公開モデル id なので、それぞれが通常のラダーで
解決され、フェイルオーバーチェーン、アカウントプール、アダプター、`effort`、
`service_tier` をそのまま保ちます。クライアントに返される id は要求された id のままで、
選ばれたターゲットはアップストリームにのみ伝わります。

| `type` | 選び方 | リクエストボディを読むか |
| :-- | :-- | :-- |
| `stage_router` | 直近の tool-result メタデータからターンごとに | 読む — `tool_use.name` と `tool_result.is_error` のみ |
| `auto` | 同じルーターを上流のプリセットで | 同上 |
| `random` | 重み付き抽選、既定ではセッション固定 | 読まない |
| `noop` | 選ばない — 空のメッセージを返す | 読まない |
| `prefill_router` | 直近のユーザーターンを読む学習済み分類器（`prefill-router` ビルドが必要） | 読む — ユーザーターンのテキスト |
| `llm_classifier` | LLM ジャッジの判定。いつ尋ねるかは `classify_trigger` が決めます。`mode = "escalation"` では、完成した効率側のターンに対するジャッジの判断 | 読む — パッケージのプロンプト、または自分で書いたプロンプトでトランスクリプトを読みます |
| `composite` | LLM ジャッジがステージルーターの fall-open ティアを決めます | 読む — ジャッジはトランスクリプトを、シグナルは tool-result メタデータを |
| `advisor` | 1 つの実行モデルがすべてのターンを提供し、より強力なレビュアーがその締めくくりのターンを承認するか差し戻します | 読む — レビュアーのためにトランスクリプトを |

ルーティングを決めている最中にターンを提供する形はふたつあります。`llm_classifier` の
[`mode = "escalation"`](#mode--escalation) と [`type = "advisor"`](#type--advisor) です。
どちらも判定が出るまでターンを留め置いてから提供するため、クライアントがストリーミングを
求めた応答を shunt がバッファリングする唯一のルートです — [留め置くターン](#留め置くターン-escalation-と-advisor)を
参照してください。`prefill_router` は実装済みですが**コンパイル時にゲート**されており、既定で無効な
`prefill-router` カーゴフィーチャーを有効にしてビルドしたバイナリでのみ利用できます —
[後述](#type--prefill_router)。

同じエントリに `[models.router]` と `[models.upstream_model]` を併記することはできません。

#### `type = "stage_router"`

コンテンツ認識のティア選択です。エントリが**2 つ**（強力なティアと効率的なティア）を指定
し、リクエストの直近の tool-result 履歴にターンごとの選択を任せます。シグナルとヒステリ
シスの仕組みは[ステージルーターガイド](/ja/guides/stage-router/)を参照してください。

```toml
[[models]]
id = "claude-auto"
display_name = "Auto (stage router)"

[models.router]
type = "stage_router"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `stage_router` |
| `capable_target` | ✅ 必須 | 難しい推論・調査・エラー復旧を担当するモデル id |
| `efficient_target` | ✅ 必須 | 計画が固まった後の定型作業を担当するモデル id |
| `picker` | `efficient_first` | シグナルが決め手に欠ける場合に使うティア。`efficient_first` または `capable_first` |
| `confidence_threshold` | `0.5` | シグナルに基づいて判断するための最小スコアラー信頼度、`(0.0, 1.0]` |
| `recent_turn_window` | `3` | スコアラーに渡すアシスタントのツール結果ターン数。最小 `1` |
| `min_dwell_turns` | `3` | 下降が発火できるようになるまでティアを保持するターン数。ティアを選んだターンから数えるため、`0` と `1` はどちらも下限なしを意味します |
| `deescalate_threshold` | `0.75` | ティアを*下げる*ために必要な信頼度。既定値は `confidence_threshold` の既定値より高く、下げる方向をより難しくしていますが、2 つの値はそれぞれ独立に範囲検査されるため、`confidence_threshold` より低い値も受け付け、ロード時に警告を出します |
| `session_ttl_seconds` | `3600` | 静かなセッションの固定ティアが維持される時間 |
| `capable_hold_turns` | `0` | シグナルによる上昇のあと強力なティアを保持するターン数。それらのターンはルートソース `capable_hold` として報告され、保持は証拠ではないので固定ティアをどちらの方向にも動かせません。既定値の `0` は固定の挙動を従来どおりに保ちます。上流自身の既定値は `2` です |

#### `[models.router.tool_semantics]`（オプション）

ルーター 1 つについて、shunt 組み込みの Claude Code ツール語彙を広げる 4 つのリストです。
組み込みテーブルを置き換えるのではなく、その**後に**適用されるため、テーブルが分類せずに
残した名前 — `Bash`、`Skill`、`mcp__*` のサーバーツール — にだけ届きます。組み込みテーブル
がすでに observe、mutate、plan に分類している名前（`Read`、`Edit`、`TodoWrite` など）を
指定すると**起動エラー**です。空白を含む名前（`" Read "`、`"some tool"`）も同様です。
名前は厳密に一致させるため、空白の付いた名前は実行時に何にもマッチしません。

```toml
[models.router.tool_semantics]
observe = ["mcp__jbcontext__code_search"]
mutate = []
plan = []
new = []
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `observe` | `[]` | 何も変えずに読むだけのツール名 |
| `mutate` | `[]` | 状態を変えるツール名。ファイル全体の書き込みとして採点されます |
| `plan` | `[]` | 計画または委譲を行うツール名 |
| `new` | `[]` | スコアラーが新規導入ツールとして数える名前 |

4 つのうちどれかを編集すると、次のロードでそのルーターの既存セッション固定が破棄されます。
しきい値を編集したときと同じです。

#### `[models.router.handoff_notes]`（オプション）

シグナルがティアを動かしたターンに限り、**アップストリームへ送る**リクエストにシステム
ブロックを 1 つ追加して、引き継いだモデルに理由を伝えます。

```toml
[models.router.handoff_notes]
escalation_note = "the previous model was stalling; pick up the diagnosis"
deescalation_note = "routine work resumes"
only_on_wrong_signal_escalation = true
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `escalation_note` | — | シグナルがターンを強力なティアへ上げたときに追加します |
| `deescalation_note` | — | スコアラーが作業を効率的なティアへ戻したときに追加します |
| `only_on_wrong_signal_escalation` | `true` | `escalation_note` をシグナル主導の上昇（ルートソース `override` と `dimensions`）に限定します。`false` にするとスコアラーによるすべての上昇で追加します |

ノートは `system` 配列の**末尾**に新しいブロックとして入ります。Claude Code の attribution
ブロックは先頭の要素で、触れません。固定のまま続いたターン、シグナルのないターン、
`count_tokens` プローブにはノートが付きません。引き継ぎが起きていないターン —
セッションの最初のターンと、すでに固定されているティアを確認しただけのターン —
も同じです。空のノートは起動エラーです。

**切り替えのたびにプロンプトキャッシュミスを 1 回払います。** `system` 配列はキャッシュ
されたプレフィックスの一部なので、ノートを足しても外してもプレフィックスは無効になります。
ティアの切り替え自体がすでに手放すモデル別プレフィックスに上乗せされるコストです。この
テーブルがオプトインである理由であり、`only_on_wrong_signal_escalation` の既定値が狭い側で
ある理由でもあります。

#### `[models.router.classifier]`（オプション）

シグナルだけでは決められないターンのための LLM **ジャッジ**です。このテーブルを置くと、
そのエントリはジャッジ呼び出しを行うレーンに移ります。スコアラーが決められなかったターン
— 本来ならルートソース `fall_open` として報告されるターン — のうち、ピンが押さえていない
セッションのターンで、shunt はジャッジに尋ね、その判定でルーティングします。
`type = "stage_router"` でのみ受け付けます。

```toml
[models.router.classifier]
target = "claude-haiku-4-5"
base_threshold = 0.5
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `target` | ✅ 必須 | ジャッジの公開モデル id。尋ねるだけで、クライアントに提供されることはありません |
| `base_threshold` | `0.5` | サポート対象のタスクを効率ティアに留める `p_solve` の下限。`(0.0, 1.0]` |
| `classify_trigger` | `every_request` | ジャッジを呼びうるタイミング。`every_request` は決着しなかったどのターンでも呼びます（ツールの継続ターンを含む）。`user_turn` は直近のメッセージが人間のユーザーターンのとき — `role: user` で、`tool_result` ではないブロックを少なくとも 1 つ持つとき — だけ呼びます。そのためツールの継続ターンは新たなジャッジ呼び出しを払わず、セッションのピンに乗ります。`new_session` はここでは `every_request` とまったく同じ挙動で、これは上流も同じことを述べています — このルーターは決定をすでに shunt 自身のセッションピンに保持しているからです |

ジャッジのターゲットも通常の公開モデル id で、ティアのターゲットと同じ 1 ホップ規則に従い
ます。さらに条件がひとつ加わります。**passthrough** ルートに解決されてはいけません。
`auth = "passthrough"` は*呼び出し元の資格情報をそのまま転送する*という意味ですが、
ジャッジ呼び出しが取り除くのはまさにその呼び出し元の資格情報です。そのためそうした
ターゲットは何も持たずに到達し、起動エラーになります。それ以外の auth モードはすべて
受け付けられ、`auth = "none"` も含まれます。このモードはそのエンドポイントが資格情報を
まったく必要としないという意味なので、認証なしで動くローカル・セルフホストのジャッジは
エラーではなくサポートされた構成です。呼び出し元の資格情報スロットはひとつも同行しません — 予約済みの
`x-shunt-*` スロットと `cookie`、`authorization`、`x-api-key`、`anthropic-beta` はすべて
取り除かれます。呼び出しはそのターゲット自身のアカウントプールのクォータを消費します。
ジャッジには専用の `[[models]]` エントリを与えるべきなのはこのためです。

ジャッジが決めたターンはルートソース `llm-classifier` として報告され、他の決定と同じように
セッションをピン留めします。ジャッジの失敗は種類を問わず — タイムアウト、応答サイズ超過、
アップストリームエラー、解釈できない判定、予算切れ — picker の既定値である `fall_open` に
解決されます。`count_tokens` プローブでジャッジを呼ぶことはなく、リクエストが認証と
ポリシー検査を通る前に呼ぶこともありません。ジャッジを呼ぶターンでは、インバウンド認証は
要求された id に加えてそのエントリが指定しうるすべてのターゲットとジャッジを、それぞれの
フェイルオーバーチェーン全体まで含めて対象にします。そのため passthrough の応答ターゲット
に資格情報を注入するジャッジが付くとクライアントの資格情報が必要になり、認証に失敗した
リクエストやポリシーが拒否したリクエストはジャッジ呼び出しを 1 回も行いません。ジャッジを
呼ばないターンは、実際に解決されたチェーンだけで認証されます — シグナルが自力で決めた
ターンと、[`[models.subagents]`](#modelssubagentsオプション) オーバーレイがルーターより
先に振り向けたターンです。

#### 呼び出しごとの上限

6 つのキーが、エントリが行うすべての内部呼び出しに上限を課します。これらはその呼び出しを
行うテーブルに置きます — `classifier` を持つ `stage_router`、`llm_classifier`、
`composite`、`advisor` といった driven タイプの `[models.router]` と、classifier 形式の
[`[models.subagents]`](#modelssubagentsオプション) オーバーレイ（自分の分を別に持ちます）
です。上限を超えるとアップストリーム呼び出しはキャンセルされます。各値は最低でも `1` で、
`0` はそのキーを示す起動エラーです。

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `judge_timeout_ms` | `30000` | ストリーミングしないジャッジ呼び出しのエンドツーエンド期限。ヘッダー*と*ボディの両方を覆うので、`200` を返したあと止まった応答もここで打ち切られます |
| `judge_max_response_bytes` | `65536` | 収集するジャッジ応答の最大サイズ。これを超えると `fall_open` に解決されます |
| `gated_max_bytes` | `8388608` | 保持するターンの最大サイズ。SSE フレームのバイト数、または JSON ボディ |
| `gated_idle_ms` | `60000` | 保持するターンで完成したコンテンツフレーム間に許される最大間隔。SSE のキープアライブ(`event: ping` フレームと `:` コメントフレーム)ではリセットされず、チャンク境界で分割されたフレームは分類前に再結合されます |
| `gated_max_duration_ms` | `600000` | 保持するターンの実時間上限。ヘッダーとボディの両方を覆います |
| `max_judge_calls` | `8` | 1 セッションが行えるジャッジ呼び出し数。留め置くターンそのものはジャッジ呼び出しではないので数えません |

`gated_*` の 3 つのキーは、[`escalation`](#mode--escalation) や [`advisor`](#type--advisor)
のエントリで**留め置かれる**ターン、つまり判定が出るまで shunt が手元に留めるターンに上限を
課します。それ以外のエントリには留め置くターンがないので、これらのキーが課す上限もありません。

#### `type = "llm_classifier"`

シグナルが尽きたところだけを埋めるのではなく、LLM **ジャッジ**がターン全体を決めます。
エントリにはジャッジと、ジャッジが選べる宛先と、3 つある判定の形のどれかを決める `mode` を
書きます。ここでは `capability` と `custom` を説明します。`escalation` は完成したターンを
判定するので、[別の節](#mode--escalation)で扱います。

`mode` は**必須**です。上流のスキーマは `capability` を既定値にしていますが、ここでは
そうしません。3 つのモードはまったく別の原理でルーティングし、そのうち `escalation` は
提供するターンをバッファリングするため、`mode` を省いた設定が黙ってどれかのモードとして
読まれてはなりません。

**`mode = "capability"`** — パッケージのジャッジがタスクの解決確率を返します。その値が
`base_threshold` 以上ならターンは `weak_target` へ、下回れば `strong_target` へ行きます。

```toml
[[models]]
id = "claude-judged"

[models.router]
type = "llm_classifier"
mode = "capability"
classifier_target = "claude-haiku-4-5"
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
base_threshold = 0.5
# threshold_step = 0.0
# classify_trigger = "every_request"
# message_hash_fallback = false
# recent_turn_window = 3
# max_output_tokens = 4096
# prompt = "…"
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `llm_classifier` |
| `mode` | ✅ 必須 | `capability` |
| `classifier_target` | ✅ 必須 | ジャッジの公開モデル id。尋ねるだけで提供はしません |
| `strong_target` | ✅ 必須 | ジャッジが自信を持てなかったタスクが行くモデル id |
| `weak_target` | ✅ 必須 | ジャッジが解けると見たタスクが行くモデル id |
| `base_threshold` | ✅ 必須 | それでも `weak_target` に送る解決確率の下限。`(0.0, 1.0]` |
| `threshold_step` | `0.0` | 有限かつ非負。不確実または一致しない判定には 1 回、サポート外の判定には 2 回加算されます。`base_threshold + 2 × threshold_step` は `1.0` 以下である必要があります |
| `prompt` | パッケージのプロンプト | パッケージの capability プロンプトを置き換えます。スキーマは構造化出力の設定として別に送られるため、プロンプトに `{{RESPONSE_SCHEMA}}` を含めてはならず、空白だけでもいけません |

**`mode = "custom"`** — プロンプトと JSON Schema を自分で与え、JSON Pointer が判定から
**モデルグループ名**を取り出します。そのグループの最初のモデルがターンを処理します。
`any` と `judge` は予約された必須のグループで、それ以外の名前はすべてあなたのものです。
1 つのエントリが 3 つ以上のモデルから選べるのはこのためです。

```toml
[models.router]
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
prompt = "このターンのターゲットをちょうど 1 つ選んでください。レスポンススキーマに一致する JSON だけを返してください。"
response_schema = '''
{"type": "object",
 "properties": {"target": {"type": "string", "enum": ["capable", "efficient"]}},
 "required": ["target"],
 "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `llm_classifier` |
| `mode` | ✅ 必須 | `custom` |
| `models.any` | ✅ 必須 | 選択されうるすべての宛先。他の回答グループのターゲットはすべてここにも現れる必要があり（`judge` は対象外）、欠けていると起動エラーです |
| `models.judge` | ✅ 必須 | 順序付きのジャッジ候補を 1 つ以上。尋ねるだけで提供はしません |
| `models.<名前>` | — | 自分で名付けたグループ。判定がその名前を挙げると、グループの最初のモデルが選ばれます |
| `default_target` | ✅ 必須 | 使える判定が得られなかったときのグループ。`judge` を除く設定済みのグループで、空であってはいけません |
| `prompt` | ✅ 必須 | ジャッジのシステムプロンプト。空白だけではいけず、`{{RESPONSE_SCHEMA}}` を含めてもいけません — スキーマは別に送られます |
| `response_schema` | ✅ 必須 | 内側の JSON Schema を収めた TOML 文字列。JSON オブジェクトとしてパースできる必要があり、プロバイダー側のラッパーは shunt が付けます |
| `policy` | ✅ 必須 | `{ type = "target_selector", selector = "…" }` の形。`selector` は `/target` のような、判定の内部を指す JSON Pointer です |

両モードが共通して持つキーと、6 つの[呼び出しごとの上限](#呼び出しごとの上限)は次のとおり
です。

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `classify_trigger` | `every_request` | ジャッジを走らせるタイミング。`every_request` はツールの継続ターンも含めて毎ターン判定します。`user_turn` は新しい人間のユーザーターンごとに判定し、その間のツール呼び出しではそのターゲットを保持します。`new_session` は一度だけ判定し、セッション中はそのターゲットを使い続けます |
| `message_hash_fallback` | `false` | セッション id を送らないクライアント向けに、最初のユーザーメッセージで保持のキーを取ります。`classify_trigger = "new_session"` が必要で、他のトリガーで設定すると起動エラーです |
| `recent_turn_window` | 未設定 | 設定すると、ジャッジが追加で見る直近のターン数。最低でも `1` |
| `max_output_tokens` | `4096` | ジャッジの判定に対する完了トークンの上限。最低でも `1` |

**ジャッジが答えないとき。** ジャッジ呼び出しがどのように失敗しても — タイムアウト、サイズ
超過、アップストリームエラー、`400`、解釈できない判定 — 判定なしとみなされ、ターンは
アルゴリズム自身の既定値へ行きます。`capability` モードなら `strong_target`、`custom`
モードなら `default_target` グループの最初のモデルです。判定するターン 1 回につき、ジャッジ
呼び出しは**ちょうど 1 回**、最初のジャッジ候補に対してだけ行われます。したがって失敗しても
`models.judge` をたどって再試行はしません。ターンはそのまま応答され、ルートソースは
`classifier_fail_open` で、クライアントは変わらず `200` を受け取ります。

**セッションはアルゴリズムの中にあります。** `classify_trigger` の保持は上流の状態で、
ルーターのインスタンスの中にあり、shunt はそれを設定を読み込むたびに一度だけ構築します。
ホットリロードは作り直すので、リロードすると各セッションが持っていたターゲットを忘れます —
`prefill_router` と同じ性質です。`max_judge_calls` は shunt 自身のもので、(セッション,
エージェント) ごとに数えます。だから委譲された子は親ではなく自分の予算を使います。
セッション id を送らないリクエストは追跡されないので、その呼び出し元にはこの上限が
リクエスト単位で効きます。予算を使い切ったターンはジャッジを飛ばして fail-open の
ターゲットへ行き、ジャッジ呼び出しの結果は `budget_exhausted` として記録されます。

**プローブはジャッジなしで解決します。** `count_tokens` リクエストがジャッジを呼ぶことは
なく、fail-open のターゲットで応答されます。リクエストボディのない面 — `GET /routes`、
`/v1/models` ディスカバリー、`shunt check` — も同じで、これらはルートソース
`classifier_default` として報告します。

ターゲットもジャッジも、ステージルーターと同じ 1 ホップ規則に従う通常の公開モデル id で、
ジャッジは **passthrough** ルートに解決されてはいけません。理由は[上](#modelsrouterclassifierオプション)
のとおりです — ジャッジ呼び出しは呼び出し元の資格情報をひとつも運ばないので、passthrough
ルートには動かすものが残りません。

#### `mode = "escalation"`

`llm_classifier` の 3 つ目のモードは、各セッションを効率側のターゲットで始め、作業の進み
具合をジャッジに読ませます。まだ固定（ラッチ）されていないセッションのターンは
`weak_target` で作って留め置きます。そのあとジャッジが**完成した**ターンを判定します —
予測ではなく、効率側のモデルが実際に行った作業を見ます。辞退の判定は昇格の連続回数を 0 に
戻し、昇格の判定はその回数を増やします。連続回数が `confirmations` に満たないあいだは、
留め置いた効率側のターンを提供します。`confirmations` に達するとセッションが固定されます。
そのターンの効率側の応答は捨てて `strong_target` がターンを提供し、以後そのセッションの
すべてのターンはジャッジ呼び出しもバッファリングもなく、そのまま `strong_target` へ行きます。

```toml
[[models]]
id = "claude-escalate"

[models.router]
type = "llm_classifier"
mode = "escalation"
classifier_target = "claude-haiku-4-5"
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
# prompt = "…"
# max_output_tokens = 4096

[models.router.escalation]
confirmations = 2
# recent_turn_window = 28
# window_message_chars = 500
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `llm_classifier` |
| `mode` | ✅ 必須 | `escalation` |
| `classifier_target` | ✅ 必須 | 軌跡ジャッジの公開モデル id。尋ねるだけで、提供はしません |
| `strong_target` | ✅ 必須 | セッションが固定されたあとに提供するモデル id |
| `weak_target` | ✅ 必須 | 固定前に提供するモデル id。`classifier_target` と同じ id でも構いません |
| `prompt` | パッケージのプロンプト | パッケージの軌跡ジャッジプロンプトを置き換えます |
| `max_output_tokens` | `4096` | ジャッジ判定の完了トークン上限。最低でも `1` |
| `escalation.confirmations` | `2` | 固定に必要な、連続した昇格判定の数。最低でも `1`。`1` より大きい値にはセッション id が必要です。ないと毎ターンが 0 から始まり、セッションはいつまでも固定されません |
| `escalation.recent_turn_window` | `28` | ジャッジに見せる直近のメッセージ数。最低でも `1` |
| `escalation.window_message_chars` | `500` | そのウィンドウ内のメッセージごとの文字数上限。最低でも `50` |

`[models.router.escalation]` テーブルは任意です。省くと 3 つの既定値が使われ、これは上流が
ベンチマークした設定です。6 つの[呼び出しごとの上限](#呼び出しごとの上限)は
`[models.router]` に置きます。classifier 形式の
[`[models.subagents]`](#modelssubagentsオプション) オーバーレイは引き続き
`mode = "custom"` のみです。

`classifier_target` と違い、`weak_target` は **passthrough** ルートでも構いません。効率側の
ターンはクライアント自身の応答なので、ライブのターンとまったく同じように呼び出し元の資格情報
を運びます。`count_tokens` プローブはジャッジ呼び出しも留め置く呼び出しも行わず、
`weak_target` で応答します。ジャッジ呼び出しは
`shunt.router.judge_calls{algorithm="llm_classifier"}` で数えます。

留め置いたターンをどう提供するか、結果ごとにクライアントに何が見えるか、コストがどれだけ
かかるかは[留め置くターン](#留め置くターン-escalation-と-advisor)を参照してください。

#### `type = "composite"`

ジャッジがステージルーターの fall-open ティアを決め、シグナルの採点には手を触れません。
stage テーブルは **`picker` を受け付けません** — そのティアは classifier が供給するから
です。したがってここに `picker` を書くと起動エラーです。

```toml
[[models]]
id = "claude-composite"

[models.router]
type = "composite"

[models.router.classifier]
target = "claude-haiku-4-5"
base_threshold = 0.5
classify_trigger = "user_turn"

[models.router.stage]
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
confidence_threshold = 0.5
# recent_turn_window = 3
# capable_hold_turns = 0
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `composite` |
| `classifier.target` | ✅ 必須 | ティアジャッジの公開モデル id。尋ねるだけで提供はしません |
| `classifier.base_threshold` | ✅ 必須 | それでも効率ティアに送る `p_solve` の下限。`(0.0, 1.0]` |
| `classifier.classify_trigger` | ✅ 必須 | `user_turn` は人間が話すたびにティアを選び直し、`new_session` は一度選んで保持します。`every_request` はここでは**拒否**されます — ツールの 1 ステップごとにジャッジを呼ぶコストこそ、このタイプが避けようとしているものだからです |
| `classifier.message_hash_fallback` | `false` | セッション id を送らないクライアント向けに、最初のユーザーメッセージをハッシュしてティアを保持します |
| `stage.capable_target` | ✅ 必須 | 強力なティア |
| `stage.efficient_target` | ✅ 必須 | 効率的なティア |
| `stage.confidence_threshold` | ✅ 必須 | 決定的なシグナルに必要な裏付けの度合い。`(0.0, 1.0]` |
| `stage.recent_turn_window` | `3` | シグナルを計算する対象となる直近の tool result 数。最低でも `1` |
| `stage.capable_hold_turns` | `0` | シグナル起因の昇格後に強力なティアを保持するターン数。shunt の既定は `0`、上流は `2` です |
| `stage.tool_semantics` | — | [`[models.router.tool_semantics]`](#modelsroutertool_semanticsオプション) と同じ 4 つのリストで、規則も同じです |

6 つの[呼び出しごとの上限](#呼び出しごとの上限)は、どちらのサブテーブルでもなく
`[models.router]` に置きます。classifier が届かなかったターンは
`stage.efficient_target` に fall-open します。これは上流の規則であり、プローブやボディの
ない面が報告する値でもあります。

stage 側は libsy 自身の stage ルートなので、決着したターンは classifier の決定ではなく
ステージルーターのルートソースをそのまま報告します — composite のシグナル起因のターンを、
素の `stage_router` のターンと同じように読めるということです。

#### `type = "advisor"`

1 つの**実行モデル**（executor）がクライアントに見えるすべてのターンを提供します。より強力な
**アドバイザー**（advisor）が、実行モデルの締めくくりのターン — 作業前に示す計画、または
作業を終えたという主張 — をクライアントが見る前にレビューします。APPROVE は留め置いた
ターンを送り出し、REDO はそのターンを捨てて、アドバイザーの計画とともに実行モデルを作業に
差し戻します。アドバイザーがターンを提供することはないので、クライアントが目にするのは実行
モデルの出力だけです。

```toml
[[models]]
id = "claude-reviewed"

[models.router]
type = "advisor"
executor_target = "claude-sonnet-4-6"
advisor_target = "claude-opus-4-8"
gate_trigger = "no_tool_call"
max_reviews = 1
# gate_stall_turns = 0
# gate_min_tool_results = 0
# advisor_max_tokens = 2048
# transcript_max_chars = 200000
# fail_open = true
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `advisor` |
| `executor_target` | ✅ 必須 | クライアントに見えるすべてのターンを提供します |
| `advisor_target` | ✅ 必須 | 留め置いたターンをレビューします。提供はしません |
| `gate_trigger` | `no_tool_call` | レビューを起こす条件。`no_tool_call`（実行モデルがツール呼び出しなしで終えた最初のターン）または `pattern` |
| `gate_trigger_pattern` | 未設定 | `pattern` トリガーの正規表現。アンカーなしで検索します。`pattern` では空でない値が必須で、`no_tool_call` で設定すると起動エラーです |
| `max_reviews` | `1` | セッションごとに許されるレビュー数。最低でも `1`。`x-claude-code-session-id` のないリクエストはそれ自体を 1 つのセッションとして数えるため、セッションのない呼び出し元どうしで予算を共有することはありません |
| `gate_stall_turns` | `0` | 会話にこの数のアシスタントターンがたまると、作業途中のチェックポイントとして 1 ターンをレビューします。`0` で無効 |
| `gate_min_tool_results` | `0` | `no_tool_call` のターンをレビュー対象にする前に会話に必要な tool result 数 |
| `advisor_max_tokens` | `2048` | レビュー 1 回あたりの出力トークン上限。最低でも `1` |
| `advisor_temperature` | 未設定 | レビューのサンプリング温度。未設定ならレビューのリクエストから省きます |
| `transcript_max_chars` | `200000` | アドバイザーに送るトランスクリプトの上限。長い場合は中央を削ります。最低でも `256` |
| `fail_open` | `true` | レビューが失敗したら留め置いたターンを提供します。`false` なら代わりにリクエストを `502` で失敗させます |
| `reviewer_system_prompt` | パッケージのプロンプト | APPROVE/REDO のレビュアープロンプトを置き換えます |
| `redo_feedback_prefix` | パッケージのプロンプト | 実行モデルに差し戻す REDO 計画の前に置く文言を置き換えます |

6 つの[呼び出しごとの上限](#呼び出しごとの上限)は `[models.router]` に置きます。

**どのターンを留め置くか。** ターンが `gate_trigger` に該当するかどうかは、ターンが完成して
はじめてわかります。そのため、セッションにレビューの予算が残っているあいだは実行モデルの
**すべての**ターンを留め置き、ゲートに該当しなかったターンはレビューなしで提供します。
`max_reviews` を使い切るか、そのセッションでレビューモデルの呼び出しが 3 回失敗すると（失敗した呼び出しは
レビューを払い戻します）、そのセッションの残りのターンはバッファリングなしでライブに
ストリーミングされます。

**REDO。** 留め置いたターンは、応答ヘッダーがひとつもクライアントに届く前に捨てます。捨てた
ターンとアドバイザーの計画を会話に追加し、実行モデルを再実行します。この再実行はライブで
ストリーミングされます。

`advisor_target` と違い、`executor_target` は **passthrough** ルートでも構いません。理由は
escalation の効率側ターゲットと同じです。`count_tokens` プローブはレビューも留め置く呼び出しも
行わず、`executor_target` で応答します。レビューは
`shunt.router.judge_calls{algorithm="advisor"}` で数え、`GET /routes` は `advisor_target` を
`judges` に並べます。

#### 留め置くターン: escalation と advisor

**留め置かれる**（gated）ターン — 固定前の escalation の効率側ターン、またはレビュー予算が
残るセッションの advisor の実行モデルのターン — は、先に作って留め置き、判定が出てから
はじめて提供します。これらのエントリのほかのターンと、ほかのすべてのルートのすべての
ターンは、これまでとまったく同じようにストリーミングされます。

**呼び出し元のモードを保ちます。** `stream: true` の呼び出し元の留め置く呼び出しは
ストリーミングします。SSE フレームは届いた順に保持し、ターンを提供するときはバイトどおりに
再生します。`stream: false` の呼び出し元の留め置く呼び出しはストリーミングせず、呼び出し元は
JSON メッセージを 1 つ受け取ります。ゲートが変えるのは応答を*いつ*送るかだけで、応答の形は
変えません。再生される `message_start.model` は実行モデルの id ではなくルーター自身の id で、
Anthropic の実行モデルでも OpenAI Responses の実行モデルでも同じです。そのため Claude Code の
`/model` 表示と `--resume` は要求した id を見ます。応答ヘッダーは再生が始まるときにはじめて
確定します。

**完成したターンだけを提供します。** 留め置いたターンは終端マーカーがあってはじめて提供
できます。ストリーミング呼び出しでは `message_stop`、ストリーミングしない呼び出しでは
1 つのメッセージとしてパースできる完全なボディです。切り詰められた `200` が再生されることは
ありません。ライブのストリームと同じく、ターンは `message_stop` フレームで終わります。その後に
届いたものは再生せず、その後で接続が切れても開いたままでも、ターンは打ち切られません。留め置くターンは、ターゲットの順序付きフェイルオーバーチェーンをたどります。

`x-gateway-route-source` — および `shunt.router.decisions` の `source` ラベル — が何が起きた
かを示します。

| ソース | エントリ | 意味 | 配信 |
| :-- | :-- | :-- | :-- |
| `escalation_weak` | escalation | ジャッジが効率側のターンを通しました。辞退したか、昇格の連続回数がまだ `confirmations` に満たないかのどちらかです | 再生 |
| `escalation_latch` | escalation | このターンかそれ以前にセッションが固定され、強力なターゲットがターンを提供しました | ライブ |
| `escalation_fallback` | escalation | 効率側のターンが失敗したか終端マーカーの前に切れたため、強力なターゲットがターンを提供しました。ジャッジは呼んでいません | ライブ |
| `classifier_fail_open` | escalation | 完成した効率側のターンのあとでジャッジが失敗したため、効率側のターンを提供しました | 再生 |
| `advisor_approve` | advisor | 実行モデルのターンをレビューし、承認しました | 再生 |
| `advisor_pass` | advisor | 実行モデルのターンをレビューなしで提供しました。ゲートに該当しなかった（たとえばツール呼び出しで終わるターン）か、レビューを予約できませんでした | 再生 |
| `advisor_fail_open` | advisor | 完成した実行モデルのターンのあとでレビューが失敗したため、そのターンを提供しました | 再生 |
| `advisor_redo` | advisor | レビュアーが REDO と答えました。捨てたターンは送っておらず、これは実行モデルの再実行です | ライブ |
| `advisor_exhausted` | advisor | セッションの `max_reviews` を使い切ったか、レビューモデルの呼び出しが 3 回失敗したため（失敗した呼び出しは `max_reviews` を払い戻します）、実行モデルがバッファリングなしでストリーミングします | ライブ |
| `gated_error` | 両方 | 留め置いたターンを提供できませんでした — 下記を参照 | エラー |

**何かが失敗したとき:**

| 失敗したもの | `escalation` | `advisor` |
| :-- | :-- | :-- |
| 留め置くターンが `gated_*` の上限を超えた、または終端マーカーの前に終わった | ヘッダーを送る前に捨て、強力なターゲットがターンをライブで提供します（`escalation_fallback`） | ヘッダーを送る前に捨て、リクエストは Anthropic エラー形式のゲートウェイ所有の `502` で失敗します（`gated_error`）。REDO でもフェイルオーバーの試行でもありません — アップストリームはすでに `2xx` で応答しています |
| 留め置く呼び出しのアップストリームがエラーのステータスで応答した | ライブのターンと同じく、クライアントにそのまま中継します（`gated_error`） | そのまま中継します（`gated_error`） |
| 完成したターンのあとでジャッジやレビューが失敗した — タイムアウト、大きすぎる応答やパースできない応答、アップストリームのエラー、`max_judge_calls` の使い切り | 効率側のターンを提供します（`classifier_fail_open`） | `fail_open = true` なら実行モデルのターンを提供し（`advisor_fail_open`）、`fail_open = false` ならリクエストはゲートウェイ所有の `502` で失敗します（`gated_error`） |

**コスト。** 以下はエントリごとに選んで支払うコストです。

- 留め置くターンでは、ターン全体が完成して判定されるまでクライアントは何も受け取らない
  ため、最初のトークンまでの時間が最後のトークンまでの時間になります。
- escalation は固定前のすべてのターンでジャッジ呼び出しを 1 回行います。固定されるターンは、
  捨てる効率側の呼び出しのコストも支払います。
- 捨てた効率側のターンや実行モデルのターンも、アップストリームのクォータはすでに消費して
  います。クライアント自身の応答のディスパッチなので、`shunt.requests` では
  `caller="client"` として数えます。

#### `type = "auto"`

上流のステージルータープリセットです。`picker = "efficient_first"` と
`confidence_threshold = 0.5` を使い、他のステージキーはすべて shunt の既定値になります。
`type` のほかは 2 つのターゲットしか受け付けないので、他のステージキーを設定したい場合は
`type = "stage_router"` を使ってください。これには `[models.router.classifier]` と
呼び出しごとの上限キーも含まれます — プリセットにジャッジはなく、`auto` エントリに
classifier テーブルを置くと起動エラーです。

```toml
[[models]]
id = "claude-quick"

[models.router]
type = "auto"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `auto` |
| `capable_target` | ✅ 必須 | `stage_router` と同じ |
| `efficient_target` | ✅ 必須 | `stage_router` と同じ |

#### `type = "random"`

2 つ以上のターゲットに重みを付けてトラフィックを分けます。カナリア用です。既定では
Claude Code のセッション 1 つが同じ枝に留まります。

```toml
[[models]]
id = "claude-canary"

[models.router]
type = "random"
targets = ["claude-sonnet-4-6", "gpt-5.6-terra"]
weights = [9, 1]
# seed = 0
# affinity = "session"
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `random` |
| `targets` | ✅ 必須 | トラフィックを分けるモデル id |
| `weights` | 均等 | ターゲットごとに 0 以上の重みを 1 つ。`0` はそのターゲットを無効にします。各重みは有限でなければならず、その合計も有限である必要があります — 合計が無限大にあふれるリストは読み込み時に拒否されます |
| `seed` | `0` | `session` アフィニティではハッシュのソルト、`request` アフィニティでは抽選のシード |
| `affinity` | `session` | `session` は 1 セッションを 1 つの枝に固定し、`request` はリクエストごとに抽選します |

`affinity = "session"` では、枝は `sha256(seed ‖ model ‖ セッション id)` を重みの範囲に
写した値です。何も保存しないので再起動しても枝は変わらず、同じ設定を読み込んだレプリカ間
でも同一です。そのかわり `seed`、`targets`、`weights` を変えると枝が動くことがあります。
`x-claude-code-session-id` を送らないリクエストは 1 つの枝を共有せず、そのリクエストだけの
重み付き抽選を行います。セッションを送らないクライアントでも 90/10 の分割は 90/10 のまま
です。`affinity = "request"` ではリクエストごとに抽選し、`seed` を設定すると抽選列を再現
できます。

セッションアフィニティは**アクセス制御ではなく固定（stickiness）です。** セッション id は
クライアントが決めるので、id を変えて再試行する呼び出し元は自分を望みの枝へ寄せられます。
それで守られるものはありません — どのターゲットも、その呼び出し元が名前で直接指定できる
公開モデル id だからです。アクセス制御は managed model ポリシーの仕事です。

リクエストボディのない面 — `GET /routes`、`/v1/models` ディスカバリ、`shunt check` — は、
重みが正の最初のターゲットを報告します。

#### `type = "noop"`

アップストリームをまったく呼ばずに応答します。呼び出し元と同じモードで、空の終端
アシスタントメッセージを合成します。`stream: true` には妥当な SSE シーケンス
（`message_start`、`stop_reason: "end_turn"` を載せた `message_delta`、`message_stop`）を、
そうでなければ Message の JSON オブジェクト 1 つを返します。`count_tokens` は
`input_tokens: 0` と答えます。このルートも他と同じく認証を通るので、認証の穴ではありません
— トークンを 1 つも使わずにインバウンド経路全体を確かめる、クライアント配線のスモーク
テストです。

```toml
[[models]]
id = "claude-noop"

[models.router]
type = "noop"
```

受け付けるキーは `type` だけです。

#### `type = "prefill_router"`

学習済みルーターです。上流の分類器が直近のテキストユーザーターンを採点してエントリの
ターゲットのひとつを選びます。判定のためにアップストリームを呼ぶのではなく、モデルを
このプロセス内で動かします。

**これだけはビルドを選びます。** `prefill_router` は `prefill-router` カーゴフィーチャーを
有効にしたときにのみコンパイルされます。このフィーチャーは**既定で無効**で、リリース
ワークフローが有効にすることもありません。したがってリリースバイナリにも Homebrew の
インストールにも入っていません。ソースからビルドしてください。

```sh
cargo build --release --features prefill-router          # ダッシュボードが必要なら ,ui を足します
```

以下の設定はどのビルドでもパースされます。違うのはロードです。
フィーチャーのないバイナリではロードが失敗するので、設定したアルゴリズムを持たないまま
ゲートウェイが起動してしまう代わりに `shunt check` がそれを報告します。
このフィーチャーのエラーはテーブルに対する他のどの指摘よりも先に報告されるため、
キー単位のルール（空のターゲット、空の `checkpoint`、0 以下の `max_length` や
`batch_size`）はフィーチャーを有効にしたビルドが報告するものです。

```text
models entry <id> router type = "prefill_router" is not compiled into this binary: it needs the `prefill-router` cargo feature, which is off by default and absent from release binaries; build from source with `cargo build --features prefill-router` (docs/routing-algorithms.md)
```

```toml
[[models]]
id = "claude-learned"

[models.router]
type = "prefill_router"
targets = ["claude-sonnet-4-6", "claude-opus-4-8"]
checkpoint = "/models/router.pt"
# device = "cpu"
# cache_dir = "/var/cache/huggingface"
# max_length = 2048
# batch_size = 32
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | `prefill_router` |
| `targets` | ✅ 必須 | 選択肢となるモデル id。チェックポイントのヘッド順に並べます |
| `checkpoint` | ✅ 必須 | テンソルのみのルーターチェックポイントへのパス。相対パスはプロセスの作業ディレクトリ基準で解決されます |
| `device` | 自動検出 | ルーターを動かす torch デバイス — `cpu`、`cuda`、`cuda:0` |
| `cache_dir` | — | エンコーダーとそのトークナイザー用の Hugging Face キャッシュディレクトリ |
| `max_length` | `2048` | エンコーダー入力の最大トークン長。これを超える入力は切り詰められます。`0` より大きい必要があり、未設定なら上流の既定値がそのまま使われます |
| `batch_size` | `32` | エンコーダーの 1 回の forward に渡すプロンプトの最大数。`0` より大きい必要があり、未設定なら上流の既定値がそのまま使われます |

**運用者が用意するもの。** このフィーチャーは PyO3 で Python を埋め込むため、ビルドは
libpython をリンクし、稼働中のゲートウェイは埋め込んだインタープリターで `torch`、
`transformers`、`numpy`、`accelerate` を import できる必要があります。ビルド時に
`PYO3_PYTHON` をそのインタープリター（3.10 以上、共有 libpython つき）に設定してください。
設定しないと PyO3 は `PATH` で最初に見つけた `python3` を使います。さらにルーター
チェックポイントも必要です。Switchyard v0.3.0 はチェックポイントもエクスポーターも
エンコーダーのアセットも同梱していないので、互換のあるものを入手するか学習させるのは
運用者の仕事です。どちらかが欠けていればゲートウェイは起動を拒否し、ホットリロードで同じ
問題に当たればリロードを拒否して稼働中の設定をそのまま残します。

```text
models entry <id> router type = "prefill_router" failed to load: <upstream error>
```

リロードはルーターを作り直し、セッションごとのアフィニティはその中にあるので、リロードは
どのセッションがどのターゲットにいたかを忘れます。

**アドミッションが先です。** ルーターが駆動されるのは、`[server.auth]` と gateway policy の
`availableModels` がリクエストを通した後だけです。インバウンド認証は、ルーターが選ぶ一つの
ターゲットではなくエントリが名指しするすべてのターゲットを対象とするため、いずれかのターゲットが
クレデンシャルを注入するなら呼び出し元は認証しなければなりません。一方 managed-model ポリシーが
見るのは要求された id だけなので、`availableModels` にはこの id を書き、内部のターゲットは
書きません。したがって、どちらかのゲートが拒否した呼び出し元は推論を走らせられず、送られてきた
セッション id にアフィニティが記録されることもありません。

前半は条件として読んでください。ターゲットが*すべて*パススルーのエントリは、envelope のどこでも
クレデンシャルを注入しないため、インバウンド認証は要求するものがなく、匿名の呼び出し元まで
通してしまい、その呼び出し元がルーターを駆動します。駆動そのものを守りたいなら、クレデンシャルを
注入するターゲットを用意してください。すべてパススルーのエントリは `[server.auth]` でも
gateway ログインでも守られません。

**ターンの決め方。** アルゴリズムに渡されるのは `user` と `assistant` のロール、そして
`text` と `tool_result` のブロックだけです。アルゴリズムは直近のテキストユーザーターンを
採点し、ブロックがすべて `tool_result` のメッセージは新しい人間のターンではなくツールの
継続として扱います。セッションの識別は `x-claude-code-session-id` から、委譲された子で
あれば `x-claude-code-agent-id` も併せて読みます。そのため継続のリクエストは推論を
やり直さずそのターンの判断を再利用します。どちらも送らない呼び出し元は、上流の規則どおり
最初のユーザーメッセージのハッシュにフォールバックします。推論はブロッキングワーカー上で、
エントリごとに 1 件ずつ実行されます。`count_tokens` のプローブも同じ方法で決まり、継続の
場合は推論ではなくアフィニティのヒットになります。

`x-gateway-route-source` と `shunt.router.decisions{algorithm="prefill_router"}` の
`source` ラベルは、3 つのどれが起きたかを示します。

| ソース | 意味 |
| :-- | :-- |
| `prefill` | ルーターがターンを決めた — 推論、またはセッションアフィニティのヒット |
| `prefill_fail_open` | ルーティング呼び出しが失敗し、既定のターゲットへ回しました。上流の規則どおり `targets` の先頭です |
| `prefill_default` | リクエストボディのない面 — `/v1/models` ディスカバリ、`GET /routes`、モデル解決 — は採点するターンがないので先頭のターゲットを報告します |

`GET /routes` はそのエントリを `algorithm: "prefill_router"` とターゲット一覧で示します。
`shunt.stage_router.*` のメトリクスはシグナルのみのルーターのもののままで、prefill の行が
増えることはありません。

#### 検証

ターゲット自身がルーターである場合、空のターゲット、`(0.0, 1.0]` を外れたしきい値、
`recent_turn_window` が `0`、ルーターの **id** が `[1m]` または `[1M]` で終わる場合、いずれか一方が
ルーターテーブルを持つ重複 `[[models]]` id、このビルドが実装していない `type`、同じ
エントリが `[models.upstream_model]` も宣言している場合は起動エラーです。`prefill_router` の
エントリでは、空の `targets`、同じターゲットの重複、空の `checkpoint`、`0` の
`max_length` や `batch_size` も起動エラーです。これらはフィーチャーを**オンにした**
ビルドが報告するものです。フィーチャーのないビルドは、これらのいずれにも達する前に
足りない cargo フィーチャーを挙げてそのエントリを拒否するからです。重複ターゲットは
他のターゲット比較と同じく末尾の `[1m]`/`[1M]`
ヒントを除去してから比較されます。マップなしの
エントリ同士は本来同じ id を共有できますが、ルーターはディスカバリのメタデータではなく
ルーティングポリシーを指定するため、重複すると 1 つの id に 2 つのポリシーが残ります。
ターゲット id はルーティングが照合するのと同じく、末尾の `[1m]` または `[1M]` ヒントを除去
してから比較されます。そのため自前のルーターを持つエントリに解決されるターゲットは、
2 つの `type` が何であっても拒否され、これが解決を 1 ホップに留めています。
[`[models.router.classifier]`](#modelsrouterclassifierオプション) のターゲットも同じ
1 ホップ規則に従い、さらに 2 つの検査が加わります。`classifier.base_threshold` は
`confidence_threshold` とまったく同じく範囲を検査され、ジャッジのターゲットは passthrough
ルートに解決されてはいけません — 実効チェーンに passthrough のアップストリームを含む
ターゲットは、ジャッジ呼び出しで呼び出し元の資格情報が取り除かれたあとに実行するものが
残らないため起動エラーです。`auth = "none"` は受け付けられます。資格情報を必要としない
エンドポイントと、資格情報が失われたエンドポイントは別物です。
[呼び出しごとの上限](#呼び出しごとの上限)の 6 つのうちどれかが `0` であれば、そのキーを
示す起動エラーです。
次の 4 つはロードを失敗させず警告のみです。いずれも運用者が意図しうる設定だからです —
明示的なルートに一致しないターゲット(一致しない他の id と同様に
`server.default_provider` で解決され、この警告はすべてのルーター `type` を対象にします)、
同じ id に解決される `capable_target` と
`efficient_target`(2 つのティアを意図的に 1 つのモデルにまとめた場合)、
`confidence_threshold` より低い `deescalate_threshold`(コストを優先する構成が望みうる、
下げる方向をより簡単にした設定)、そしてルーター自身の id を指定した `[[routes]]`
エントリ(その id の宛先はルーターが決めるため参照されません)。id が単にその接頭辞で
始まるだけの `[[route_prefixes]]` エントリは報告され**ません** — その接頭辞に一致する
他の id は引き続き処理されるからです。
各警告はロードごとに一度出力されます。ホットリロードもロードなので、設定を直さない限り
リロードのたびに再び出力されます。

### `[models.subagents]`（オプション）

任意の `[[models]]` エントリに載せられる、**委譲された作業のためのオーバーレイです。**
`[models.upstream_model]` マップを持つエントリ、`[models.router]` テーブルを持つエントリ、
マップを持たず `[[routes]]` 経由で解決される id のいずれでも構いません。その id を要求した
`Task` サブエージェント、フックエージェント、ワークフローサブエージェントは、オーバー
レイのターゲットへ振り分けられます。親セッション自身のターンはこのテーブルを一切見ず、
オーバーレイがなかったときとまったく同じようにエントリを解決します。テーブルを `router`
の中ではなくエントリ側に置くのは、固定エントリにはルーターテーブルが存在せず、
Switchyard の「passthrough with subagents」がここではまさに固定エントリにあたるからです。

```toml
[[models]]
id = "claude-opus-4-8"

[models.upstream_model]
anthropic = "claude-opus-4-8"

[models.subagents]
type = "passthrough"
target = "claude-haiku-4-5"
by_type = { Explore = "claude-haiku-4-5", fork = "claude-sonnet-4-6", teammate = "claude-sonnet-4-6" }
```

| キー | 既定値 | 意味 |
| :-- | :-- | :-- |
| `type` | ✅ 必須 | 上記の固定形式である `passthrough`、またはジャッジが子のターゲットを選ぶ [`llm_classifier`](#subagents-type--llm_classifier) |
| `target` | ✅ 必須（`passthrough`） | 委譲されたターンのエージェントタイプについて `by_type` が何も指定していないときの宛先モデル id — エージェントタイプのヘッダーが送られてこない場合は、委譲されたすべてのターンがここへ行きます |
| `by_type` | `{}` | エージェントタイプ → モデル id。`x-claude-code-agent-type` のリテラル値をキーにします |

**委譲された作業とみなされるもの。** `x-claude-code-request-class` が `subagent` または
`workflow` のリクエストです。そのヘッダーがない場合は、空でない
`x-claude-code-agent-id` を持つリクエストが該当します。このヘッダーは Claude Code が
ヒントのゲートに関係なくすべての委譲ターンで送ります。クラスが送られてきたときは
そちらが正です。エージェント id を伴う `main` はメイントラフィックであり、`compaction`
と `auxiliary` はハーネスの保守作業で、この 3 つがオーバーレイを使うことはありません。
したがって、クラスとタイプのヘッダーがゲートで止まっているデフォルトのデプロイでは、
`Task` の子はすべて `target` へ行きます。`by_type` を使うにはクライアント側で
`CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` を設定する必要があります。

**`by_type` のキーは厳密に照合され、大文字小文字も区別します。** 組み込みエージェントの
id はそのまま届きます — `Explore`、`Plan`、`general-purpose`、`claude`、そして `fork`
（クライアントは `CLAUDE_CODE_FORK_SUBAGENT=1` のときだけ提示します）。`.claude/agents/`
のプロジェクトエージェントは `custom` として届き、自身の名前は決して送られないため、
一致しうるキーは `custom` だけです。`teammate` は Agent Teams のメンバーを指すクライアント
側のリテラルで、ワイヤ上ではまだ観測されていません。空のキーや空白文字を含むキーは、
どうやっても一致しないため起動エラーです。

**ターゲット。** ルーターのターゲットと同じワンホップ規則に従う、通常の公開モデル id です。
`target` と `by_type` のすべての値は、末尾の `[1m]`／`[1M]` ヒントを除去したあとで、自前の
`[models.router]` テーブルや `[models.subagents]` テーブルを持つエントリに解決されては
なりません — そしてルーターのターゲットが、このオーバーレイを持つエントリに解決されるこ
とも許されません。空のターゲット、`[1m]` または `[1M]` で終わるオーバーレイ付きの id、
いずれか一方がこのテーブルを持つ重複 `[[models]]` id は起動エラーです。明示的なルートに
一致しないターゲットは、ルーターのターゲットと同様にロード時に警告を出し、その場合も
`server.default_provider` で解決されます。

**状態を持ちません。** ターゲットは設定とリクエストのヘッダーだけで決まります。セッション
ピンもストアもジャッジ呼び出しもありません。ルーターを持つ id では、子はルーターが走る
前に振り分けられるため、子のターンがトランスクリプトに対して採点されることはなく、親の
ピンに触れることもありません。振り分けられたターンには `x-gateway-routed-model`
（ターゲット）と `x-gateway-route-source`（`by_type` に一致したときは `subagent_type`、
`target` へのフォールバックなら `subagent`）が付き、`shunt.router.decisions` に
`algorithm = "subagents"` として計上されます。リクエストを伴わないサーフェスは何も
解決しません。`/v1/models` ディスカバリと `shunt check` はモデルごとの宛先情報を
まったく持たず、`GET /routes` は親自身の `[[routes]]`／`[models.router]` エントリが
ある場合にそれを示すだけです（`server.default_provider` に委ねられた id はどちらの
配列にも現れません）。いずれのサーフェスも振り分け先のターゲットを解決せず、この
オーバーレイが `routers` 配列に載ることもありません。

#### subagents `type = "llm_classifier"`

オーバーレイのもうひとつの形式です。固定のターゲットの代わりに、ジャッジが委譲された
タスクを読み、それを処理するグループの名前を挙げます。ここにあるのは `mode = "custom"`
だけで — `mode = "capability"` は起動エラーです — キーは[上](#type--llm_classifier)で
説明した `custom` モードのものです。

```toml
[models.subagents]
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
classify_trigger = "new_session"
max_output_tokens = 64
prompt = """
委譲されたタスクに対するターゲットをちょうど 1 つ選んでください。

- コードレビュー、批評、監査、正しさの分析には "capable" を選んでください。
- 実装、調査、説明、その他の委譲作業には "efficient" を選んでください。

レスポンススキーマに一致する JSON だけを返してください。
"""
response_schema = '''
{"type": "object",
 "properties": {"target": {"type": "string", "enum": ["capable", "efficient"]}},
 "required": ["target"],
 "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }
```

`[models.router]` の形式と異なる規則が 3 つあります。

- **`classify_trigger` の既定値は `new_session`** で、`user_turn` は拒否されます。委譲
  された子はひとつのタスクなので、ターゲットは一度選んで最後まで保持します。ユーザー
  ターンごとに判定し直せば、変わりようのない決定にジャッジ呼び出しを払い続けることに
  なります。
- **`message_hash_fallback` は `false` でなければなりません。** 分類はすでに (セッション,
  エージェント) でキーを取っているので、代わりに最初のメッセージをハッシュすると、ひとつ
  のセッションの異なる子ふたつがひとつの判定に束ねられてしまいます。
- **親が分類されることはありません。** 何が委譲された作業かは、上の `passthrough` 形式と
  まったく同じです。したがって親のターン、エージェント id を伴う `main` のターン、
  `compaction` と `auxiliary` のクラスはいずれも、このテーブルがないかのようにエントリを
  解決し、ジャッジ呼び出しも行いません。

6 つの[呼び出しごとの上限](#呼び出しごとの上限)は、呼び出しを行う当事者であるこの
テーブルに置きます。ジャッジは他と同じ規則に従います — 1 ホップ、passthrough ルート禁止、
そして呼び出し元の資格情報スロットはひとつも同行しません。委譲されたターンがジャッジを
呼びうるため、そうしたターンのインバウンド認証はオーバーレイのターゲットとジャッジまで
対象にします — 認証できない委譲ターンはジャッジ呼び出しを 1 回も行わずに拒否されます。
`count_tokens` プローブも同様にジャッジ呼び出しなしで、`default_target` グループの最初の
モデルで応答します。

## `[sentry]`(任意)

自分の Sentry プロジェクトへのオプトインのエラーレポーティング。`dsn` を設定しない限りオフで、`[otel]` とは独立しています。ゲートウェイ自身の診断情報を報告します — 致命的なゲートウェイの起動/サーブエラー、パニック、`error` レベルのログイベント(`warn`/`info` はブレッドクラムとして、メッセージのみ)— さらに `dsn` が設定されていれば、アップストリームのプロバイダーが失敗レスポンスを返すたびに無条件でエラー/警告イベントを送信します: 5xx レスポンスは `error`、429/529(レート制限/過負荷)は `warning` で、それぞれ `model`、`provider`、`upstream_status` のみをタグ付けします。リクエスト/レスポンスの本文、ヘッダー、認証情報は決して送信されません。メトリクスとトレーシングはそれぞれ別個の追加オプトインです。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `dsn` | — | Sentry プロジェクトの DSN。空で無効化、不正な DSN は起動エラー。Redacting secret — 診断出力では `[redacted]` と表示される([Secret 参照](#secret-参照)を参照)。 |
| `environment` | — | 報告イベントに付く任意の environment タグ |
| `metrics` | `false` | 使用量メトリクスも送信 — OpenTelemetry ガイドに記載された gateway メトリクス系列(集計値のみ) |
| `traces_sample_rate` | `0.0` | パフォーマンストレースも送信: リクエストごとのスパンが Sentry トランザクションになり、`[0.0, 1.0]` のこのレートでヘッドサンプリング。`0.0` はスパンを一切送らず、範囲外は起動エラー。 |
| `include_session_id` | `false` | Sentry へ送るリクエストスパンにクライアントのセッション id を付与 |

## `[otel]`(任意)

トレース・メトリクス・ログを自分のコレクターへ送るオプトインの OpenTelemetry(OTLP/HTTP)エクスポート([詳細](/ja/guides/opentelemetry/))。`endpoint` を設定しない限りオフで、Sentry とは独立しています。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `endpoint` | — | OTLP/HTTP のベース URL(例: `http://localhost:4318`)。shunt が `/v1/{traces,metrics,logs}` を付加。空で無効化、`http(s)` 以外の URL は起動エラー。 |
| `service_name` | `shunt` | `service.name` リソース属性(`OTEL_SERVICE_NAME` より優先) |
| `environment` | — | 任意: `deployment.environment.name` |
| `sample_ratio` | `1.0` | `[0.0, 1.0]` のヘッドベースのトレースサンプリング。範囲外は起動エラー |
| `traces` | `true` | リクエストごとの `proxy_request` スパンをエクスポート |
| `metrics` | `true` | OpenTelemetry ガイドに記載された gateway メトリクス系列をエクスポート |
| `logs` | `true` | `tracing` ログイベントをエクスポート(stderr ログには影響なし) |
| `include_session_id` | `false` | リクエストスパンにクライアントのセッション id を付与 |

## `[otel.headers]`(任意)

すべての OTLP リクエストに付くヘッダー(例: ホスト型コレクターのトークン)。標準の `OTEL_EXPORTER_OTLP_HEADERS` の下にマージされます。各ヘッダー値は redacting secret 型として扱われ、診断出力では `[redacted]` と表示されます([Secret 参照](#secret-参照)を参照)。

| キー | 意味 |
| :-- | :-- |
| 任意 | ヘッダー名 → 値、例: `authorization = "Bearer <token>"` |

## ルーティング優先順位

委譲されたターンでは、一致する `[models.subagents]` オーバーレイ → 一致する `[models.router]` エントリ → 一致する `[models.upstream_model]` エントリ → 厳密な `[[routes]]` マッチ → `[[route_prefixes]]` プレフィックスマッチ → `server.default_provider`。

オーバーレイが先頭に来るのは委譲された作業に限られます。オーバーレイを持つ id への `Task` の
子要求は、そのエントリ自身のルーターやマップが参照される前にオーバーレイのターゲットへ
振り向けられ、親自身のターンと `compaction`・`auxiliary` のターンは、そのテーブルが無いものと
してエントリを解決します。以下のラダーは、そうしたターンとオーバーレイを持たないすべての id
が解決していく経路です。

ルーターが次に来るのは、`[[models]]` エントリ自体で一致するからです。ルーターを持つ id
への要求はルーターが応答し、ルーターはティアを選んだうえで**そのターゲット**を残りのラダーで
解決します。したがって `[[routes]]` のエントリが指定すべきなのはルーター id ではなく
ターゲットです。ルーター id を指定した厳密一致のエントリは参照されず、ロード時に警告が
出ます。`[[route_prefixes]]` のエントリは影響を受けません。ルーターがその接頭辞から
取り去るのは自身の id だけです。
