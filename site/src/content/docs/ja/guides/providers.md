---
title: Providers
description: 組み込みプロバイダーと、TOML テーブルで任意の Anthropic 互換バックエンドを追加する方法。
---

プロバイダーは**名前 → 設定のマップ**です。新しい上流は `[providers.<name>]` テーブルをもう 1 つ書くだけ — コード変更は不要です。3 種類のアダプターですべてをカバーします。

- **`kind = "anthropic"`** — 上流が Anthropic Messages API を話します。shunt はリクエストをパススルーし、オプションで別の API キーを注入します。
- **`kind = "responses"`** — 上流が OpenAI Responses API を話します。shunt は Anthropic Messages ⇄ Responses を、ストリーミングを含めて変換します。
- **`kind = "cursor"`** — ネイティブな Cursor アダプター。shunt は Cursor の ConnectRPC/protobuf AgentService（およびそのツールプロトコル）を Anthropic Messages API へ、ストリーミングを含めてブリッジします。組み込みの `cursor` プロバイダーが使用します。

## 組み込みプロバイダー

| 名前 | Kind | 認証 | バックエンド |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` — 呼び出し元自身の認証情報を転送 |
| `openai` | `responses` | `api_key`（`OPENAI_API_KEY`） | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` — `~/.codex/auth.json` を再利用 |
| `xai` | `responses` | `api_key`（`XAI_API_KEY`） | `api.x.ai/v1` — xAI 開発者 API |
| `grok` | `responses` | `xai_oauth` | `cli-chat-proxy.grok.com/v1` — `shunt login xai` 経由の SuperGrok / X Premium+ サブスクリプション |
| `cursor` | `cursor` | `cursor_oauth` | `api2.cursor.sh` — `~/.shunt/cursor-auth.json`（`shunt login cursor`）を再利用 |

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

### cursor プロバイダー（Cursor サブスクリプション）

組み込みの `cursor` プロバイダーは、Cursor 自身の ConnectRPC/protobuf AgentService（`api2.cursor.sh`）を通じて、あなたの **Cursor** サブスクリプションに到達します — `kind = "cursor"` のネイティブアダプターが、ストリーミングと Cursor のネイティブなツール呼び出しを含めて Anthropic Messages との間で変換します。一度ログインしてください。

```bash
shunt login cursor
```

これは Cursor の OAuth フローを実行し、`~/.shunt/cursor-auth.json` を書き込みます。shunt はこれを読み込み、自動リフレッシュします。ファイルが存在しないか期限切れの場合、shunt は `authentication_error` を返し、`shunt login cursor` を実行するよう伝えます。

`cursor:*` のモデル id をこれにルーティングします — プロバイダーはデフォルトでシードされているため、`[providers.cursor]` テーブルは不要です。

```toml
[[routes]]
model = "cursor:gpt-5.5"
provider = "cursor"
```

**モデル id とエージェントモード。** プレフィックスが Cursor のエージェントモードを選択し、サフィックスが Cursor のモデル id です。

| 形式 | エージェントモード | 例 |
| :-- | :-- | :-- |
| `cursor:<id>` / `cursor-agent:<id>` | Agent | `cursor:gpt-5.5` |
| `cursor-plan:<id>` | Plan | `cursor-plan:gpt-5.5` |
| `cursor-ask:<id>` | Ask | `cursor-ask:gpt-5.5` |

レガシーな素の名前も受け入れられます: `cursor`、`cursor-agent`、`cursor-composer`、`cursor-composer-fast`（Agent）、`cursor-plan`、`composer-2.5`（Plan）、`cursor-ask`、`composer-2.5-fast`（Ask）。それ以外のモデル id は `invalid_request_error` で拒否されます。

:::note[オーバーライド]
`SHUNT_CURSOR_BASE_URL` はエンドポイントを、`SHUNT_CURSOR_AUTH_FILE` は認証情報のパスを、`SHUNT_CURSOR_CLIENT_VERSION` は `x-cursor-client-version` ヘッダーをオーバーライドします（Cursor が古いクライアントバージョンを拒否し始めた場合、再ビルドせずに更新できます）。`cursor_oauth` プロバイダーは HTTPS 経由の Cursor ホストに固定されます — `base_url` をオリジン外へ向けることは拒否され、ベアラートークンが漏洩しないようになっています。
:::

:::caution[自己責任]
非公式なクライアントから Cursor サブスクリプションを再利用するかどうかは、あなた自身の判断です — Cursor の利用規約やアカウントに対する措置に抵触する可能性があります。ご利用は自己責任でお願いします。
:::

### xai / grok プロバイダー（Grok）

2 つの組み込みプロバイダーが xAI の **Grok** モデルにアクセスします。認証情報で分かれます。**`grok`** は
OAuth 経由であなたの **SuperGrok / X Premium+** サブスクリプションを消費し（`shunt login xai`、トークン単位の課金なし）、一方 **`xai`** は従量制の開発者 API に対して `XAI_API_KEY` を使います。サブスクリプションの bearer と API キーは**互換ではありません** — それぞれ自身のプロバイダーに対してのみ機能します。

ログイン、両方のプロバイダーブロック、モデルスラッグ、オプトインのエフォートダイヤル、entitlement の落とし穴を含む完全なセットアップについては、専用の [xAI / Grok ガイド](/ja/guides/xai/)を参照してください。

## Anthropic 互換バックエンドを追加する

「Claude Code を X で使う」系のサードパーティゲートウェイのほとんどは Anthropic Messages 互換です。`auth = "api_key"` を伴う `kind = "anthropic"` で、違いは `base_url` とキーの環境変数だけです。すぐ使えるベース URL:

| プロバイダー | `base_url` | モデル ID の例 |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) を参照 |
| Mimo (Xiaomi) | `https://api.xiaomimimo.com/anthropic` | `mimo-v2.5-pro` — [Mimo docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) を参照 |
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

## サブエージェントプラグイン

[`pleaseai/shunt` マーケットプレイス](https://github.com/pleaseai/shunt/tree/main/plugins)は、各プロバイダーのモデルに固定された既製の Claude Code サブエージェントを出荷しています — モデルごとに 1 つのエージェントです。プラグインをインストールし、モデルを `@` メンションするか `CLAUDE_CODE_SUBAGENT_MODEL` を設定します。各エージェントの `model:` フロントマターはそのサブエージェントだけを迂回させ、メインセッションは Claude のまま残ります。

| プラグイン | モデル（各 1 エージェント） | プロバイダー |
| :-- | :-- | :-- |
| `shunt-codex` | `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna` | `codex`（ChatGPT サブスクリプション） |
| `shunt-xai` | `grok-build-0.1`、`grok-4.5`、`grok-4.3` | `xai`（API キー）または `grok`（サブスクリプション） |
| `shunt-kimi` | `kimi-k2.7-code` | `kimi` |
| `shunt-deepseek` | `deepseek-v4-pro`、`deepseek-v4-flash` | `deepseek` |
| `shunt-zai` | `glm-5.2`、`glm-4.7` | `zai` |
| `shunt-minimax` | `MiniMax-M3[1m]` | `minimax` |
| `shunt-mimo` | `mimo-v2.5-pro` | `mimo` |

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-xai@shunt
```

各プラグインは依然として `shunt.toml` でそのプロバイダーをルーティングし（上記のセクションを参照）、対応する認証情報をエクスポートする必要があります — プラグイン自身の README に正確なルートと環境変数が記載されています。grok モデルはどちらの xAI プロバイダーでも提供できます: `xai`（API キー、トークン単位で課金）または `grok`（`shunt login xai` 経由の SuperGrok / X Premium+ サブスクリプション。ティアでゲートされます — 403 の場合は `xai` にフォールバック）。
