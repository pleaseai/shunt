---
title: OpenRouter
description: マッピングしたモデルを OpenRouter の Anthropic 互換エンドポイントへルーティングする — API キー 1 本で数百のモデル。
---

**OpenRouter** は多数のモデルベンダーを 1 本の API キーの背後に集約し、**Anthropic 互換**エンドポイントを
公開しています — shunt は OpenRouter のキーを注入し、Claude Code の Messages リクエストを転送します。
Anthropic 以外のスラッグ（`stealth/ox-alpha` など）については、OpenRouter のスキンが HTTP 400 で拒否する
遅延ツール関連のフィールド（`defer_loading`、`tool_search_tool_*`）を取り除きます。Anthropic の id
（`claude*`、`anthropic/*`）ではこれらのフィールドを保持します。組み込みのプリセットはないため、upstream で
`kind` と `base_url` を明示的に宣言します。

## クイックスタート

コーディングエージェントにセットアップを任せることもできます — 名前付きブループリントのないプロバイダーでは、
`shunt add` がドキュメント URL を汎用のリサーチガイドに差し込みます（オフラインかつ読み取り専用で、設定を
編集するのはエージェントです。このコマンド自体は編集しません）。

```bash
shunt add upstream https://openrouter.ai/docs --print | claude
```

または、以下の手順に沿って手動で設定してください。

## upstream を設定する

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルーティングされないモデル（例 claude-*）のデフォルトとして Anthropic を残す

[[upstreams]]
name = "openrouter"
kind = "anthropic"
base_url = "https://openrouter.ai/api"
auth = { mode = "api_key", env = "OPENROUTER_API_KEY" }

[[routes]]
model = "anthropic/claude-opus-4.8"
provider = "openrouter"
```

順序付きの `[[upstreams]]` は shunt の組み込みプロバイダーを置き換えるため、設定側でフォールバック先の
`anthropic` デフォルトも宣言する必要があります（`server.default_provider` のデフォルトは `anthropic`）。

従来の `[providers.openrouter]` テーブル形式も引き続きサポートされます — ただし `[[upstreams]]` と
`[providers.*]` を 1 つのファイルで混在させないでください。

## 認証情報

```bash
export OPENROUTER_API_KEY='...'
```

キーを設定ファイルに書き込まないでください。`shunt check` は設定の構造を検証しますが、キーの値は読み取り
ません — `OPENROUTER_API_KEY` が未設定の場合、`openrouter` へルーティングされた最初のリクエストが認証
エラーを返します。

## モデル

OpenRouter のモデル ID は `vendor/model` 形式のスラッグです（例 `anthropic/claude-opus-4.8`） —
[OpenRouter のモデルカタログ](https://openrouter.ai/models)を参照し、到達可能にしたいスラッグごとに
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

レスポンスの `x-gateway-upstream` ヘッダーが `openrouter` を示すことを確認したら、
[Claude Code を shunt へ向け](/ja/guides/connect-claude-code/)てください。
