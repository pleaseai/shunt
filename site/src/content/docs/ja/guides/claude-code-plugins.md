---
title: Claude Code プラグイン
description: shunt の Claude Code プラグインを導入する — プールの余裕を表示する /shunt:usage mod と、shunt が迂回させるモデル向けのサブエージェントバンドル。
---

shunt は独自の Claude Code プラグインマーケットプレイスを提供しています。一度だけ追加してください。

```
/plugin marketplace add pleaseai/shunt
```

そこには 2 種類のプラグインがあります。ひとつは **`shunt` mod** — ゲートウェイ自身のプール使用量を報告するコマンドを追加します。もうひとつは **プロバイダーバンドル** — shunt が他のプロバイダーへ迂回させるモデル上で動くサブエージェントを追加します。

## `shunt` mod: `/shunt:usage`

```
/plugin install shunt@shunt
```

`/shunt:usage` は、ゲートウェイの [`GET /usage`](/ja/reference/endpoints/) エンドポイントから読み取った、共有アカウントプールの残りの余裕を出力します。

```
shunt: pool — degraded   http://127.0.0.1:3001

  5h    ▓▓▓▓▓▓░░░░  62% left   resets 04:11
  7d    ▓▓▓▓▓▓▓▓░░  81% left   resets Sun 01:11
  fable ▓▓░░░░░░░░  19% left   resets Sun 01:11

  claude  ok         5h  71%  7d  84%  fable  19%
  codex   exhausted  5h   0%  7d  40%  fable    —

  headroom left, averaged over the pool's accounts; a shared figure, not a promise about your next request
```

mod がコマンドに自分で応答するため、モデルへは何も送られず、応答にトークンはかかりません。

### 数値の読み方

`remaining` は、プールの総容量のうちまだ**未使用**の割合です。`62%` は余裕が 62% 残っているという意味であり、62% を使ったという意味ではありません。これはそのウィンドウを報告する無効化されていないアカウントに対する `mean(1 - utilization)` なので、使い切ったアカウント 9 つに新しいアカウント 1 つなら `100%` ではなく `10%` と読めます。

これはプール全体の集計であり、**予測ではありません**。ルーティングは可用性、モデル、セッションアフィニティ、優先度も考慮するため、健全な数値であっても次のリクエストが受け付けられる保証にはなりません。

| ウィンドウ | 対象 |
| ------- | -------------- |
| `5h`    | ローリング 5 時間のセッションウィンドウ |
| `7d`    | 共有の週次ウィンドウ |
| `fable` | Fable スコープの週次ウィンドウ（`7d_oi`） |

無効化されていないアカウントのどれもそのウィンドウを報告しない場合、そのウィンドウは `—` と表示されます。ChatGPT/Codex アカウントは `x-codex-*` レスポンスヘッダーから `5h` と `7d` を埋めますが、Fable スコープの独自シグナルは持ちません。

最初のブロックはプールされたすべてのプロバイダーにわたる集計で、その下の行はプールされたプロバイダーごとの同じ集計です。そのため 1 つのプロバイダーへルーティングされたセッションは、混合された数値ではなくそのプロバイダーの余裕を読めます。このエンドポイントがアカウント名、件数、優先度、アカウント単位の数値を運ぶことは決してありません — その詳細は管理者専用の `GET /admin/api/pool` の背後に留まります。

### 前提条件

1. [Claude Code の接続](/ja/guides/connect-claude-code/) と同じように、Claude Code をあなたのゲートウェイへ向けます。

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
   export ANTHROPIC_AUTH_TOKEN=<your client token>
   ```

2. エンドポイントを有効にします。`GET /usage` はオプトインで [`[server.auth]`](/ja/guides/shared-gateway/) を必要とするため、[設定](/ja/reference/configuration/) には両方のテーブルが存在しなければなりません。

   ```toml
   [server.auth]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

   `[server.auth]` はトークンを TOML ではなく環境変数から読み取ります。既定では `SHUNT_CLIENT_TOKENS` で、`name:token` のペア形式です。この変数が未設定の場合、ゲートウェイは起動に失敗します。上で `ANTHROPIC_AUTH_TOKEN` に設定したものと同じトークンを使って、ゲートウェイを実行する側で設定してください。

   ```bash
   export SHUNT_CLIENT_TOKENS="claude-code:<your client token>"
   ```

3. function hooks を有効にして Claude Code を実行します — この機能はアーリーアクセスです。

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

手順 3 がなくてもコマンド自体は存在しますが、直接応答する代わりに、ツール呼び出しでエンドポイントを読むようモデルに依頼する動作へフォールバックします。

### 何を読むか

mod は環境変数を 5 つ読み、どれにも書き込みません。既定では、セッションがすでにすべてのメッセージを送っているそのゲートウェイへセッション自身の認証情報を送るため、セッションがまだ使っていなかったホストへ到達することはありません。`SHUNT_BASE_URL` だけが意図的な例外で、指定したゲートウェイを代わりに参照します。base URL がまったく設定されていない場合は、Anthropic 自身の API を呼ぶのではなく、その旨を伝えます。

| 変数 | 用途 |
| -------- | ------- |
| `SHUNT_BASE_URL` | ゲートウェイの base URL。`ANTHROPIC_BASE_URL` を上書きします |
| `ANTHROPIC_BASE_URL` | このセッションがすでに経由しているゲートウェイ |
| `SHUNT_TOKEN` | クライアントトークン。下の 2 つを上書きし、`Authorization: Bearer` として送信されます |
| `ANTHROPIC_AUTH_TOKEN` | Claude Code が送るのと同じく `Authorization: Bearer` として送信 |
| `ANTHROPIC_API_KEY` | Claude Code が送るのと同じく `x-api-key` として送信 |

`SHUNT_BASE_URL` は、トラフィックを別のゲートウェイ経由でルーティングしながら、あるゲートウェイのプールを読むことを可能にするものです。

### なぜ `/usage` ではなく `/shunt:usage` なのか

`/usage` は Claude Code 自身の組み込みコマンドであり、エンジンはプラグインが組み込みの名前を取ることを許しません。代わりにプラグインの markdown コマンドはプラグインによって名前空間が付くため、このコマンドは `shunt:usage` として出荷され、何とも衝突しません。

## プロバイダーサブエージェントプラグイン

これらは、shunt が別のプロバイダーへルーティングするモデル id に固定されたサブエージェントを追加します。セッションは Claude Code のハーネス内で動き続け — 同じツール、同じスキル — 迂回するのはトークン生成だけです。

| プラグイン | モデル | セットアップ |
| ------ | ------ | ----- |
| `shunt-codex` | GPT-6 Sol · Luna、GPT-5.6 Sol · Terra · Luna | [ChatGPT / Codex](/ja/guides/codex/) |
| `shunt-xai` | Grok 4.6 · 4.5 · Build | [xAI / Grok](/ja/guides/xai/) |
| `shunt-kimi` | Kimi K2.7 Code · K3 | [Kimi](/ja/providers/kimi/) |
| `shunt-deepseek` | DeepSeek V4 Pro · Flash | [DeepSeek](/ja/providers/deepseek/) |
| `shunt-zai` | GLM 5.2 · 4.7 | [Z.ai](/ja/providers/zai/) |
| `shunt-minimax` | MiniMax-M3 | [MiniMax](/ja/providers/minimax/) |
| `shunt-mimo` | MiMo V2.5 Pro | [MiMo](/ja/providers/mimo/) |

インストールも同じ方法です。

```
/plugin install shunt-codex@shunt
```

いずれも、ゲートウェイ設定で対応するモデル id がそのプロバイダーへルーティングされている必要があります。そうでなければ Claude Code はモデル id をそのまま Anthropic へ送り、リクエストは失敗します。

## 注意点

function hooks はアーリーアクセスです。hooks モジュールは function hooks が有効な環境でのみロードされ、`shunt` mod が対象としている API は Claude Code のリリース間で予告なく変わる可能性があります。プロバイダーバンドルは function hooks を使わないため、影響を受けません。
