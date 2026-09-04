---
title: MiniMax China
description: MINIMAX_API_KEY で MiniMax-M3 を MiniMax 中国の Anthropic 互換エンドポイントへルーティングする。
---

**MiniMax China** は **MiniMax-M3** を **Anthropic 互換**エンドポイントで提供します。組み込みの
`minimax-cn` preset は `kind = "anthropic"`、
`base_url = "https://api.minimax.cn/anthropic"`、および `MINIMAX_API_KEY`
からの API キー認証を提供します。

国際版エンドポイントは [MiniMax](/ja/providers/minimax/) を参照してください。ホストと認証情報は別です。

## アップストリームの設定

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルート未指定のモデル（例: claude-*）の既定として Anthropic を維持

[[upstreams]]
name = "minimax-cn"
provider = "minimax-cn"

[[routes]]
model = "MiniMax-M3"
provider = "minimax-cn"
```

順序付き `[[upstreams]]` は shunt の組み込み provider を置き換えるため、設定はフォールバック先の
`anthropic` 既定も宣言する必要があります（`server.default_provider` の既定は `anthropic`）。

## 認証情報

```bash
export MINIMAX_API_KEY='...'
```

中国の MiniMax open platform のキーを使用してください。キーを設定ファイルに書き込まないでください。
`shunt check` は設定構造のみを検証し、キーの値は読み取りません。`MINIMAX_API_KEY` が未設定の場合、
`minimax-cn` へルーティングされた最初のリクエストは認証エラーを返します。

## モデル

| モデル ID | 備考 |
| :-- | :-- |
| `MiniMax-M3` | 1M トークンコンテキスト。クライアントは Claude Code の `[1m]` マーカーを付けられますが、shunt はマッチ前に除去するため、接尾辞なしの ID をルーティングしてください |

Claude Code では `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、またはサブエージェントの
`model:` frontmatter でルーティングされた ID を選択します。代わりに `/model` ピッカーへ表示するには、
`[models.upstream_model]` マップとともに `claude` プレフィックス付きのエイリアスを広告してください —
[Model Discovery](/ja/guides/model-discovery/) を参照。マッピングする ID は `[1m]` で終わっては**いけません**。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `minimax-cn` であることを確認し、
[Claude Code を shunt へ向けてください](/ja/guides/connect-claude-code/)。
