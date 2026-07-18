---
title: モデルディスカバリー
description: Claude Code の /model ピッカーを Claude 命名のエイリアスで自動的に埋める。
---

Discovery（`GET /v1/models`）は Claude Code の `/model` ピッカーを自動的に埋められます。デフォルトでは、shunt は管理者が選定した `[[models]]` エントリを先に返し、その後にリファレンス Claude apps gateway をミラーする組み込み Claude モデルカタログを追加します。同一 id は選定したエントリを優先して重複を除きます。選定したリストだけを公開するには、トップレベルで `auto_include_builtin_models = false` を設定してください。組み込みモデルは専用の `[[routes]]` エントリを必要としません。通常のルーティング規則で解決され、`[[routes]]` と `[[route_prefixes]]` のいずれにも一致しない場合は `server.default_provider` にフォールバックします。

Claude Code は discovery された id が `claude`/`anthropic` で始まらない場合、それを無視します（[プロトコルリファレンス](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)）。したがって `gpt-*` などの非 Claude モデルを選定リストへ追加するときは、**Claude 命名のエイリアス**を作り、`[[routes]]` エントリで実際の上流スラッグへ書き換えてください。

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"     # must begin with claude/anthropic
display_name = "GPT-5.6-Sol (via Codex)"

[[routes]]
model = "claude-gpt-5.6-sol-via-codex"  # the alias Claude Code sends
provider = "codex"
upstream_model = "gpt-5.6-sol"          # real slug forwarded to the ChatGPT backend
```

そして discovery を有効化し（Claude Code v2.1.129+）、shunt + Claude Code を再起動します。

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

エイリアスは `/model` に *From gateway* とラベル付けされて表示されます。それを選ぶと `claude-gpt-5.6-sol-via-codex` が送られ、shunt がそれを `codex` へルーティングし、`gpt-5.6-sol` へ書き換えます。

エイリアスのない `gpt-*` id には、代わりに `ANTHROPIC_CUSTOM_MODEL_OPTION` を使ってください — [Connect Claude Code](/ja/guides/connect-claude-code/#4-select-a-mapped-model) を参照。

## Claude Desktop は tier 名の id のみを認識します

Claude Code は `claude`/`anthropic` で始まる discovery id をすべて受け入れますが、**Claude Desktop はより厳格です**。`claude-sonnet-*`、`claude-opus-*`、`claude-haiku-*`、`claude-fable-*` といった tier 名の id のみを表示します。したがって上記の `claude-<slug>-via-<provider>` エイリアスは Claude Code には現れますが、`gpt` は tier 名ではないため **Claude Desktop では静かに破棄されます**。

組み込みカタログはすべて tier 名なので Desktop でも表示されたままです。失われるのは選定した `claude-<slug>-via-<provider>` エイリアスだけです。非 Anthropic バックエンドを Claude Desktop へ公開するには、tier 名の id を再利用し、`[[routes]]` の `upstream_model` でマッピングしてください。

```toml
[[routes]]
model = "claude-sonnet-5"        # a tier-named id Claude Desktop recognizes
provider = "codex"
upstream_model = "gpt-5.6-sol"   # real backend slug
```

Desktop でそれを選ぶと、意図した上流へ解決されます。この route はその id に対する組み込みカタログのデフォルトルーティングを上書きするため、バックエンドのマッピングがユーザーにとって意味を保つ tier 名を選んでください。

## Discovery にはゲートウェイの認証情報が必要

claude.ai OAuth の*ログイン*だけでは discovery はトリガーされません。Claude Code は `ANTHROPIC_AUTH_TOKEN`、API キー、または `apiKeyHelper` が設定されているときのみ `/v1/models` リクエストを発行します。素の Max/Pro サブスクリプションログインでは、フラグをオンにしても何も送りません — shunt に届くリクエストはなく、キャッシュも書かれません。[認証情報の選択](/ja/guides/connect-claude-code/#2-choose-the-anthropic-credential)を参照してください。`claude setup-token` が推奨ルートです。

## デバッグ

Discovery は**静かに**失敗し（3 秒のタイムアウト、リダイレクトはすべて失敗としてカウント）、キャッシュ／組み込みのリストにフォールバックします。`claude --debug` を実行し、`[gatewayDiscovery]` の行を探して実行されたか確認してください。
