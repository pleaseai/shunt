---
title: Installation
description: Homebrew、cargo、ビルド済みバイナリ、またはソースから shunt をインストールする。
---

## Homebrew (macOS / Linux)

```bash
brew install pleaseai/tap/shunt
```

## Cargo

クレートは `shunt-gateway` として公開されています（バイナリは引き続き `shunt`）。

```bash
cargo install shunt-gateway
```

## ビルド済みバイナリ

各 [GitHub リリース](https://github.com/pleaseai/shunt/releases)には、macOS（arm64/x64）と Linux（arm64/x64）のスタンドアロンバイナリ、および `SHA256SUMS` ファイルが添付されています。

```bash
# Selects the right asset for your OS/CPU (darwin/linux × arm64/x64)
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m); case "$ARCH" in x86_64) ARCH=x64 ;; aarch64) ARCH=arm64 ;; esac
curl -fsSLO "https://github.com/pleaseai/shunt/releases/latest/download/shunt-${OS}-${ARCH}"
chmod +x "shunt-${OS}-${ARCH}" && mv "shunt-${OS}-${ARCH}" /usr/local/bin/shunt
```

## ソースから

`cargo` を備えた stable Rust が必要です。

```bash
git clone https://github.com/pleaseai/shunt
cd shunt
cargo build --release   # -> target/release/shunt
```

`cargo run -- <args>` でソースツリーから直接実行することもできます。

## Claude Code に接続するための前提条件

- **Claude Code** — 最近のバージョンならどれでも。[model discovery](/ja/guides/model-discovery/) が必要な場合のみ v2.1.129+。
- マッピングするプロバイダーの認証情報:
  - `openai` プロバイダー向けの **OpenAI API キー**、または
  - `codex` プロバイダー向けの、Codex CLI（`codex login`）による **ChatGPT ログイン**。
- 通常の **Anthropic 認証情報**（claude.ai ログインまたは API キー） — shunt はマッピング*しない*すべてのモデルについてこれを転送します。

次へ: [Quickstart](/ja/getting-started/quickstart/)。
