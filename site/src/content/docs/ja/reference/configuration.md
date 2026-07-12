---
title: Configuration Reference
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

## `[providers.<name>]`

各プロバイダーは、あなたが選んだ名前の下のテーブルです。組み込み（`anthropic`、`openai`、`codex`、`xai`、`grok`、`cursor`）は部分的にオーバーライドできます — 設定マップはディープマージします。

| キー | 値 | 意味 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` | 上流プロトコル / アダプター。`anthropic` = Messages API（パススルー、オプションで再キー付け）。`responses` = Anthropic Messages を OpenAI Responses API へ変換。`cursor` = ネイティブな Cursor ConnectRPC/protobuf AgentService アダプター。 |
| `base_url` | URL | 上流のベース。shunt がエンドポイントパスを追加します。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough` はクライアント自身の認証情報を転送。`api_key` は `api_key_env` からキーを注入。`chatgpt_oauth` は `~/.codex/auth.json` を再利用。`xai_oauth` は `shunt login xai` からの `~/.shunt/xai-auth.json` を再利用（HTTPS 上の x.ai/grok.com ホストへのみ送信）。`cursor_oauth` は `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用。 |
| `api_key_env` | 環境変数名 | `auth = "api_key"` のとき、キーを読み取る場所。 |
| `api_key_header` | `bearer`（デフォルト） \| `x_api_key` | 注入されたキーを送るヘッダー。 |
| `effort` | `low` … `max` | オプションのデフォルト reasoning エフォート（`responses` プロバイダー）。 |
| `count_tokens` | `tiktoken`（デフォルト） \| `estimate` | `responses` プロバイダーのみ: ローカルの tiktoken カウント vs. 404 フォールバック（[詳細](/ja/guides/effort-and-context/#token-counting-count_tokens)）。 |

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
