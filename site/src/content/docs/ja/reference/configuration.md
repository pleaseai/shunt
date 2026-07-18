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

すべての OTLP リクエストに付くヘッダー(例: ホスト型コレクターのトークン)。標準の `OTEL_EXPORTER_OTLP_HEADERS` の下にマージされます。

| キー | 意味 |
| :-- | :-- |
| 任意 | ヘッダー名 → 値、例: `authorization = "Bearer <token>"` |

## ルーティング優先順位

厳密な `[[routes]]` マッチ → `[[route_prefixes]]` プレフィックスマッチ → `server.default_provider`。
