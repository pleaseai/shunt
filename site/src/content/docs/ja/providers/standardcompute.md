---
title: Standard Compute
description: STANDARDCOMPUTE_API_KEY でマッピングしたモデルを Standard Compute の Messages エンドポイントへルーティングする。
---

**Standard Compute** は `https://api.stdcmpt.com/v1` で OpenAI 互換 API を提供する LLM
ゲートウェイです。`/v1/messages` で **Anthropic Messages** エンドポイントも提供するため、shunt は
Claude Code の Messages リクエストをそのまま転送し、キーを注入します。組み込みの
`standardcompute` preset は `kind = "anthropic"`、`base_url = "https://api.stdcmpt.com"`、
および `STANDARDCOMPUTE_API_KEY` からの API キー認証を提供します。

## クイックスタート

コーディングエージェントに任せる。`shunt add` は組み込みのセットアップブループリントを表示します(オフライン・読み取り専用。設定を編集するのはエージェントで、コマンドは何も変更しません):

```bash
shunt add upstream standardcompute --print | claude
```

または以下の手動手順に従ってください。

## アップストリームの設定

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルート未指定のモデル（例: claude-*）の既定として Anthropic を維持

[[upstreams]]
name = "standardcompute"
provider = "standardcompute"

[[routes]]
model = "StandardCompute"
provider = "standardcompute"
```

順序付き `[[upstreams]]` は shunt の組み込み provider を置き換えるため、設定はフォールバック先の
`anthropic` 既定も宣言する必要があります（`server.default_provider` の既定は `anthropic`）。

## 認証情報

```bash
export STANDARDCOMPUTE_API_KEY='...'
```

[standardcompute.com](https://standardcompute.com) でキーを作成してください。キーを設定ファイルに
書き込まないでください。`shunt check` は設定構造のみを検証し、キーの値は読み取りません。
`STANDARDCOMPUTE_API_KEY` が未設定の場合、`standardcompute` へルーティングされた最初の
リクエストは認証エラーを返します。

## モデル

| モデル ID | 備考 |
| :-- | :-- |
| `StandardCompute` | ルーティング型: 各リクエストを処理できる最も安価な有効モデルへ送り、自動フォールバックする |

Claude Code では `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、またはサブエージェントの
`model:` frontmatter でルーティングされた ID を選択します。代わりに `/model` ピッカーへ表示するには、
`[models.upstream_model]` マップとともに `claude` プレフィックス付きのエイリアスを広告してください。
[Model Discovery](/ja/guides/model-discovery/) を参照。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"StandardCompute","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `standardcompute` であることを確認し、
[Claude Code を shunt へ向けてください](/ja/guides/connect-claude-code/)。
