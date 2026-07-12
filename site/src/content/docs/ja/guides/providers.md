---
title: Providers
description: 組み込みプロバイダーと、TOML テーブルで任意の Anthropic 互換バックエンドを追加する方法。
---

プロバイダーは**名前 → 設定のマップ**です。新しい上流は `[providers.<name>]` テーブルをもう 1 つ書くだけ — コード変更は不要です。2 種類のアダプターですべてをカバーします。

- **`kind = "anthropic"`** — 上流が Anthropic Messages API を話します。shunt はリクエストをパススルーし、オプションで別の API キーを注入します。
- **`kind = "responses"`** — 上流が OpenAI Responses API を話します。shunt は Anthropic Messages ⇄ Responses を、ストリーミングを含めて変換します。

## 組み込みプロバイダー

| 名前 | Kind | 認証 | バックエンド |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` — 呼び出し元自身の認証情報を転送 |
| `openai` | `responses` | `api_key`（`OPENAI_API_KEY`） | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` — `~/.codex/auth.json` を再利用 |

### codex プロバイダー（ChatGPT サブスクリプション）

Codex CLI で一度ログインすれば、shunt が `~/.codex/auth.json` を読み込み、自動リフレッシュします。

```bash
codex login
```

ファイルが存在しないか期限切れの場合、shunt は `authentication_error` を返し、`codex login` を実行するよう伝えます。

認証ファイルの扱い、モデル選択、エフォート、コンテキストサイズを含む完全なセットアップについては、専用の [ChatGPT / Codex ガイド](/ja/guides/codex/)を参照してください。

:::caution[モデルスラッグ]
ChatGPT アカウントの Codex バックエンドは `gpt-*-codex` スラッグを**拒否**します — アカウントがライブで entitle されているスラッグのみを受け入れます。信頼できるカタログは openai/codex の [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) です。現在のスラッグは `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`（フロンティア）と `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2` です。古いアカウントでは以前のものにしか entitle されていない場合があります。ルート内で `upstream_model` を使い、任意のエイリアスを entitle されたスラッグへマッピングしてください。
:::

## Anthropic 互換バックエンドを追加する

「Claude Code を X で使う」系のサードパーティゲートウェイのほとんどは Anthropic Messages 互換です。`auth = "api_key"` を伴う `kind = "anthropic"` で、違いは `base_url` とキーの環境変数だけです。すぐ使えるベース URL:

| プロバイダー | `base_url` | モデル ID の例 |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) を参照 |
| Mimo (Xiaomi) | `https://api-mimo.mi.com/anthropic` | [Mimo docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) を参照 |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8`（`x_api_key` を受け入れる） |

例えば、Kimi のモデルを shunt 経由でルーティングするには:

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "KIMI_API_KEY"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

そして `export KIMI_API_KEY=…` し、[Claude Code を shunt へ向け](/ja/guides/connect-claude-code/)、`kimi-k2.7-code` を選択します（`ANTHROPIC_CUSTOM_MODEL_OPTION` または `ANTHROPIC_MODEL` 経由）。`shunt check` を実行して検証してください — ルート内の未知のプロバイダー、`api_key_env` の欠落、不正な `base_url` を報告します。

すべてのプロバイダーキー（`kind`、`auth`、`api_key_header`、`count_tokens`、…）は [Configuration Reference](/ja/reference/configuration/) に記載されています。
