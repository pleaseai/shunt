---
title: API Route
description: 自分の API キーで API Route にモデルをルーティングする。
---

[API Route](https://api-route.com) は LLM ゲートウェイです。この preset は `/v1/messages` の Anthropic Messages エンドポイントを使い、`API_ROUTE_API_KEY` で Bearer 認証を行います。API Route の利用料金は Claude のサブスクリプションとは別です。

## 設定

```bash
shunt add upstream api-route --print | claude
```

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "api-route"
provider = "api-route"

[[routes]]
model = "gpt-6.1-sol"
provider = "api-route"
```

未指定モデルのデフォルトとして Anthropic upstream を残してください。順序付き upstream と旧形式の provider テーブルを混在させないでください。

## 認証情報

```bash
export API_ROUTE_API_KEY='...'
```

[API Route](https://api-route.com) で自分のキーを作成してください。設定ファイルやバージョン管理にキーを保存しないでください。`shunt check` は設定構造のみを検証し、認証情報は検証しません。

## モデルと検証

キーを使ってモデル一覧を取得します。利用可能なモデルはアカウントによって異なります。別のモデルを選ぶ前に Messages 対応を確認してください。

```bash
curl -sS https://global.api-route.com/v1/models \
  -H "Authorization: Bearer $API_ROUTE_API_KEY"
```

```bash
shunt check
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-6.1-sol","max_tokens":32,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

応答の `x-gateway-upstream: api-route` を確認してください。クライアント設定とモデル別名は [Claude Code の接続](/ja/guides/connect-claude-code/) と [モデル検出](/ja/guides/model-discovery/) を参照してください。
