---
title: クイックスタート
description: shunt を設定し、ゲートウェイを起動し、5 分で Claude Code をそこへ向ける。
---

この手順では、インストール済みの `shunt` バイナリから、`gpt-*` モデルが Claude Code 自身のハーネス内で走る Claude Code セッションまでを案内します。まず shunt をインストールしてください — [Installation](/ja/getting-started/installation/) を参照。

## 1. 設定

shunt はすべてのプロバイダーが事前設定済みで出荷されるため、最小限の設定ではルーティングを宣言するだけで済みます。`shunt.toml` を作成します（作業ディレクトリ、または `~/.config/shunt/shunt.toml`）。

```toml
# Exact model id -> provider
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"     # reuses your ChatGPT login via `codex login`

# Or send every gpt-* id to the OpenAI API
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"    # uses OPENAI_API_KEY
```

検証します。

```bash
shunt check
# -> config ok
```

## 2. プロバイダーの認証情報を用意する

ルーティング先のプロバイダーを選びます。

```bash
codex login                     # codex provider: ChatGPT subscription login
# or
export OPENAI_API_KEY=sk-...    # openai provider: API key
```

## 3. ゲートウェイを起動する

```bash
shunt run
# -> shunt listening on 127.0.0.1:3001
```

## 4. Claude Code をそこへ向ける

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1   # so /effort maps to reasoning.effort
claude
```

Claude Code 内で `/model` を実行し、`gpt-5.6-sol` を選びます。マッピングされていないモデル（あなたのすべての `claude-*` id）は、これまでとまったく同じように動作します。shunt はあなた自身の認証情報を使って Anthropic へ転送します。

## 5. 検証

Claude Code を開く前に（あるいは開かずに）ゲートウェイを直接テストします。

```bash
# Mapped model -> diverted to the provider (uses shunt's provider credential)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

`{"id":"msg_` で始まる JSON レスポンスが返れば成功です。Claude Code 内では、`/status` で **Anthropic base URL** が `http://127.0.0.1:3001` と表示されるはずです。

## 次はどこへ

- [Configuration](/ja/guides/configuration/) — 設定ファイル、環境変数オーバーライド、ルーティング優先順位。
- [Providers](/ja/guides/providers/) — Kimi、DeepSeek、GLM、OpenRouter などのバックエンドを追加。
- [Connect Claude Code](/ja/guides/connect-claude-code/) — 認証情報の詳細、エージェント単位のルーティング。
- [Troubleshooting](/ja/reference/troubleshooting/) — よくあるエラーと対処法。
