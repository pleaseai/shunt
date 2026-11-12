---
title: UberLLM
description: UBERLLM_API_KEY でマッピングしたモデルを UberLLM の Messages エンドポイントへルーティングする。
---

**UberLLM** は OpenAI 互換エンドポイントと **Anthropic Messages** エンドポイントを持つ LLM
ゲートウェイです。1 つの API キーで Claude を含む多数のラボのモデルを利用できます。`/v1/messages`
を提供するため、shunt は Claude Code の Messages リクエストをそのまま転送し、キーを注入します。
組み込みの `uberllm` preset は `kind = "anthropic"`、`base_url = "https://api.uberllm.dev"`、
および `UBERLLM_API_KEY` からの API キー認証を提供します。

## クイックスタート

コーディングエージェントに任せる —— `shunt add` は組み込みのセットアップブループリントを表示します(オフライン・読み取り専用。設定を編集するのはエージェントで、コマンドは何も変更しません):

```bash
shunt add upstream uberllm --print | claude
```

または以下の手動手順に従ってください。

## アップストリームの設定

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルート未指定のモデル（例: claude-*）の既定として Anthropic を維持

[[upstreams]]
name = "uberllm"
provider = "uberllm"

[[routes]]
model = "deepseek/deepseek-v4.1-flash"
provider = "uberllm"
```

順序付き `[[upstreams]]` は shunt の組み込み provider を置き換えるため、設定はフォールバック先の
`anthropic` 既定も宣言する必要があります（`server.default_provider` の既定は `anthropic`）。

## 認証情報

```bash
export UBERLLM_API_KEY='...'
```

[uberllm.dev](https://uberllm.dev/signup) でキーを作成してください。キーを設定ファイルに
書き込まないでください。`shunt check` は設定構造のみを検証し、キーの値は読み取りません。
`UBERLLM_API_KEY` が未設定の場合、`uberllm` へルーティングされた最初のリクエストは認証エラーを
返します。

## モデル

モデル ID は `vendor/model` 形式です。公開リストは `https://api.uberllm.dev/v1/models`
(キー不要)、価格は [uberllm.dev/models](https://uberllm.dev/models) にあります。ID の例:

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
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4.1-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `uberllm` であることを確認し、
[Claude Code を shunt へ向けてください](/ja/guides/connect-claude-code/)。
