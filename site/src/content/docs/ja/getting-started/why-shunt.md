---
title: shunt とは
description: shunt とは何か、他の Claude Code プロキシとどう違うか、いつ使うか。
---

`shunt` は仕様準拠の [Claude Code LLM ゲートウェイ](https://code.claude.com/docs/en/llm-gateway-protocol)です。透過的なプロキシとして、**マッピングしたモデル**についてのみ、推論を**推論レイヤー**で別の LLM プロバイダーへ振り分けます。リクエストの `model` id に基づいてルーティングし、デフォルトではそれ以外はすべて変更なしで Anthropic へパススルーします（これが「shunt」であり、フォールバック先は `server.default_provider` で設定可能です）。

この名前が仕組みそのものを表しています。電気回路や鉄道の *shunt*（分岐器）が、選んだ一部の流れを並行した経路へ振り分けるのと同じように、ここではマッピングされたモデルの推論を別のプロバイダーへ振り分けつつ、Claude Code のツールやスキルはそのまま保たれます。

## 仕組み

Claude Code はすべてのターンを Anthropic API へ送信します。`shunt` はその前段に（`ANTHROPIC_BASE_URL` を介して）位置し、マッピングしたモデルについてのみ、推論を別のプロバイダー（OpenAI、Codex/ChatGPT、…）へ振り分けます。ルーティングが HTTP/推論レイヤーで行われる — 別の CLI へタスクを引き渡すのではない — ため、セッションは Claude Code のハーネス内で走り続けます。同じツールループ、同じプリロード済みスキル、同じバンドルスクリプトのパス解決です。外部化されるのはトークン生成だけです。

これを、サブエージェントを別ランタイム（Codex CLI など）へ引き渡す方式と対比してください。そちらはスタックのより上層で切り替えるため、ペルソナとプリロード済みスキルが失われます。

## エージェント単位ではなくモデル単位 — そしてグローバルな一括切り替えでもない

ほとんどの Claude Code プロキシは、**すべての**トラフィックを 1 つの代替プロバイダーへルーティングします（グローバルなモデル一括切り替え）。`shunt` の焦点は、リクエストの `model` id によって駆動される**選択的でモデル単位**の振り分けです。メインセッションは Claude のまま残し、あなたが指名したモデルだけを他プロバイダーへ shunt します。

選択性は Claude Code 自身の中で決まります。Claude Code はすでにコンテキストごとにモデルを選べるようにしています。

- メインセッション向けの `/model` ピッカー、
- サブエージェント定義の `model:` フロントマター、
- すべてのサブエージェント向けの `CLAUDE_CODE_SUBAGENT_MODEL`、
- ピッカーにカスタムエントリを追加する `ANTHROPIC_CUSTOM_MODEL_OPTION`。

shunt は受け取ったモデル id を尊重するだけです — エージェントごとのシステムプロンプトの脆いフィンガープリンティングは不要です。その同じ選択性が、shunt が呼び出し元を一切詮索することなく、個々のエージェントにまで届きます。

## shunt が実装するもの

- **`POST /v1/messages`** — 推論。リクエストの `model` id に従ってルーティングされます。マッピングされていないモデルは、呼び出し元自身の認証情報を使ってバイト単位でそのまま Anthropic へ転送されます。
- **Anthropic Messages ⇄ OpenAI Responses 変換** — マッピングされた OpenAI ファミリーのモデル向け。ストリーミングを含みます。
- **ChatGPT サブスクリプションの再利用** — `codex` プロバイダーは Codex CLI の `~/.codex/auth.json` ログインを再利用（かつ自動リフレッシュ）します。
- **`GET /v1/models`** — Claude 命名のエイリアス向けの [model discovery](/ja/guides/model-discovery/)。
- **トークンカウント** — 変換されるプロバイダーにはローカルの tiktoken カウント、パススルーには上流の正確なカウント。
- **ストリーミングの堅牢性** — [SSE キープアライブ ping](/ja/guides/shared-gateway/#sse-keepalive-pings) により、Cloudflare のようなプロキシが長い推論の合間を切断しないようにします。
- **オプションのインバウンド認証** — 共有デプロイ向けの[クライアント単位トークン](/ja/guides/shared-gateway/)。

試す準備はできましたか？ [Installation](/ja/getting-started/installation/) へ進んでください。
