---
title: モデルディスカバリー
description: Claude Code の /model ピッカーを Claude 命名のエイリアスで自動的に埋める。
---

Discovery（`GET /v1/models`）は Claude Code の `/model` ピッカーを自動的に埋められます — **ただし Claude Code は `claude`/`anthropic` で始まらない id をすべて無視します**（[プロトコルリファレンス](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)）。したがって `gpt-*` id は何をしてもクライアント側で落とされます。discovery が役立つのは、`[[routes]]` エントリが実際の上流スラッグへ書き換える **Claude 命名のエイリアス**を公開するときだけです。

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

## Discovery にはゲートウェイの認証情報が必要

claude.ai OAuth の*ログイン*だけでは discovery はトリガーされません。Claude Code は `ANTHROPIC_AUTH_TOKEN`、API キー、または `apiKeyHelper` が設定されているときのみ `/v1/models` リクエストを発行します。素の Max/Pro サブスクリプションログインでは、フラグをオンにしても何も送りません — shunt に届くリクエストはなく、キャッシュも書かれません。[認証情報の選択](/ja/guides/connect-claude-code/#2-choose-the-anthropic-credential)を参照してください。`claude setup-token` が推奨ルートです。

## デバッグ

Discovery は**静かに**失敗し（3 秒のタイムアウト、リダイレクトはすべて失敗としてカウント）、キャッシュ／組み込みのリストにフォールバックします。`claude --debug` を実行し、`[gatewayDiscovery]` の行を探して実行されたか確認してください。
