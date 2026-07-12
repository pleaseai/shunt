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

各プロバイダーは、あなたが選んだ名前の下のテーブルです。組み込み（`anthropic`、`openai`、`codex`）は部分的にオーバーライドできます — 設定マップはディープマージします。

| キー | 値 | 意味 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` | 上流プロトコル / アダプター。`anthropic` = Messages API（パススルー、オプションで再キー付け）。`responses` = Anthropic Messages を OpenAI Responses API へ変換。 |
| `base_url` | URL | 上流のベース。shunt がエンドポイントパスを追加します。 |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` | `passthrough` はクライアント自身の認証情報を転送。`api_key` は `api_key_env` からキーを注入。`chatgpt_oauth` は `~/.codex/auth.json` を再利用。 |
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

## ルーティング優先順位

厳密な `[[routes]]` マッチ → `[[route_prefixes]]` プレフィックスマッチ → `server.default_provider`。
