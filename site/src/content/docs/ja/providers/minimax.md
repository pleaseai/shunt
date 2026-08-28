---
title: MiniMax
description: MiniMax-M3（1M コンテキスト）を、MINIMAX_API_KEY を使って MiniMax の Anthropic 互換エンドポイントへルーティングする。
---

**MiniMax** は自社のモデルを **Anthropic 互換**エンドポイントで提供します — shunt は Claude Code の
Messages リクエストをそのまま転送し、MiniMax の API キーを注入します。組み込みのプリセットはないため、
upstream で `kind` と `base_url` を明示的に宣言します。

## クイックスタート

コーディングエージェントにセットアップを任せることもできます — 名前付きブループリントのないプロバイダーでは、
`shunt add` がドキュメント URL を汎用のリサーチガイドに差し込みます（オフラインかつ読み取り専用で、設定を
編集するのはエージェントです。このコマンド自体は編集しません）。

```bash
shunt add upstream https://platform.minimax.io/docs/token-plan/claude-code --print | claude
```

または、以下の手順に沿って手動で設定してください。

## upstream を設定する

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルーティングされないモデル（例 claude-*）のデフォルトとして Anthropic を残す

[[upstreams]]
name = "minimax"
kind = "anthropic"
base_url = "https://api.minimax.io/anthropic"
auth = { mode = "api_key", env = "MINIMAX_API_KEY" }

[[routes]]
model = "MiniMax-M3"
provider = "minimax"
```

順序付きの `[[upstreams]]` は shunt の組み込みプロバイダーを置き換えるため、設定側でフォールバック先の
`anthropic` デフォルトも宣言する必要があります（`server.default_provider` のデフォルトは `anthropic`）。

従来の `[providers.minimax]` テーブル形式も引き続きサポートされます — ただし `[[upstreams]]` と
`[providers.*]` を 1 つのファイルで混在させないでください。

## 認証情報

```bash
export MINIMAX_API_KEY='...'
```

キーを設定ファイルに書き込まないでください。`shunt check` は設定の構造を検証しますが、キーの値は読み取り
ません — `MINIMAX_API_KEY` が未設定の場合、`minimax` へルーティングされた最初のリクエストが認証エラーを
返します。

## モデル

| モデル ID | 備考 |
| :-- | :-- |
| `MiniMax-M3` | 1M トークンのコンテキスト。クライアントが Claude Code の `[1m]` マーカーを付ける場合があります（`MiniMax-M3[1m]`。MiniMax 自身の [Claude Code インテグレーション](https://platform.minimax.io/docs/token-plan/claude-code)が記載しているスラッグです） — shunt はマッチングの前にこれを取り除くため、サフィックスなしの id をルーティングしてください |

ルーティング済みの id は、Claude Code で `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、または
サブエージェントの `model:` フロントマターから選択します。代わりに `/model` ピッカーへエントリを出したい
場合は、`[models.upstream_model]` マップとともに `claude` プレフィックス付きのエイリアスを広告してください —
[Model Discovery](/ja/guides/model-discovery/) を参照。マッピングする id は `[1m]` で終わっては**いけません** —
クライアントはマッチングの前にこのヒントを取り除くため、`MiniMax-M3[1m]` をキーにした `[[routes]]` エントリは
到達不能にもなります。常にサフィックスなしの `MiniMax-M3` をルーティングしてください。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `minimax` を示すことを確認したら、
[Claude Code を shunt へ向け](/ja/guides/connect-claude-code/)てください。

## サブエージェントプラグイン

[`shunt-minimax` プラグイン](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-minimax)は、上記の
モデル向けの既製の Claude Code サブエージェントを出荷しています。

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-minimax@shunt
```
