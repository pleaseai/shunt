---
title: 設定リファレンス
description: すべての shunt.toml キー — server、providers、routes、models。
---

ファイルの場所、優先順位、注釈付きの例については [Configuration](/ja/guides/configuration/) を参照してください。完全なテンプレート: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)。

## `[server]`

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt がリッスンするアドレス |
| `default_provider` | `anthropic` | マッチするルートがないモデルのプロバイダー |
| `sse_keepalive_seconds` | `30` | SSE `ping` が注入されるまでのアイドル秒数。`0` で無効化（[詳細](/ja/guides/shared-gateway/#sse-keepalive-pings)） |

## `[server.auth]`（オプション）

このテーブルの存在がインバウンドのクライアントトークン認証を有効化します（[詳細](/ja/guides/shared-gateway/)）。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | クライアントトークンを運ぶヘッダー |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | カンマ区切りの `name:token` ペアを保持する環境変数 |

指定された環境変数には 1 つ以上の認証情報が必要です。例: `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`。テーブルが存在するのに変数が未設定・空・不正な場合、起動はフェイルクローズします。ゲートされるルート（マッピングされた `/v1/messages` 推論と `GET /v1/models` discovery）は、設定されたヘッダー、`Authorization: Bearer`、`x-api-key` のいずれでもトークンを受け付けます — 複数のスロットに有効なトークンがある場合は専用ヘッダーが優先されます。

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

## `[server.pool]`（オプション）

Claude（Anthropic）アカウントプール向けの、クォータを考慮した負荷分散のチューニングです（[詳細](/ja/guides/anthropic-multi-account/#選択のチューニングserverpool)）。テーブルが存在しない場合、選択はこのテーブルが導入される前と同じ、組み込みの単一しきい値 `0.98` を使います。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | すべてのクォータウィンドウに対する安全策のバックストップ。これ以上のアカウントは、利用可能なアカウントの中で常に最後にソートされます |
| `default_threshold` | 未設定 | より具体的な値を持たないウィンドウに対するソフトなデフォルトしきい値 |
| `default_threshold_5h` | 未設定 | 5 時間ウィンドウのソフトなデフォルト |
| `default_threshold_7d` | 未設定 | 共有の週次（`7d`）ウィンドウのソフトなデフォルト |
| `default_threshold_fable` | 未設定 | fable 専用の週次（`7d_oi`）ウィンドウのソフトなデフォルト |
| `burn_rate_avoidance` | `false` | ウィンドウのリセット前にソフトしきい値を使い切ると予測されるアカウントも回避する |

各ウィンドウ `X` について、有効なソフトしきい値は次の順で解決されます: アカウントの `threshold_X` → アカウントの `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`。これは `hard_threshold` を上限としてクランプされます。すべてのしきい値は `[0.0, 1.0]` の使用率の割合であり、範囲外の値は起動時にエラーになります。クォータヘッダーは Anthropic バックエンドにのみ存在するため、これらのノブは Codex/ChatGPT プールでは無効です — アカウント単位の `priority` と `disabled` はそちらでも引き続き適用されます（キーの詳細は [Anthropic マルチアカウント](/ja/guides/anthropic-multi-account/) を参照）。

## `[providers.<name>]`

各プロバイダーは、あなたが選んだ名前の下のテーブルです。組み込み（`anthropic`、`openai`、`codex`、`xai`、`grok`、`cursor`）は部分的にオーバーライドできます — 設定マップはディープマージします。

| キー | 値 | 意味 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` | 上流プロトコル / アダプター。`anthropic` = Messages API（パススルー、オプションで再キー付け）。`responses` = Anthropic Messages を OpenAI Responses API へ変換。`cursor` = ネイティブな Cursor ConnectRPC/protobuf AgentService アダプター。 |
| `base_url` | URL | 上流のベース。shunt がエンドポイントパスを追加します。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough` はクライアント自身の credential を転送。`api_key` は `api_key_env` からキーを注入。`chatgpt_oauth` は `~/.codex/auth.json` を再利用。`claude_oauth` は明示的な Anthropic アカウントから選択。`xai_oauth` は `shunt login xai` からの `~/.shunt/xai-auth.json` を再利用（HTTPS 上の x.ai/grok.com ホストへのみ送信）。`cursor_oauth` は `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用。 |
| `api_key_env` | 環境変数名 | `auth = "api_key"` のとき、キーを読み取る場所。 |
| `api_key_header` | `bearer`（デフォルト） \| `x_api_key` | 注入されたキーを送るヘッダー。 |
| `effort` | `low` … `max` | オプションのデフォルト reasoning エフォート（`responses` プロバイダー）。 |
| `count_tokens` | `tiktoken`（デフォルト） \| `estimate` | `responses` および `cursor` provider: ローカルの tiktoken カウント vs. `501 not_supported` フォールバック（[詳細](/ja/guides/effort-and-context/#token-counting-count_tokens)）。 |

名前だけのエントリーは、`shunt login claude --name <name> --mode oauth|import|setup-token` で作成した `~/.shunt/accounts/claude/<name>.json` を読み取ります。対話型 CLI はこの 3 つの mode を提示し、リフレッシュ可能な OAuth を推奨します。`--long-lived` は `--mode setup-token` の deprecated alias です。`SHUNT_CLAUDE_ACCOUNTS_DIR` でストアディレクトリを上書きできます。リフレッシュ可能な OAuth/import ファイルは provider が refresh token をローテーションすると同じ場所に更新されるため、ファイルごとに稼働中の owner は 1 つだけにしてください。複数の shunt プロセスで共有したり、独立してコピーしたりしないでください。プロセスごとに個別にプロビジョニングするか、適切な場合は静的な setup token を使ってください。

## `[[routes]]`

厳密一致のルーティングエントリ — 最初にチェックされます。

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `model` | ✅ | Claude Code が送る正確な `model` id |
| `provider` | ✅ | `[providers.<name>]` テーブルの名前 |
| `upstream_model` | — | 上流へ転送するモデル id を書き換える |
| `effort` | — | ルート単位の reasoning エフォートオーバーライド |

## `[[route_prefixes]]`

プレフィックス一致のルーティングエントリ — 厳密ルートの後にチェックされます。

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `prefix` | ✅ | モデル id のプレフィックス、例 `gpt-` |
| `provider` | ✅ | `[providers.<name>]` テーブルの名前 |

## `[[models]]`

[model discovery](/ja/guides/model-discovery/) 向けに `GET /v1/models` が返すエントリ。id は `claude` または `anthropic` で始まる必要があります。さもないと Claude Code が無視します。

| キー | 必須 | 意味 |
| :-- | :-- | :-- |
| `id` | ✅ | Claude Code に公開されるモデル id |
| `display_name` | — | `/model` ピッカーに表示されるラベル |

## `[sentry]`(任意)

自分の Sentry プロジェクトへのオプトインのエラーレポーティング。`dsn` を設定しない限りオフで、`[otel]` とは独立しています。ゲートウェイ自身の診断情報のみを報告します — 致命的なゲートウェイの起動/サーブエラー、パニック、`error` レベルのログイベント(`warn`/`info` はブレッドクラムとして、メッセージのみ);リクエスト/レスポンスの本文、ヘッダー、認証情報は決して送信されません。メトリクスとトレーシングはそれぞれ別個の追加オプトインです。

| キー | デフォルト | 意味 |
| :-- | :-- | :-- |
| `dsn` | — | Sentry プロジェクトの DSN。空で無効化、不正な DSN は起動エラー。 |
| `environment` | — | 報告イベントに付く任意の environment タグ |
| `metrics` | `false` | 使用量メトリクスも送信 — `shunt.requests` / `shunt.latency` 系列(集計値のみ) |
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
| `metrics` | `true` | `shunt.requests` / `shunt.latency` 系列をエクスポート |
| `logs` | `true` | `tracing` ログイベントをエクスポート(stderr ログには影響なし) |
| `include_session_id` | `false` | リクエストスパンにクライアントのセッション id を付与 |

## `[otel.headers]`(任意)

すべての OTLP リクエストに付くヘッダー(例: ホスト型コレクターのトークン)。標準の `OTEL_EXPORTER_OTLP_HEADERS` の下にマージされます。

| キー | 意味 |
| :-- | :-- |
| 任意 | ヘッダー名 → 値、例: `authorization = "Bearer <token>"` |

## ルーティング優先順位

厳密な `[[routes]]` マッチ → `[[route_prefixes]]` プレフィックスマッチ → `server.default_provider`。
