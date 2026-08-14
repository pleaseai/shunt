---
title: 設定リファレンス
description: すべての shunt.toml キー — server、providers、routes、models。
---

ファイルの場所、優先順位、注釈付きの例については [Configuration](/ja/guides/configuration/) を参照してください。完全なテンプレート: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)。

## Secret 参照

設定ファイルの文字列値は、リテラルの代わりに `${VAR}` または `${file:/絶対/パス}` として書けます。`${VAR}` は環境変数 `VAR` の値に置き換わり、`"Bearer ${TOKEN}"` のようにより長い文字列に埋め込むこともできます(変数が未定義だと設定の読み込みは失敗します)。`${file:/絶対/パス}` は指定したファイルの内容(トリム済み)に置き換わり、パスは絶対パスでなければならず、フィールドの値全体でなければなりません — 他の文字列に埋め込むことはできません(ファイルが読み取れない、パスが相対パスである、または他の文字列に埋め込まれている場合、設定の読み込みは失敗します)。`$${` はリテラルの `${` にエスケープされます。解決は再帰的ではありません — 解決済みの値は再スキャンされません。この置換は設定ファイルにのみ適用され、`SHUNT_*` 環境変数オーバーライドはそのまま使われます。起動時、`shunt check`、[ホットリロード](https://github.com/pleaseai/shunt/blob/main/docs/config-reload.md)(SIGHUP とファイル監視)を含む設定の読み込みのたびに再実行されるため、`${file:}` で参照したシークレットはファイルを書き換えてリロードをトリガーするだけで、再起動なしにローテーションできます。ただし、ローテーションした値が実際に反映されるかどうかは、そのフィールド自身のリロード動作に従います。`[sentry]` と `[otel]` は起動時に一度だけ初期化されるため、この 2 つのセクションのシークレットをローテーションしても設定が更新されるだけで、反映するには再起動が必要です。

`[sentry] dsn`、`[otel.headers]` の値、`[server.gateway.telemetry] forward_to[].headers` の値、`[server.gateway.session] jwt_secret` の 4 つのフィールドは redacting secret 型として扱われ、診断出力では `[redacted]` と表示されます。これらのフィールドにリテラル値を書くことは以前とまったく同じように動作します。secret 型のフィールドがリテラルを保持している場合、shunt は起動時に該当するフィールドパスのみを(値は決して含めずに)知らせる勧告的な警告を 1 回記録します。

既存の `tokens_env`、`jwt_secret_env`、`client_secret_env`、`api_key_env`、`users_env`、`token_env`、`tokens_file` フィールドはこの変更の影響を受けず、引き続き環境変数(`tokens_file` の場合はファイルパス)を指します(`jwt_secret_env` は別途 [`session.jwt_secret`](#servergatewaysessionオプション) に置き換えられ deprecated です)。

## `[server]`

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt がリッスンするアドレス |
| `default_provider` | `anthropic` | マッチするルートがないモデルのプロバイダー |
| `max_concurrent_requests` | `1024` | レスポンスボディの完了まで実行中として数えるインバウンドリクエストの最大数。超過したリクエストはキューに入れず、即座に `503` と `Retry-After: 1` で拒否します。`0` で制限を無効化でき、`/` と `/health` は対象外です。このキーを変更した後は再起動が必要です |
| `sse_keepalive_seconds` | `30` | SSE `ping` が注入されるまでのアイドル秒数。`0` で無効化（[詳細](/ja/guides/shared-gateway/#sse-keepalive-pings)） |

## HTTP チューニングテーブル

`[server.access_control]` は `allow_cidrs = []`、`deny_cidrs = []`、`trust_forwarded_for = false` を提供します。deny が先に評価され、`/` と `/health` にも適用されます。allow リストが空でなければデフォルト拒否になりますが、この 2 つのヘルスパスは allow チェックだけを免除されます。転送ヘッダーは、クライアント指定値を上書きする信頼済みプロキシの背後でのみ信頼してください。変更には再起動が必要です。

この `trust_forwarded_for` 設定は `[server.gateway] trust_forwarded_for` とは独立しています。access-control の設定は CIDR の許可・拒否ルールだけに適用され、gateway の設定はデバイスフローのレート制限だけに適用されます。両方のサーフェスを信頼済みリバースプロキシの背後で運用する場合は、両方の設定を有効にしてください。一方だけを設定すると、もう一方のサーフェスは引き続きソケットのピアアドレスを使用します。

`[server.limits]` の `max_request_bytes` は Anthropic Messages とインバウンド Codex Responses のリクエストボディに適用され、デフォルトは `33554432`（32 MiB）です。超過時は `413` を返します。その他のゲートウェイ、管理、テレメトリ、分析ルートでは、エンドポイント固有のボディ制限が維持されます。`max_request_header_bytes` と `max_url_length` はデフォルト未設定で、それぞれ `431` と `414` を返します。ヘッダーサイズは、解析済みの全ヘッダーについて名前と値の長さを合計した値です。ボディ制限はホットリロードされますが、ヘッダーと URL の制限には再起動が必要です。

`[server.timeouts] upstream_ttfb_ms` はデフォルト `120000` で、`0` で無効化します。推論アップストリームの HTTP レスポンスヘッダー待ちだけを制限するため、レスポンスボディと長時間の SSE ストリームには全体時間制限を設定しません。Anthropic Messages、OpenAI Responses HTTP（WebSocket フォールバックを含む）、Gemini HTTP、インバウンド Codex Responses パススルーを対象とし、Codex WebSocket、Cursor、Antigravity、補助 HTTP 呼び出しは対象外です。

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

このテーブルの存在が、ブラウザーでのアカウントプロビジョニングとアカウントプールの健全性のための管理 Web サーフェスを有効化します（[詳細](/ja/guides/admin-remote-provisioning/)）。テーブルがない場合、`/admin*` ルートは一切登録されません。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | API/curl 呼び出し用の管理トークンを運ぶヘッダー |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | カンマ区切りの `name:token` ペアを保持する環境変数 |
| `session_ttl_secs` | `3600` | ログイン後のブラウザーセッションの寿命（秒） |
| `pending_ttl_secs` | `600` | 開始したプロビジョニングフローを完了できる時間（秒） |

指定された環境変数には 1 つ以上の認証情報が必要です。例: `SHUNT_ADMIN_TOKENS="ops:<token>"`。テーブルが存在するのに変数が未設定・空・不正な場合、起動はフェイルクローズします。

管理トークンは `[server.auth]` の下で設定されるクライアントトークンとは別個の認証情報です。1 つの認証情報を両方のサーフェスで再利用しないでください。

`[server.auth]` の `tokens_env` と同様、この `tokens_env` の値も `${VAR}` / `${file:...}` で書けます([Secret 参照](#secret-参照)を参照)。

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

## `[server.pool]`（オプション）

アカウントプール向けの、クォータを考慮した負荷分散のチューニングです — Claude（Anthropic）（[詳細](/ja/guides/anthropic-multi-account/#選択のチューニングserverpool)）と、issue #195 以降は Codex/ChatGPT（[詳細](/ja/guides/codex-multi-account/)）が対象です。テーブルが存在しない場合、選択はこのテーブルが導入される前と同じ、組み込みの単一しきい値 `0.98` を使います。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | すべてのクォータウィンドウに対する安全策のバックストップ。これ以上のアカウントは、利用可能なアカウントの中で常に最後にソートされます |
| `default_threshold` | 未設定 | より具体的な値を持たないウィンドウに対するソフトなデフォルトしきい値 |
| `default_threshold_5h` | 未設定 | 5 時間ウィンドウのソフトなデフォルト |
| `default_threshold_7d` | 未設定 | 共有の週次（`7d`）ウィンドウのソフトなデフォルト |
| `default_threshold_fable` | 未設定 | fable 専用の週次（`7d_oi`）ウィンドウのソフトなデフォルト |
| `burn_rate_avoidance` | `false` | ウィンドウのリセット前にソフトしきい値を使い切ると予測されるアカウントも回避する |
| `usage_refresh_seconds` | 無効（`0`/未設定） | `GET /api/oauth/usage` のポーリング間隔（秒）。60 未満の正の値は 60 秒の下限に切り上げられます |
| `state_path` | 未設定 | プールのアカウント単位のクォータ状態を保存するファイル。再起動時に空のプールではなく、最後に観測された使用率からウォームスタートします。未設定で永続化は無効（デフォルト） |
| `ramp_initial_concurrency` | 無効（`0`/未設定） | ストーム制御: トラフィックを受け始めたばかりのアカウントアイデンティティに対する初期の並行受け入れ許容量。`0` または未設定で受け入れゲーティングは無効 |

各ウィンドウ `X` について、有効なソフトしきい値は次の順で解決されます: アカウントの `threshold_X` → アカウントの `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`。これは `hard_threshold` を上限としてクランプされます。すべてのしきい値は `[0.0, 1.0]` の使用率の割合であり、範囲外の値は起動時にエラーになります。しきい値とバーンレートのノブは両方のプールファミリーを制御します: Anthropic プールは `anthropic-ratelimit-unified-*` ヘッダーから、Codex/ChatGPT プールは `x-codex-*` の 5 時間／週次ウィンドウから制御されます（Codex には Fable スコープの `7d_oi` ウィンドウがないため、そこでは `default_threshold_fable` は無効です）。`usage_refresh_seconds` は Anthropic 専用です — Codex には帯域外の usage API がありません。

正の `usage_refresh_seconds` は追加でバックグラウンドポーラーを起動し、Claude アカウントプールのクォータ状態を Anthropic OAuth usage API と突き合わせて補正します。未設定または `0` で無効（デフォルト）です。ポーリングされるのは imported（更新可能）な `claude_oauth` アカウントのみで、長期の `claude setup-token` や `token_env` アカウントは、usage エンドポイントが更新不可トークンを拒否するためスキップされます。ポーラーはヘッダー由来の 5h／週次／Fable（`7d_oi`）クォータ状態を、shunt の外での同一アカウントの消費まで含む権威ある使用量と突き合わせます。間隔は起動時に固定され、設定のリロードではポーラーの起動・停止・再調整は行われません。

`state_path` はプールのクォータ状態（すべてのプロバイダーのアカウントについて、ウィンドウごとの使用率とリセット）をディスクに保存します。設定しない場合、再起動は空のプールから始まり、各アカウントは再起動後の最初のレスポンスまで未観測に見えるため、burn-rate 回避が無効になり、トラフィックでプールが再充填されるまで `GET /usage` は空を返します。このファイルは権威あるソースではなくベストエフォートのキャッシュです — クォータはいずれにせよアップストリームのレスポンスから再導出されるため、ファイルが欠落・陳腐化・破損していてもコールドスタートになるだけで、起動失敗にはなりません。書き込みは非公開の temp ファイル（Unix では `0600`）を対象にアトミックにリネームする方式で、クォータが変化したときだけバックグラウンドタイマーで行われます。書き込みに失敗した場合は次の tick で再試行します。クールダウンは保存されず（再起動で失効）、復元されたウィンドウのうちすでにリセットを過ぎたものは、復元後の最初の選択または snapshot で遅延破棄されます。パスは起動時に固定され、設定のリロードでは永続化の開始・停止・パス変更は行われません。

正の `ramp_initial_concurrency` は、すべてのアカウントプールで**ストーム制御（storm control）**を有効にします。フェイルオーバーの切り替え後、そうしなければ進行中の並行リクエストがすべて切り替え直後のアカウントに一度に着地してしまいます。ゲートを有効にすると、トラフィックを受け始めたばかりのアイデンティティ（新規、クールダウンから復帰、または 60 秒アイドル）は、設定された数までの並行リクエストしか受け入れません。成功レスポンスごとに許容量が倍増し（スロースタート）、フェイルオーバーに値する失敗はランプをリセットし、拒否されたリクエストは選択順で次のアカウントに回されます。最後に残った候補はゲートに関係なく常に試行されるため、ゲーティングはリクエストを遅延させることはあっても、ゲートなしのプールなら処理できたリクエストを失敗させることは決してありません。これは、プールのすべてのアカウントが単一のアップストリームアイデンティティに解決される場合、実質的にゲートなしと同じであることも意味します。唯一の候補は常に最後の候補でもあるため、この設定は異なるアカウントアイデンティティが 2 つ以上あるときにのみ効果を持ちます。

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
| `kind` | preset がない場合 | `anthropic`、`responses`、`cursor`、`gemini`、`antigravity`。後者 2 つは下記の preset 表に項目がないため（同名の組み込み `[providers.*]` テーブルは preset ではなく、別建てのレガシー方式です）、順序付き upstream では `kind` を明示的に指定する必要があります。 |
| `base_url` | preset がない場合 | アップストリームの base URL。`kind = "cursor"` ではログイン／トークン更新用エンドポイントにのみ使われます。推論は固定のエージェントホスト `https://agentn.global.api5.cursor.sh` を使用し、`SHUNT_CURSOR_AGENT_BASE_URL` でのみ上書きできます。 |
| `auth` | いいえ | auth mode の文字列、または mode 固有のマップ。デフォルトは preset の auth、preset もなければ `passthrough`。 |
| `effort`, `count_tokens`, `websocket`, `tool_search`, `request_compression`, `retry` | いいえ | レガシー provider と同じアップストリーム単位の設定。preset は `count_tokens` を上書きしません。Cursor アップストリームでも `retry` は正規化されますが、Cursor のストリーミングターンには適用されません。 |

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

`auth = "claude_oauth"` のような文字列は `auth = { mode = "claude_oauth" }` の省略形です。`api_key` マップは `env`（preset が提供しない場合は必須）と `header`（デフォルトは `bearer`、または `x_api_key`）を受け取ります。`claude_oauth` と `chatgpt_oauth` のマップは `account = "name"` または `accounts = [...]` で範囲を絞れますが、両方は指定できません。`accounts` にはストアエントリ名の文字列と完全なアカウントテーブルを指定できます。明示的な `accounts = []` は拒否され、両方のスコープフィールドを省略するとストア全体を走査します。ChatGPT ストアが空の場合、`chatgpt_oauth` は従来どおり `~/.codex/auth.json` にフォールバックします。`passthrough`、`xai_oauth`、`cursor_oauth` のマップは `mode` のみを受け付け、mode 固有の未知のキーはエラーです。

設定ファイル内で `[[upstreams]]` と `[providers.*]` を混在させないでください。ファイル層に両方の宣言形式があると起動に失敗します。環境変数はどちらの形式でも、正規化後のアップストリーム／provider 名を指定する `SHUNT_PROVIDERS__<name>__<field>` により個々のフィールドを上書きできます。順序付き `[[upstreams]]` 配列そのものは、1 つの環境変数で合成しようとせず、設定ファイルで宣言してください。レガシー `[providers.<name>]` は引き続きサポートされ、名前順の暗黙的アップストリームに正規化されます。この形式はフェイルオーバー順を宣言しないため、モデルマップは 0 または 1 エントリだけをサポートします。モデルマップに複数エントリを追加する前に `[[upstreams]]` へ移行してください。

### フェイルオーバー動作

複数エントリのモデルマップでは、宣言済みアップストリーム列からマップ内の名前だけを残してチェーンを構成します。アップストリームのステータスが `429`、`401`、`403`、`404`、任意の `5xx` の場合、またはアップストリームのレスポンスヘッダーを受け取る前に失敗した場合は、次のエントリへ進みます。auth の設定不備やアダプター自身の検証・ヘッダー構築エラーなど、アップストリーム試行を表さないゲートウェイローカルエラーは直ちに返し、設定問題をフェイルオーバーで隠しません。`2xx` ヘッダーを返した後は、その後ストリーミング本文が失敗してもフェイルオーバーしません。

チェーンを使い切ると、`429` → `401`/`403` → `404` → その他の `5xx` の優先順位で、最適な中継済み失敗を返します。ヘッダー前の失敗は最終候補として記憶しません。記憶した中継レスポンスがなければ、`all upstreams failed (N attempted)` というメッセージの `502 api_error` を返します。

`passthrough` アップストリームでは、クライアント自身の `authorization` / `x-api-key` がフェイルオーバー試行で転送されるのは、**プライマリ**ルート自体が `passthrough` であり、かつ試行先のオリジンがそのプライマリと一致する場合に限られます。このときの資格情報はプライマリにオリジン固有なクライアント自身のアップストリーム資格情報であるため、**異なる**オリジンへの `passthrough` フェイルオーバー試行ではこれを削除してフェイルクローズし、ホスト固有のトークンを別のオリジンへ再送しません。同一オリジンのフォールバック（例：1 つのホスト上の 2 つの passthrough エントリ）は引き続き資格情報を保持します。プライマリが自前の資格情報を注入する場合、クライアントのヘッダーはアップストリーム資格情報ではなくゲートウェイ／クライアントのシークレットであるため、すべての `passthrough` フォールバックはオリジンに関係なくこれを削除します。`api_key`／OAuth アップストリームは位置に関係なく自前のサーバーサイド資格情報を注入します。

origin に関係なく、保持された各スロットはそのスロットが実際に保持している値でもチェックされます。`authorization` と `x-api-key` は、そのスロット自身の値が shunt 自身が発行した JWT と**形が一致する**場合 — 3 セグメント構造で、ペイロードの `aud` クレームが `"shunt"` であるか `iss` クレームがこのゲートウェイの identity と一致する場合 — または設定済みの `[server.auth]` クライアントトークンと一致する場合にのみクリアされます。この JWT チェックは意図的に「今このトークンが認証されるか」ではなく「形が一致するか」で判定します: 期限切れのトークン、別の `public_url` を持つ兄弟インスタンスが発行したトークン、`jwt_secret` のローテーション後に検証できなくなったトークンも、依然として shunt 自身の認証情報であるため引き続きクリアされます。`apiKeyHelper` は両方のスロットを同じ値で埋めるため、どちらの認証情報も一方または両方のスロットに入り得ます。もう一方のスロットがゲートウェイ JWT や static なクライアントトークンを保持していても、本物のアップストリーム認証情報を保持しているスロットはそのまま転送されます。クリアされるのはゲート用認証情報を保持しているスロットだけです。`[server.auth] header` には `authorization` 自体を含め任意のヘッダー名を指定でき、そう設定した場合クライアントはプレフィックスなしの `Authorization: <token>` で認証します。そのためこのスロットは `Bearer` ペイロードだけでなく値全体としてもチェックされ、そうしたトークンがアップストリームへ転送されることはありません。 この設定には注意点があります: 推論リクエストでは shunt がルーティング前に設定されたヘッダーを無条件に除去するため、そのスロットは上流へ何も運びません — ゲートトークンだけでなく、呼び出し元自身の認証情報も落ちます。`header` を既定の専用 `x-shunt-token` のままにすればこの衝突を避けられます。

プロキシされた成功レスポンスと最終失敗には、`x-gateway-upstream`（選択したアップストリーム名）、`x-gateway-model`（クライアントが要求した id）、`x-gateway-upstream-model`（マッピング後のバックエンド id）が必ず含まれます。`count_tokens` はチェーンの最初の要素だけを使い、フェイルオーバーしません。`[server.codex_endpoint]` は設定された単一アップストリームに固定され、このチェーンには参加しません。

### 既存設定の移行

既存設定に**変更は不要です**。レガシー provider のルーティングと名前順の選択動作は維持されます。アップグレード時には、次の 3 つの追加または意図された動作変更があります。

1. 同じ物理 OAuth アカウントへ解決されるレガシー provider は、クォータウィンドウ、health、cooldown、refresh lock、in-flight admission 状態を共有するようになります。プール永続化キーのスキーマバージョンが上がるため、既存の `state_path` キャッシュは一度無視され、プールは一度コールドスタートします。
2. すべてのプロキシレスポンスに、上記 3 つの `x-gateway-*` metadata ヘッダーが追加されます。
3. Anthropic Messages ルート（`/v1/messages`）では、Claude または Codex OAuth プールのサイズにかかわらず、すべての試行がレスポンスヘッダー前に失敗すると、プール固有の `all Claude OAuth accounts failed before receiving an upstream response` または `all Codex OAuth accounts failed before receiving an upstream response` の代わりに `all upstreams failed (N attempted)` を返すようになりました。別の `[server.codex_endpoint]` インバウンド経路は影響を受けず、Codex 固有のメッセージを維持します。

順序付きフェイルオーバーを採用するには、各 `[providers.<name>]` テーブルを同名の `[[upstreams]]` エントリへ書き換え、`api_key_env`、`api_key_header`、OAuth `accounts` を `auth` マップへ移し、優先順に並べ、モデルの `upstream_model` マップへ参加する各名前を追加します。

`kimi` preset は `MOONSHOT_API_KEY` を読み取ります。`api_key_env = "KIMI_API_KEY"` を明示していた古い例はレガシー形式で引き続き動作し、アップストリームでも `auth = { mode = "api_key", env = "KIMI_API_KEY" }` と明示すれば従来の名前を維持できます。preset のデフォルトに依存するユーザーだけが `MOONSHOT_API_KEY` を export する必要があります。

## `[providers.<name>]`（レガシー）

各プロバイダーは、あなたが選んだ名前の下のテーブルです。組み込み（`anthropic`、`openai`、`codex`、`xai`、`grok`、`cursor`、`gemini`、`antigravity`）は部分的にオーバーライドできます — 設定マップはディープマージします。

| キー | 値 | 意味 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` \| `gemini` \| `antigravity` | 上流プロトコル / アダプター。`anthropic` = Messages API（パススルー、オプションで再キー付け）。`responses` = Anthropic Messages を OpenAI Responses API へ変換。`cursor` = ネイティブな Cursor ConnectRPC/protobuf AgentService アダプター。`gemini` = Anthropic Messages を Google Code Assist バックエンドの Gemini `generateContent`/`streamGenerateContent` へ変換。`antigravity` = 上流を持たず、ローカルの Antigravity CLI バイナリ（`agy`）をサブプロセスとして実行。 |
| `base_url` | URL | 上流のベース。shunt がエンドポイントパスを追加します。`kind = "cursor"` ではログイン／トークン更新用エンドポイントにのみ使われ、エージェント／推論ホストは選択しません。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` \| `google_oauth` \| `none` | `passthrough` はクライアント自身の credential を転送。`api_key` は `api_key_env` からキーを注入。`chatgpt_oauth` は `~/.codex/auth.json` を再利用。`claude_oauth` は明示的な Anthropic アカウントから選択。`xai_oauth` は `shunt login xai` からの `~/.shunt/xai-auth.json` を再利用（HTTPS 上の x.ai/grok.com ホストへのみ送信）。`cursor_oauth` は `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用。`google_oauth` は gemini CLI ログインの `~/.gemini/oauth_creds.json` を再利用し、`kind = "gemini"` でのみ有効。`none` は認証すべき上流を持たないアダプター（`kind = "antigravity"`）向けに、credential を一切送信しません。 |
| `api_key_env` | 環境変数名 | `auth = "api_key"` のとき、キーを読み取る場所。この値自体も `${VAR}` / `${file:...}` で書けます([Secret 参照](#secret-参照)を参照)。 |
| `api_key_header` | `bearer`（デフォルト） \| `x_api_key` | 注入されたキーを送るヘッダー。 |
| `effort` | `low` … `max` | オプションのデフォルト reasoning エフォート（`responses` プロバイダー）。 |
| `count_tokens` | `tiktoken`（デフォルト） \| `estimate` | `responses` および `cursor` provider: ローカルの tiktoken カウント vs. `501 not_supported` フォールバック（[詳細](/ja/guides/effort-and-context/#token-counting-count_tokens)）。 |
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
| `effort` | — | ルート単位の reasoning エフォートオーバーライド |

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

選定したエントリに `[models.upstream_model]` を追加すると、1つの宣言で id の公開、ルーティング、上流 id への変換を行えます。厳密な id のルーティングには、`[[routes]]` の代わりにこの形式を推奨します。順序付き `[[upstreams]]` では、マップに 1 つ以上の `upstream = "backend-id"` ペアを含めることができ、`[[upstreams]]` の宣言順でフェイルオーバーチェーンになります。レガシー `[providers.*]` には宣言済み順序がないため、正確に 1 ペアだけを許可します。その id ではマップが `[[routes]]`、`[[route_prefixes]]`、`server.default_provider` より優先され、各アップストリームのデフォルト `effort` がそのチェーン要素に適用されます。空のマップ、空または空白文字のみのアップストリーム名またはバックエンド id、未知のアップストリーム、同じ id の `[[routes]]` エントリ、`[1m]` または `[1M]` で終わるマップ付き id、あるいはいずれか一方がマップ付きである重複 `[[models]]` id は起動エラーです。client はマッチング前に context-window hint を取り除くため、マップ付き id にこの suffix を含めると、そのエントリには到達できません。マップなしエントリ同士の重複は従来の動作を維持します。

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

一致する `[models.upstream_model]` エントリ → 厳密な `[[routes]]` マッチ → `[[route_prefixes]]` プレフィックスマッチ → `server.default_provider`。
