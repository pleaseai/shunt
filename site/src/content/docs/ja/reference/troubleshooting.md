---
title: トラブルシューティング
description: よくある shunt のエラーとその修正方法。
---

| 症状 | 原因 / 対処 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | shunt が `~/.codex/auth.json` を読めない。`codex login` を実行。 |
| マッピングされたモデルで `authentication_error` | プロバイダー認証情報が期限切れ／不在 — `codex login` を再実行するか `OPENAI_API_KEY` をエクスポート。shunt はバックエンドの本当の `detail` メッセージを表面化します。 |
| `400 … model is not supported when using Codex with a ChatGPT account` | `-codex` スラッグ（またはアカウントが entitle されていないもの）を使った。[models.json](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) の entitle されたスラッグ（例 `gpt-5.6-sol`、`gpt-5.5`）を使うか `upstream_model` を設定。 |
| `/model` にモデルが表示されない | `gpt-*` id には `ANTHROPIC_CUSTOM_MODEL_OPTION` を使う。[discovery](/ja/guides/model-discovery/) は `claude`/`anthropic` プレフィックスの id のみを表面化します。 |
| `opus` で Opus 4.7／`sonnet` で Sonnet 4.6 が選択される | Claude Code の組み込みエイリアステーブルでは、gateway セッション用にこれらの tier が固定されています。`ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5` を使ってクライアント側で tier を固定するか、shunt で id をリマップしてください — [モデルエイリアス](/ja/guides/model-aliases/#エイリアスの解決)を参照。 |
| Opus／Fable のコンテキストウィンドウが 200K と表示される | Claude Code は base URL が `api.anthropic.com` の場合にのみ、モデルのネイティブ 1M ウィンドウを信頼します。`opus[1m]`／`fable[1m]` を選択してください — [モデルエイリアス](/ja/guides/model-aliases/#1m-コンテキストは自動的に適用されない)を参照。 |
| `/model` に Fable が表示されない、または `claude-fable-5` が Opus にフォールバックする | 単純な `ANTHROPIC_BASE_URL` の背後では Fable が除外されます。`ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5` を設定するか、[gateway login](/ja/guides/gateway-login/) を使ってください — [モデルエイリアス](/ja/guides/model-aliases/#fable-がピッカーから消える)を参照。 |
| Discovery が発火しない | ゲートウェイ認証情報（`ANTHROPIC_AUTH_TOKEN`、API キー、または `apiKeyHelper`）に加え `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` にゲートされています。`claude --debug` → `[gatewayDiscovery]` の行でデバッグ。 |
| `config check failed` | 正確な理由（バインドアドレス、ルート内の未知のプロバイダー、誤ったアダプター/認証）は `shunt check` を実行。 |
| Claude Code がログインを求めてくる | shunt がマッピングされていないモデル向けに転送できる Anthropic 認証情報（`ANTHROPIC_AUTH_TOKEN` / ログイン）を設定。base URL だけでは認証情報になりません。 |
| マッピングされたモデルでエフォートが `medium` に固定される | `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` を設定 — [Effort & Context](/ja/guides/effort-and-context/#reasoning-エフォート) を参照。 |
| マッピングされたモデルでツール検索が無効(毎ターン全ツールのスキーマが送られる) | `ENABLE_TOOL_SEARCH=true` を設定。Claude Code はファーストパーティでない base URL の背後で楽観的なツール検索を自動で無効化します。shunt は `tool_reference` ブロックを転送し、遅延スキーマを必要なときに明らかにします — [ChatGPT / Codex → ツール検索](/ja/guides/codex/#ツール検索) を参照。 |
| ツール検索は動くがコンテキストを削減しない(テキストシムのキャッシュ無効化コストを払ったまま) | ネイティブな `tool_search` がデフォルトになったため、フレーバー / モデルのゲートを確認してください — 標準 OpenAI または ChatGPT/Codex 系のプロバイダーが gpt-5.4 以降のモデルへルーティングしている必要があります。非対応のフレーバー/モデルは静かにテキストシムのままです。両方のゲートを満たしているのに削減されない場合は `tool_search = false` を設定していないか確認してください — [ChatGPT / Codex → ツール検索 → ネイティブプロトコル](/ja/guides/codex/#ネイティブプロトコル) を参照。 |
| マッピングされたモデルでコンテキスト長エラーの後にセッションが立ち往生 | shunt は上流のオーバーフローエラーを `prompt is too long …` へ書き換えるため Claude Code は自動コンパクトして再試行します — [コンテキストオーバーフローの回復](/ja/guides/effort-and-context/#コンテキストオーバーフローの回復) を参照。数ターンごとに再発する場合は `CLAUDE_CODE_MAX_CONTEXT_TOKENS` をモデルの実ウィンドウへ下げてください。 |
| Cloudflare の背後でストリームが切れる（524） | [`sse_keepalive_seconds`](/ja/guides/shared-gateway/#sse-キープアライブ-ping) を `0` ではなくデフォルト（30）のままにする。 |
| 共有ゲートウェイでマッピングされたモデルに 401 | クライアントトークンが欠落／無効 — `ANTHROPIC_AUTH_TOKEN=<token>`（`Authorization: Bearer` として受理、プール専用ゲートウェイ）または `ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <token>"`（パススルーモデルが混在する場合は必須）を設定。[ゲートウェイの共有](/ja/guides/shared-gateway/#インバウンドのクライアントトークン) を参照。 |
| Anthropic アダプターモデルで 429 | ゲートウェイログの `rate_limit_kind` を確認してください。`quota`（`retry-after`／`anthropic-ratelimit-*` ヘッダーあり）は実際のレート制限です。待機するか、並列負荷を減らしてください。`client-shape-rejection`（OAuth リクエスト、どちらのヘッダーもなく、body が単なる `"Error"`）は、Claude Code らしくない subscription OAuth リクエストを api.anthropic.com が拒否したことを意味します。Claude Code 以外のクライアントは OAuth トークンではなく API キーを使う必要があります。このエラーが集中すると、Claude Code の auto-mode classifier も機能しなくなる場合があります（「model temporarily unavailable」）。`no-ratelimit-headers`（非 OAuth 認証情報）は、レート制限メタデータのないプロバイダー 429 です。`quota` として扱ってください。 |
| 共有ゲートウェイで `503 overloaded_error` | ゲートウェイがインバウンド同時実行数の上限に達し、リクエストをキューに入れずに拒否しました（body メッセージは `too many requests are already in flight`、`Retry-After: 1` 付き）。これは上流の 503 ではなく shunt 自身の admission control です。上流の 503 はプロバイダー自身のメッセージのまま中継されます。指定時間後に再試行するか、並列負荷を減らすか、[`max_concurrent_requests`](/ja/guides/shared-gateway/#インバウンド同時実行数の上限) を上げて再起動してください（上限は起動時に固定されます）。 |

完全なゲートウェイのトラブルシューティング表については、[Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors) を参照してください。
