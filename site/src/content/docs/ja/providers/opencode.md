---
title: OpenCode Zen
description: OPENCODE_API_KEY でマップされたモデルを OpenCode Zen の Anthropic 互換エンドポイントへルーティングする。
---

**OpenCode Zen** は OpenCode チームが厳選したモデルカタログ —— Anthropic、OpenAI、Google、xAI、Z.ai など各社のテスト済みモデルをひとつのエンドポイントで提供し、Zen API キーで従量課金されます。Zen は **Anthropic Messages** のワイヤ形式をネイティブに話します(`stream: true` の SSE を含む)ので、shunt は Claude Code の Messages リクエストをそのまま転送し、Zen キーを注入します。`opencode` プリセットは組み込みなので、設定は上流エントリ 1 行とルートだけです。

## クイックスタート

コーディングエージェントに任せる —— `shunt add` は組み込みのセットアップブループリントを表示します(オフライン・読み取り専用。設定を編集するのはエージェントで、コマンドは何も変更しません):

```bash
shunt add upstream opencode --print | claude
```

または以下の手動手順に従ってください。

## 上流の設定

`opencode` プリセットは `kind = "anthropic"`、`base_url = "https://opencode.ai/zen"`、`OPENCODE_API_KEY` からの API キー認証、そして zen エンドポイントが読む `x-api-key` ヘッダーを提供します:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 未ルーティングのモデル(例: claude-*)のために Anthropic デフォルトを保持

[[upstreams]]
name = "opencode"
provider = "opencode"

[[routes]]
model = "claude-fable-5-1-via-zen"
provider = "opencode"
```

順序付き `[[upstreams]]` は shunt の組み込みプロバイダを置き換えるので、`opencode` へルーティングする設定では、それが指す `anthropic` デフォルトも宣言する必要があります(`server.default_provider` のデフォルトは `anthropic`)。`default_provider` を宣言済み上流に変える場合のみ `anthropic` エントリを外してください。

従来の `[providers.opencode]` テーブル形式も引き続きサポートされますが、プリセットがこの形式を埋めることはありません —— 従来テーブルでは `kind`、`base_url`、`auth = "api_key"`、`api_key_env`、`api_key_header = "x_api_key"` を自分で記述する必要があります。`[[upstreams]]` と `[providers.*]` を同じファイルに混在させないでください。

## 認証情報

[opencode コンソール](https://opencode.ai/console)で API キーを作成し(または `opencode` CLI ログインが `~/.local/share/opencode/auth.json` に保存したキーを再利用し)、shunt を起動する環境でエクスポートします:

```bash
export OPENCODE_API_KEY='...'
```

キーを設定ファイルに書かないでください。`shunt check` は設定の構造のみを検証し、キーの値は読みません —— `OPENCODE_API_KEY` が未設定なら、`opencode` にルーティングされた最初のリクエストは認証エラーを返します。

Zen は `x-api-key` のみから資格情報を読みます —— `Authorization: Bearer` ヘッダーはキー不足として拒否されます。プリセットが正しいヘッダーを送るので設定は不要です。明示的な `auth = { mode = "api_key", header = "bearer" }` マップで上書きできます(zen が求めるものではありません)。

## モデル

Zen はクロスベンダーのカタログを提供します —— Claude、GPT、Gemini、Grok、GLM など。ライブリストは `https://opencode.ai/zen/v1/models` で公開されています。例:

| モデル id | 備考 |
| :-- | :-- |
| `claude-fable-5-1` | zen 上の Anthropic フロンティア階層 |
| `claude-opus-5` | zen 上の Anthropic 旗艦階層 |
| `gpt-6-astra` | zen 上の OpenAI 階層 |

モデルの可用性はカタログの入れ替わりで変わります —— 新しい id をルーティングする前にライブリストを確認し、告知の前にプランで id を検証してください。

Claude Code では `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、サブエージェントの `model:` frontmatter でルーティング id を選択します。`/model` ピッカーに表示するには、`[models.upstream_model]` マップで `claude` プレフィックスのエイリアスを告知してください —— [モデル発見](/ja/guides/model-discovery/)を参照。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-fable-5-1-via-zen","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `opencode` を指すことを確認し、[Claude Code を shunt に向ける](/ja/guides/connect-claude-code/)。

## 注意

- Zen の `/v1/messages/count_tokens` は存在しません:呼び出したクライアントは Anthropic のエラー形式ではなく zen の HTML 404 ページを受け取ります。これは zen 側のギャップで、shunt の `count_tokens` パススルーはそのまま到達します。Claude Code の `/context` はカウントエンドポイントが失敗すると自分のカウントにフォールバックするので、実際の影響は `/context` が遅くなることで、壊れることではありません。
- 402 `Payment Required` などの課金エラーは zen 自身の応答です —— 資格情報は機能しており、注意が必要なのはアカウント残高かプランです。
