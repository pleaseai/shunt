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
| `GET` | `/routes` | shunt ネイティブのルート discovery — 設定された `[[routes]]` テーブルをそのまま返す（model → provider/upstream_model/effort のマッピング、claude プレフィックスの discovery エイリアスを含む）。`/v1/models` とは別物で、後者はより狭い Anthropic プロトコルの discovery レスポンス（`id`、`display_name`、およびアップストリームのモデルメタデータ）を提供する |
| `POST` | `/v1/messages` | 推論 — リクエストの `model` id に従ってルーティング |
| `POST` | `/v1/messages/count_tokens` | [トークンカウント](/ja/guides/effort-and-context/#token-counting-count_tokens) |
| `GET` | `/managed/settings` | ゲートウェイ JWT ごとの Claude Code managed settings。`ETag`、`If-None-Match`、`304 Not Modified` に対応 |
| `GET` | `/v1/organizations/spend_limits` | 保存された支出上限を方向付きカーソルページネーションで一覧表示 |
| `POST` | `/v1/organizations/spend_limits` | 1 つの `(scope, period)` に対する支出上限を作成または置換 |
| `GET` | `/v1/organizations/spend_limits/{id}` | 保存された支出上限を 1 件取得 |
| `DELETE` | `/v1/organizations/spend_limits/{id}` | 保存された支出上限を 1 件削除 |
| `POST` | `/v1/metrics` | 管理された Claude Code クライアントからのインバウンド OTLP/HTTP メトリクス — opt-in したゲートウェイテレメトリー宛先へ verbatim 中継 |
| `POST` | `/v1/logs` | インバウンド OTLP/HTTP log record — `logs = true` の宛先にのみ中継 |
| `POST` | `/v1/traces` | インバウンド OTLP/HTTP span — `traces = true` の宛先にのみ中継 |
| `GET` | `/admin` | 管理ダッシュボード（HTML）。未サインイン時は `/admin/login` へリダイレクト |
| `GET`, `POST` | `/admin/login` | 管理トークンのログインフォームとブラウザーセッションの作成 |
| `POST` | `/admin/logout` | ブラウザーセッションの破棄 |
| `GET` | `/admin/accounts` | Claude アカウントストアのメタデータ: 名前、種類、有効期限、UUID。トークン本体は決して返さない |
| `GET` | `/admin/accounts/codex` | Codex アカウントストアのメタデータ: 名前、有効期限、ChatGPT アカウント ID。トークン本体は決して返さない |
| `GET` | `/admin/pool` | `claude_oauth` / `chatgpt_oauth` provider ごとのプール状態。Codex はクォータヘッダーを送らないため使用率フィールドは空 |
| `POST` | `/admin/accounts/claude` | `{name, mode}` で Claude のブラウザープロビジョニングを開始。`mode` は `oauth` または `setup_token` で、省略時は `setup_token`。`{authorize_url}` を返す |
| `POST` | `/admin/accounts/claude/{name}/complete` | `<code>#<state>` を含む `{code}` で Claude プロビジョニングを完了。アカウントを保存し、有効（live）かどうかを報告 |
| `DELETE` | `/admin/accounts/claude/{name}` | 指定した Claude アカウントのストアファイルを削除 |
| `POST` | `/admin/accounts/codex` | `{name}` で ChatGPT OAuth を開始し、`{authorize_url}` を返す |
| `POST` | `/admin/accounts/codex/{name}/complete` | localhost の redirect URL 全体または `<code>#<state>` を含む `{code}` で Codex プロビジョニングを完了 |
| `DELETE` | `/admin/accounts/codex/{name}` | 指定した Codex アカウントのストアファイルを削除 |
| `POST` | `/backend-api/codex/responses` | Inbound Codex CLI パススルー — 実際の ChatGPT バックエンドパスをミラー |
| `POST` | `/responses` | Inbound Codex CLI パススルー — bare `base_url` 形式 |
| `POST` | `/v1/responses` | Inbound Codex CLI パススルー — `/v1` サフィックスの `base_url` 形式 |
| `POST` | `/backend-api/codex/analytics-events/events` | Codex CLI analytics sink — 受理して破棄し、サニタイズ済みイベント名のカウンターのみ記録 |
| `POST` | `/codex/analytics-events/events` | Codex CLI analytics sink — ルート形式の `chatgpt_base_url` |

`/admin*` ルートは [`[server.admin]`](/ja/reference/configuration/#serveradminオプション) が設定されている場合にのみ存在します。そのテーブルがなければ、いずれも登録されません。

spend-limit ルートは、起動時に [`[server.gateway.admin]`](/ja/reference/configuration/) が設定されていた場合にのみ存在します。すべての操作には `write_keys_env` の `x-api-key` を送信します。`read_keys_env` のキーは GET のみ使用できます。`POST` は `user` と `organization` の scope、`daily`／`weekly`／`monthly` の period、USD セントの非負整数文字列または `null` の `amount` を受け付け、`(scope, period)` 単位で upsert します。一覧では `limit`（1～1000、デフォルト 20）、`after_id`、`before_id`、`scope_type` を使用でき、2 つのカーソルは同時に指定できません。すべてのレスポンスに `request-id` が含まれ、エラーは Anthropic Admin API のエラー形式です。上限と変更監査レコードは、設定したバージョン付き JSON 状態ファイルに一緒に保存されます。ステージ 1 は `/effective` と `/audit` を公開せず、推論リクエストに上限を適用しません。

`GET /managed/settings` と `POST /v1/{metrics,logs,traces}` のテレメトリー受信ルートは、起動時に `[server.gateway]` が有効だった場合にのみ存在し、どちらも同じゲートウェイのベアラー JWT を要求します。受信ルートは、管理された Claude Code クライアントが export する OTLP/HTTP ペイロードを受け取り（[`[server.gateway.telemetry]`](/ja/reference/configuration/) がそれらの exporter をゲートウェイへ向けます）、リクエストのバイト列をその signal に opt-in したすべての宛先へそのまま中継します。インバウンドの `content-type` と `content-encoding` は保持され、宛先に設定された headers がその上に適用されます（設定されたキーは転送値を置き換え、ヘッダーを重複させません）。クライアントの `Authorization` ヘッダーが転送されることはなく、中継はリダイレクトに従いません。宛先は signal ごとに opt-in し（`metrics` はデフォルト on、`logs`／`traces` は off）、どの宛先も opt-in していない signal は受理後に破棄されます。中継はデタッチされているため、宛先の状態にかかわらずレスポンスは常に即座の `200` で、成功ボディは OTLP/HTTP に従いリクエストのプロトコルをミラーします（`application/json` には `{}`、それ以外には空の `application/x-protobuf` ボディ）。32 MiB の受信上限を超えるボディは `413` を返します。

Inbound Codex Responses と analytics のルートは [`[server.codex_endpoint]`](/ja/reference/configuration/) が設定されている場合にのみ存在します。Responses ルートは OpenAI Responses のリクエストとレスポンスをそのまま中継します。2 つの analytics ルートは同じ inbound auth ポリシーを適用し、クライアント payload を転送または保持せず、認証後は不正な JSON やサイズ超過の body にも `200 {}` を返します。サニタイズ済みイベント名だけを `shunt.codex_client_events` に記録し、metric sink がなければ純粋な破棄 sink として動作します。

`GET /` と `GET /health` は、[`[server.auth]`](/ja/guides/shared-gateway/) が有効なときも開いたままです（ヘルスチェックツールは通常トークンを付けられません）。機密情報は何も公開しません — ステータス、バージョン、およびすでに公開されているエンドポイント一覧のみです。

## ゲートウェイプロトコル

shunt は公式の [Claude Code LLM ゲートウェイプロトコル](https://code.claude.com/docs/en/llm-gateway-protocol)を実装します: 正しいヘッダーとボディフィールドの転送、機能のパススルー、システムプロンプトのアトリビューション処理。ゲートウェイ所有のエラーは Anthropic のエラー形で返され、上流のコンテキストオーバーフローエラーは Anthropic の `prompt is too long` の文言へ書き換えられて Claude Code の[コンパクト＆リトライ](/ja/guides/effort-and-context/#context-overflow-recovery)が発火し、ストリーミングレスポンスはバッファリングなしで中継されます（オプションで[キープアライブ ping](/ja/guides/shared-gateway/#sse-keepalive-pings) 付き）。
