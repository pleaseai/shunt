---
title: Troubleshooting
description: よくある shunt のエラーとその修正方法。
---

| 症状 | 原因 / 対処 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | shunt が `~/.codex/auth.json` を読めない。`codex login` を実行。 |
| マッピングされたモデルで `authentication_error` | プロバイダー認証情報が期限切れ／不在 — `codex login` を再実行するか `OPENAI_API_KEY` をエクスポート。shunt はバックエンドの本当の `detail` メッセージを表面化します。 |
| `400 … model is not supported when using Codex with a ChatGPT account` | `-codex` スラッグ（またはアカウントが entitle されていないもの）を使った。[models.json](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) の entitle されたスラッグ（例 `gpt-5.6-sol`、`gpt-5.5`）を使うか `upstream_model` を設定。 |
| `/model` にモデルが表示されない | `gpt-*` id には `ANTHROPIC_CUSTOM_MODEL_OPTION` を使う。[discovery](/ja/guides/model-discovery/) は `claude`/`anthropic` プレフィックスの id のみを表面化します。 |
| Discovery が発火しない | ゲートウェイ認証情報（`ANTHROPIC_AUTH_TOKEN`、API キー、または `apiKeyHelper`）に加え `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` にゲートされています。`claude --debug` → `[gatewayDiscovery]` の行でデバッグ。 |
| `config check failed` | 正確な理由（バインドアドレス、ルート内の未知のプロバイダー、誤ったアダプター/認証）は `shunt check` を実行。 |
| Claude Code がログインを求めてくる | shunt がマッピングされていないモデル向けに転送できる Anthropic 認証情報（`ANTHROPIC_AUTH_TOKEN` / ログイン）を設定。base URL だけでは認証情報になりません。 |
| マッピングされたモデルでエフォートが `medium` に固定される | `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` を設定 — [Effort & Context](/ja/guides/effort-and-context/#reasoning-effort) を参照。 |
| マッピングされたモデルでツール検索が無効(毎ターン全ツールのスキーマが送られる) | `ENABLE_TOOL_SEARCH=true` を設定。Claude Code はファーストパーティでない base URL の背後で楽観的なツール検索を自動で無効化します。shunt は `tool_reference` ブロックを転送し、遅延スキーマを必要なときに明らかにします — [ChatGPT / Codex → ツール検索](/ja/guides/codex/#ツール検索) を参照。 |
| ツール検索は動くがコンテキストを削減しない(シムは完全なスキーマの送信を遅らせるだけ) | ネイティブな Responses `tool_search` プロトコルにオプトインする — 標準 OpenAI または ChatGPT/Codex 系のプロバイダーで gpt-5.4 以降のモデルへルーティングしている場合、`[providers.<name>]` に `tool_search = true` を設定します。非対応のフレーバー/モデルは静かにテキストシムのままです — [ChatGPT / Codex → ツール検索 → オプトインのネイティブプロトコル](/ja/guides/codex/#オプトインのネイティブプロトコル) を参照。 |
| マッピングされたモデルでコンテキスト長エラーの後にセッションが立ち往生 | shunt は上流のオーバーフローエラーを `prompt is too long …` へ書き換えるため Claude Code は自動コンパクトして再試行します — [コンテキストオーバーフローの回復](/ja/guides/effort-and-context/#context-overflow-recovery) を参照。数ターンごとに再発する場合は `CLAUDE_CODE_MAX_CONTEXT_TOKENS` をモデルの実ウィンドウへ下げてください。 |
| Cloudflare の背後でストリームが切れる（524） | [`sse_keepalive_seconds`](/ja/guides/shared-gateway/#sse-keepalive-pings) を `0` ではなくデフォルト（30）のままにする。 |
| 共有ゲートウェイでマッピングされたモデルに 401 | クライアントトークンが欠落／無効 — `ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <token>"` を設定。[Sharing a Gateway](/ja/guides/shared-gateway/) を参照。 |

完全なゲートウェイのトラブルシューティング表については、[Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors) を参照してください。
