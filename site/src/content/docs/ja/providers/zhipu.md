---
title: Zhipu (GLM China)
description: ZHIPUAI_API_KEY で GLM Coding Plan モデルを Zhipu の Anthropic 互換 BigModel エンドポイントへルーティングする。
---

**Zhipu** は中国の BigModel プラットフォームで **GLM** モデルを **Anthropic 互換**
エンドポイントとして提供します。組み込みの `zhipu` preset は `kind = "anthropic"`、
`base_url = "https://open.bigmodel.cn/api/anthropic"`、および `ZHIPUAI_API_KEY`
からの API キー認証を提供します。

国際版 Z.ai エンドポイントは [Z.ai (GLM)](/ja/providers/zai/) を参照してください。ホストと認証情報は別です。

## アップストリームの設定

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルート未指定のモデル（例: claude-*）の既定として Anthropic を維持

[[upstreams]]
name = "zhipu"
provider = "zhipu"

[[routes]]
model = "glm-5.3"
provider = "zhipu"

[[routes]]
model = "glm-5.3-flash"
provider = "zhipu"
```

順序付き `[[upstreams]]` は shunt の組み込み provider を置き換えるため、設定はフォールバック先の
`anthropic` 既定も宣言する必要があります（`server.default_provider` の既定は `anthropic`）。

## 認証情報

```bash
export ZHIPUAI_API_KEY='...'
```

キーを設定ファイルに書き込まないでください。`shunt check` は設定構造のみを検証し、キーの値は読み取りません。
`ZHIPUAI_API_KEY` が未設定の場合、`zhipu` へルーティングされた最初のリクエストは認証エラーを返します。

## モデル

| モデル ID | 備考 |
| :-- | :-- |
| `glm-5.3` | GLM Coding Plan のフラッグシップテキストモデル |
| `glm-5.3-flash` | より高速なマルチモーダルティア。クライアントは `[1m]` を付けられますが、shunt はルートマッチ前に除去します |

Claude Code では `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、またはサブエージェントの
`model:` frontmatter でルーティングされた ID を選択します。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"glm-5.3-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `zhipu` であることを確認し、
[Claude Code を shunt へ向けてください](/ja/guides/connect-claude-code/)。
