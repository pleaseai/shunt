---
title: Cheaper Inference
description: CHEAPER_INFERENCE_API_KEY でマッピングしたモデルを Cheaper Inference の Messages エンドポイントへルーティングする。
---

**Cheaper Inference** は OpenAI 互換の LLM ゲートウェイです。1 つの API キーで複数のラボのモデルを
利用できます。`/v1/messages` で **Anthropic Messages** エンドポイントも提供するため、shunt は
Claude Code の Messages リクエストをそのまま転送し、キーを注入します。組み込みの
`cheaperinference` preset は `kind = "anthropic"`、
`base_url = "https://api.cheaperinference.com"`、および `CHEAPER_INFERENCE_API_KEY`
からの API キー認証を提供します。

## クイックスタート

コーディングエージェントに任せる —— `shunt add` は組み込みのセットアップブループリントを表示します(オフライン・読み取り専用。設定を編集するのはエージェントで、コマンドは何も変更しません):

```bash
shunt add upstream cheaperinference --print | claude
```

または以下の手動手順に従ってください。

## アップストリームの設定

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルート未指定のモデル（例: claude-*）の既定として Anthropic を維持

[[upstreams]]
name = "cheaperinference"
provider = "cheaperinference"

[[routes]]
model = "gpt-5.4"
provider = "cheaperinference"
```

順序付き `[[upstreams]]` は shunt の組み込み provider を置き換えるため、設定はフォールバック先の
`anthropic` 既定も宣言する必要があります（`server.default_provider` の既定は `anthropic`）。

## 認証情報

```bash
export CHEAPER_INFERENCE_API_KEY='...'
```

[cheaperinference.com](https://cheaperinference.com) でキーを作成してください。キーを設定ファイルに
書き込まないでください。`shunt check` は設定構造のみを検証し、キーの値は読み取りません。
`CHEAPER_INFERENCE_API_KEY` が未設定の場合、`cheaperinference` へルーティングされた最初の
リクエストは認証エラーを返します。

## モデル

モデル ID にはベンダープレフィックスが付きません。公開リストは
`https://api.cheaperinference.com/public/models` にあります。ID の例:

| モデル ID | 備考 |
| :-- | :-- |
| `gpt-5.4` | OpenAI のモデル |
| `deepseek-v4-flash` | DeepSeek のモデル |
| `glm-5.3` | Z.ai のモデル |

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
  -d '{"model":"gpt-5.4","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `cheaperinference` であることを確認し、
[Claude Code を shunt へ向けてください](/ja/guides/connect-claude-code/)。
