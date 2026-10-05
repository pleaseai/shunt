---
title: Claude Code プラグイン
description: shunt の Claude Code プラグインを導入する — プロンプトの上に使用量を、/shunt:usage にプールの余裕を表示する mod と、shunt が迂回させるモデル向けのサブエージェントバンドル。
---

shunt は独自の Claude Code プラグインマーケットプレイスを提供しています。一度だけ追加してください。

```
/plugin marketplace add pleaseai/shunt
```

そこには 2 種類のプラグインがあります。ひとつは **`shunt` mod** — プロンプトの上のバンドに使用量（shunt 上ではゲートウェイのプール、それ以外ではセッション自身）を表示し、コマンドでプールの詳細を表示します。もうひとつは **プロバイダーバンドル** — shunt が他のプロバイダーへ迂回させるモデル上で動くサブエージェントを追加します。

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

### 使用量バンド

mod は使用量をプロンプトの上のバンドにも常に表示するので、尋ねなくても確認できます。shunt ゲートウェイ上では、`shunt` タグを付けてプールの使用量を表示します。

```
shunt · 5H 5% ↻1h 41m · WK 46% ↻1d 7h · Fable 31% ↻1d 7h ⚠ anthropic degraded
```

各数値はそのウィンドウの**使用量**です。コマンドが余裕として報告するプール全体の平均の `1 - remaining` で（下の「数値の読み方」を参照）、その後ろにそのウィンドウの最も早いリセットまでの残り時間が続きます。残量ではなく使用量で数えるのは、Claude Code 自身の `/usage` や、shunt でないときのバンドと同じように読めるようにするためです。数値は 70% 以上で黄色、90% 以上で赤になります。ステータスが `ok` でないプール対象のプロバイダーは末尾に表示され、`degraded` は黄色、`exhausted` と `capped` は赤です。プロバイダーの一覧がないときはプール自体のステータスを表示します。どのアカウントも報告していないウィンドウは省きます。

shunt でないときは、タグなしでセッション自身の Claude レート制限を表示します。ゲートウェイが設定されていないセッション、`GET /usage` に 404 やプールの報告でない応答を返すゲートウェイ、プールしているプロバイダーがないゲートウェイがこれに当たります。

```
5H 5% ↻1h 41m · WK 46% ↻1d 7h
```

shunt 上では、セッション開始時、その後は 1 分ごと、そして各ターンの終了後に `GET /usage` を読みます。このときセッション自身の制限は使いません。最後に応答したプールのアカウント 1 つの値だからです。トークンの拒否、ゲートウェイへの接続失敗、その他のエラーステータスは shunt 上の障害なので、別の値で置き換えず `shunt · ⚠ credential refused` のように表示します。ゲートウェイもレート制限もない場合（API キーで Anthropic に直接送る場合）、バンドは何も描きません。

そのセッションの間だけ隠すには、バンドの `[-]`（ctrl+x ctrl+a）で折りたたみます。完全にオフにするには、`/config` でプラグインの **Usage band** オプション（`usageBand`）をオフにします。すると mod は何もポーリングせず、`/shunt:usage` はそのまま応答します。

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

   `[server.gateway]` でログインを発行するゲートウェイは、`[server.auth]` の代わりにそのテーブルで `[server.usage]` を有効にできます。その場合、`shunt gateway claude` で起動したセッションにクライアントトークンは不要で、mod はそのセッションのゲートウェイのログインで認証します（下記参照）。

3. function hooks を有効にして Claude Code を実行します — この機能はアーリーアクセスです。

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

手順 3 がないとバンドは表示されず、コマンド自体は存在しますが、直接応答する代わりに、ツール呼び出しでエンドポイントを読むようモデルに依頼する動作へフォールバックします。

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

`shunt gateway claude` で起動したセッションは、上の変数に資格情報を持ちません。ランチャーがこれらの変数を取り除き、代わりに `apiKeyHelper` を `shunt gateway token` につなぐためです。そのため上の変数がどれも設定されていないとき、mod はマージされた Claude Code の設定を読み、`apiKeyHelper` が shunt 自身の `shunt gateway token`（名前だけでもパス付きでも）であれば、シェルを介さず直接実行し、出力されたゲートウェイのログイントークンを `Authorization: Bearer` として送ります。このトークンは 5 分間再利用し、再利用したトークンをゲートウェイが拒否したときはヘルパーをもう一度実行します。それ以外の `apiKeyHelper` は実行しないので、そのようなセッションでは代わりに `SHUNT_TOKEN` を export してください。ゲートウェイは `GET /usage` で、`[server.auth]` のクライアントトークンに加えてゲートウェイのログインも受け付けます。

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
