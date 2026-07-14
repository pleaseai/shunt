---
title: HTTP エンドポイント
description: shunt が Claude Code LLM ゲートウェイとして提供するエンドポイント。
---

| メソッド | パス | 目的 |
| :-- | :-- | :-- |
| `HEAD` | `/` | Liveness プローブ |
| `GET` | `/` | 人間可読なランディング（バージョン + エンドポイント一覧） |
| `GET` | `/health` | ヘルスチェック — `{"status":"ok","version":"x.y.z"}` |
| `GET` | `/v1/models` | [Model discovery](/ja/guides/model-discovery/) — あなたの `[[models]]` エントリを返す |
| `GET` | `/routes` | shunt ネイティブのルート discovery — 設定された `[[routes]]` テーブルをそのまま返す（model → provider/upstream_model/effort のマッピング、claude プレフィックスの discovery エイリアスを含む）。`/v1/models` とは別物で、後者はより狭い Anthropic プロトコルの discovery レスポンス（`id`/`display_name` のみ）を提供する |
| `POST` | `/v1/messages` | 推論 — リクエストの `model` id に従ってルーティング |
| `POST` | `/v1/messages/count_tokens` | [トークンカウント](/ja/guides/effort-and-context/#token-counting-count_tokens) |
| `GET` | `/admin` | 管理ダッシュボード（HTML）。未サインイン時は `/admin/login` へリダイレクト |
| `GET`, `POST` | `/admin/login` | 管理トークンのログインフォームとブラウザーセッションの作成 |
| `POST` | `/admin/logout` | ブラウザーセッションの破棄 |
| `GET` | `/admin/accounts` | アカウントストアのメタデータ: 名前、種類、有効期限、UUID。トークン本体は決して返さない |
| `GET` | `/admin/pool` | `claude_oauth` プロバイダーごとのプール健全性: クォータ使用率、status、クールダウン、利用可否 |
| `POST` | `/admin/accounts/claude` | `{name, mode}` でブラウザープロビジョニングを開始。`mode` は `oauth` または `setup_token` で、省略時は `setup_token`。`{authorize_url}` を返す |
| `POST` | `/admin/accounts/claude/{name}/complete` | `<code>#<state>` を含む `{code}` でプロビジョニングを完了。アカウントを保存し、有効（live）かどうかを報告 |
| `DELETE` | `/admin/accounts/claude/{name}` | 指定した名前のアカウントのストアファイルを削除 |

`/admin*` ルートは [`[server.admin]`](/ja/reference/configuration/#serveradminオプション) が設定されている場合にのみ存在します。そのテーブルがなければ、いずれも登録されません。

`GET /` と `GET /health` は、[`[server.auth]`](/ja/guides/shared-gateway/) が有効なときも開いたままです（ヘルスチェックツールは通常トークンを付けられません）。機密情報は何も公開しません — ステータス、バージョン、およびすでに公開されているエンドポイント一覧のみです。

## ゲートウェイプロトコル

shunt は公式の [Claude Code LLM ゲートウェイプロトコル](https://code.claude.com/docs/en/llm-gateway-protocol)を実装します: 正しいヘッダーとボディフィールドの転送、機能のパススルー、システムプロンプトのアトリビューション処理。ゲートウェイ所有のエラーは Anthropic のエラー形で返され、上流のコンテキストオーバーフローエラーは Anthropic の `prompt is too long` の文言へ書き換えられて Claude Code の[コンパクト＆リトライ](/ja/guides/effort-and-context/#context-overflow-recovery)が発火し、ストリーミングレスポンスはバッファリングなしで中継されます（オプションで[キープアライブ ping](/ja/guides/shared-gateway/#sse-keepalive-pings) 付き）。
