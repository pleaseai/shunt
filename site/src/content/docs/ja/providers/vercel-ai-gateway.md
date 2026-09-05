---
title: Vercel AI Gateway
description: マッピングしたモデルを、AI_GATEWAY_API_KEY を使って Vercel AI Gateway の Anthropic 互換エンドポイント経由でルーティングする。
---

**Vercel AI Gateway** は多数のモデルベンダーを 1 本のキーの背後に置き、**Anthropic 互換**エンドポイントを
公開しています — shunt は Claude Code の Messages リクエストをそのまま転送し、ゲートウェイのキーを注入します。
組み込みのプリセットはないため、upstream で `kind` と `base_url` を明示的に宣言します。

## クイックスタート

コーディングエージェントにセットアップを任せることもできます — 名前付きブループリントのないプロバイダーでは、
`shunt add` がドキュメント URL を汎用のリサーチガイドに差し込みます（オフラインかつ読み取り専用で、設定を
編集するのはエージェントです。このコマンド自体は編集しません）。

```bash
shunt add upstream https://vercel.com/docs/ai-gateway --print | claude
```

または、以下の手順に沿って手動で設定してください。

## upstream を設定する

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルーティングされないモデル（例 claude-*）のデフォルトとして Anthropic を残す

[[upstreams]]
name = "vercel"
kind = "anthropic"
base_url = "https://ai-gateway.vercel.sh"
auth = { mode = "api_key", env = "AI_GATEWAY_API_KEY" }

[[routes]]
model = "anthropic/claude-opus-4.8"
provider = "vercel"
```

順序付きの `[[upstreams]]` は shunt の組み込みプロバイダーを置き換えるため、設定側でフォールバック先の
`anthropic` デフォルトも宣言する必要があります（`server.default_provider` のデフォルトは `anthropic`）。

このゲートウェイは bearer 認証（デフォルト）と Anthropic の `x-api-key` ヘッダーの両方を受け入れます — 後者を
使いたい場合は auth マップに `header = "x_api_key"` を追加してください。従来の `[providers.vercel]` テーブル
形式も引き続きサポートされます — ただし `[[upstreams]]` と `[providers.*]` を 1 つのファイルで混在させない
でください。

## 認証情報

```bash
export AI_GATEWAY_API_KEY='...'
```

キーを設定ファイルに書き込まないでください。`shunt check` は設定の構造を検証しますが、キーの値は読み取り
ません — `AI_GATEWAY_API_KEY` が未設定の場合、`vercel` へルーティングされた最初のリクエストが認証エラーを
返します。

## モデル

AI Gateway のモデル ID は `vendor/model` 形式のスラッグです（例 `anthropic/claude-opus-4.8`） —
[AI Gateway のモデルカタログ](https://vercel.com/ai-gateway/models)を参照し、到達可能にしたいスラッグごとに
`[[routes]]` エントリを 1 つ追加してください。ルーティング済みの id は、Claude Code で `ANTHROPIC_MODEL`、
`ANTHROPIC_CUSTOM_MODEL_OPTION`、またはサブエージェントの `model:` フロントマターから選択します。代わりに
`/model` ピッカーへエントリを出したい場合は、`[models.upstream_model]` マップとともに `claude` プレフィックス
付きのエイリアスを広告してください — [Model Discovery](/ja/guides/model-discovery/) を参照。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"anthropic/claude-opus-4.8","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `vercel` を示すことを確認したら、
[Claude Code を shunt へ向け](/ja/guides/connect-claude-code/)てください。
