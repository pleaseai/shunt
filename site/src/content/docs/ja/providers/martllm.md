---
title: MartLLM
description: MARTLLM_API_KEY でマッピングしたモデルを MartLLM の Messages エンドポイントへルーティングする。
---

**MartLLM** は OpenAI 互換エンドポイントと **Anthropic Messages** エンドポイントを持つ LLM
ゲートウェイです。1 つの API キーで Claude を含む多数のラボのモデルを利用できます。`/v1/messages`
を提供するため、shunt は Claude Code の Messages リクエストをそのまま転送し、キーを注入します。
組み込みの `martllm` preset は `kind = "anthropic"`、`base_url = "https://api.martllm.com"`、
および `MARTLLM_API_KEY` からの API キー認証を提供します。

## クイックスタート

コーディングエージェントに任せる —— `shunt add` は組み込みのセットアップブループリントを表示します(オフライン・読み取り専用。設定を編集するのはエージェントであり、コマンド自体は何も変更しません):

```bash
shunt add upstream martllm --print | claude
```

または以下の手動手順に従ってください。

## アップストリームの設定

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルート未指定のモデル（例: claude-*）の既定として Anthropic を維持

[[upstreams]]
name = "martllm"
provider = "martllm"

[[routes]]
model = "deepseek/deepseek-v4.1-flash"
provider = "martllm"
```

順序付き `[[upstreams]]` は shunt の組み込み provider を置き換えるため、設定はフォールバック先の
`anthropic` 既定も宣言する必要があります（`server.default_provider` の既定は `anthropic`）。

## 認証情報

```bash
export MARTLLM_API_KEY='...'
```

[martllm.com](https://martllm.com/signup) でキーを作成してください。キーを設定ファイルに
書き込まないでください。`shunt check` は設定構造のみを検証し、キーの値は読み取りません。
`MARTLLM_API_KEY` が未設定の場合、`martllm` へルーティングされた最初のリクエストは認証エラーを
返します。

## モデル

モデル ID は `vendor/model` 形式です。公開リストは `https://api.martllm.com/v1/models`
(キー不要)、価格は [martllm.com/models](https://martllm.com/models) にあります。ID の例:

| モデル ID | 備考 |
| :-- | :-- |
| `deepseek/deepseek-v4.1-flash` | DeepSeek のモデル |
| `anthropic/claude-sonnet-5` | Anthropic のモデル |
| `openai/gpt-5.4-mini` | OpenAI のモデル |

新しい ID をルーティングする前に最新のリストを確認してください。Claude Code では
`ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、またはサブエージェントの `model:`
frontmatter でルーティングされた ID を選択します。代わりに `/model` ピッカーへ表示するには、
`[models.upstream_model]` マップとともに `claude` プレフィックス付きのエイリアスを広告してください —
[Model Discovery](/ja/guides/model-discovery/) を参照。

## 検証

```bash
shunt check    # -> config ok
shunt run      # フォアグラウンドで動き続けます。下のリクエストは別のターミナルから実行してください

curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4.1-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `martllm` であることを確認し、
[Claude Code を shunt へ向けてください](/ja/guides/connect-claude-code/)。
