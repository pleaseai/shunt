---
title: Effort とコンテキスト
description: マッピングされたモデルに対して、reasoning エフォート、トークンカウント、コンテキストインジケーターがどう振る舞うか。
---

## Reasoning エフォート

Claude Code のエフォートレベル（`/effort`、`/model` のスライダー、`--effort`、または `CLAUDE_CODE_EFFORT_LEVEL`）は `output_config.effort` リクエストフィールドとして送られ、shunt はマッピングされたモデルに対してそれを Responses の `reasoning.effort` へマッピングします。

| Claude Code のエフォート | → `reasoning.effort` |
| :-- | :-- |
| `low` / `medium` / `high` / `xhigh` | パススルー |
| `max` | 受け入れるモデル（**gpt-5.6** ファミリー）ではパススルー、それ以外は `xhigh` に折りたたむ |

Codex スラッグがどの reasoning レベルを受け入れるかは、openai/codex の [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)（`supported_reasoning_levels`）にモデルごとに記載されています。

:::note[カスタムモデル id にはフラグが必要]
`gpt-5.6-sol` のようなカスタムゲートウェイ id には `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` を設定する必要があります — そうしないと Claude Code は、エフォート対応と認識しないモデル id に対して `output_config.effort` を省略し、shunt は `medium` にフォールバックします。

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```
:::

shunt での優先順位: 設定の `route.effort` / `[providers.*].effort` オーバーライドが最初に優先されます。それ以外の場合はリクエストの `output_config.effort` が尊重されます。それ以外の場合は `thinking.enabled → high`、次にモデル名のサフィックス（`-xhigh`/`-high`/`-medium`/`-low`、`-spark` は `-low` として扱う）、それ以外は `medium`。

## トークンカウント（`count_tokens`）

**Anthropic へルーティングされる**モデルでは、shunt は `POST /v1/messages/count_tokens` を上流へパススルーします（正確なカウント）。**`responses` へルーティングされる**モデルには相当する上流エンドポイントがないため、プロバイダーの `count_tokens` 設定が決めます。

- **`count_tokens = "tiktoken"`（デフォルト）** — shunt は tiktoken の `o200k_base` エンコーダーでローカルにカウントを計算し、`{"input_tokens": N}` を返します。GPT ファミリーのモデルのテキストにはほぼ正確で、プロセス内（~ミリ秒）で応答されます — これは、Claude Code の `/context` が表示項目ごとに `count_tokens` 呼び出しを 1 回発行する（呼び出しごとに 30～50 回）ため重要です。
- **`count_tokens = "estimate"`（オプトイン）** — shunt は **404** を返します。これはゲートウェイプロトコルが明示的に許可しています。メインループのコンテキストバーはその後ローカルで推定しますが、`/context` は各カテゴリのカウントをネットワーク越しに Haiku に対して再実行します — 遅く、Anthropic の認証情報がないと静かに 0 トークンと報告されます。

いずれの場合も、リクエストが responses アダプターに届くことは決してないため、カウントリクエストが完全な推論呼び出しに変わって（課金されて）しまうことはありません。

## マッピングされたモデルのコンテキスト / 使用量表示

Claude Code は、アシスタントメッセージのトークン `usage` をモデルのコンテキストウィンドウサイズで割って、コンテキストインジケーターをローカルで計算します。`responses` プロバイダーへルーティングされたモデルの場合:

- **トークンカウント（分子）は正確です。** shunt は Responses の `usage` から `input_tokens`（およびキャッシュされたトークン）を読み取り、Anthropic の `message_delta` で転送し、キャッシュされた部分を `cache_read_input_tokens` に切り分けます。
- **ウィンドウ（分母）は、認識されない id に対してデフォルトで固定の 200k になります。** より大きな実ウィンドウを持つモデル（例 372k の `gpt-5.6-sol`）は、控えめで過大報告されたパーセンテージを示します — これは自動コンパクトが少し早めにトリガーされるだけです。

200k のデフォルトは `CLAUDE_CODE_MAX_CONTEXT_TOKENS`（Claude Code 2.1.205+）でクライアント側からオーバーライドできます。これは `claude-` で**始まらない**任意のモデル id に適用されます。

```bash
# e.g. gpt-5.6-sol's real window
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

オーバーライドは `claude-` で始まらない id **のみ**に適用されるため、[discovery エイリアス](/ja/guides/model-discovery/)（`claude-` で始まる*必要がある*）はこれを取れません — そのウィンドウは 200k デフォルトに固定されたままです。ピッカーでは便利ですが、正確なウィンドウが必要なときは非 `claude-` id（`ANTHROPIC_CUSTOM_MODEL_OPTION` 経由、または [tier エイリアスのリマップ](/ja/guides/codex/#remap-the-tier-aliases-to-codex)による）を使ってください。2 つのマッピングされた tier がウィンドウを共有するとき — `gpt-5.6-sol` と `gpt-5.6-luna` はどちらも 372k — 1 つのグローバル値が両方をカバーします。

:::caution
値は**グローバル**です — セッション内のすべての非 `claude-` モデルに 1 つの値 — で、実際の上流ウィンドウより大きく設定すると、リクエストが実際の上限をオーバーフローするまで自動コンパクトが遅れます。shunt はその[オーバーフローエラーを書き換える](#context-overflow-recovery)ため Claude Code は自動的にコンパクトして再試行しますが、各オーバーフローのラウンドトリップは無駄なレイテンシです — マッピングされたモデルのうち最も小さい実ウィンドウに値を合わせてください。

`gpt-5.6-sol`（実ウィンドウ 372k）のライブ検証された境界: 365k の入力トークンは正常に応答します。372k 超では、ストリーミングリクエストが自動コンパクトをトリガーする `prompt is too long` エラーを返します（`gpt-5.5` は 272000）。*非*ストリーミングリクエストは代わりに `input_tokens: 0` の空の `200` に劣化しますが、Claude Code のメインループは常にストリーミングします。
:::

もう 1 つのクライアント側のレバーは `[1m]` モデル id サフィックスで、これは 1M ウィンドウを強制します — 上流が本当にそのウィンドウを持つときだけ使ってください。（shunt はルートマッチングと転送の前に末尾の `[1m]` を除去するため、ヒントは純粋にクライアント側にとどまり、プロバイダーがそれを見ることはありません。）

| フィールド | マッピングされた（`responses`）モデル | Claude パススルー |
| :-- | :-- | :-- |
| 使用されたコンテキストトークン | ✅ 正確（shunt が転送） | ✅ 正確 |
| コンテキストウィンドウ（分母） | ⚠️ 200k デフォルト。`CLAUDE_CODE_MAX_CONTEXT_TOKENS` を設定 | ✅ 正確 |
| `count_tokens`（事前チェック） | ⚠️ ローカルの tiktoken カウント（デフォルト） | ✅ 正確（上流） |
| `rate_limits`（5h / 週次） | ❌ Anthropic ヘッダーが必要 | ✅ 表示される |

## コンテキストオーバーフローの回復

会話が上流モデルの実ウィンドウを超えると、プロバイダーは独自の文言でリクエストを拒否します — OpenAI の `context_length_exceeded`、`"This model's maximum context length is N tokens…"`、またはプロキシの `"prompt token count of N exceeds the limit of M"`。Claude Code の自動コンパクト＆リトライは Anthropic の言い回しにのみ発火するため、書き換えなければこれらのエラーはセッションを手動 `/compact` まで立ち往生させてしまいます（[文書化されたゲートウェイの落とし穴](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors)）。

shunt は `responses` へルーティングされたモデルのコンテキストオーバーフローエラーを検出し、Claude Code がマッチする Anthropic の形へ書き換えます。

```json
{"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long: 372982 tokens > 272000 maximum"}}
```

上流メッセージが両方のトークンカウントを伴うとき、shunt はそれらを保持します（上流がどの順序で述べても） — Claude Code は `N tokens > M maximum` のギャップをパースし、1 回の再試行でオーバーシュート全体を超えてコンパクトします。上流がカウントを与えないとき（例 Responses API の素の *"Your input exceeds the context window of this model"*）、shunt は `prompt is too long` だけを発行し、これでもコンパクトはトリガーされます。オーバーフロー以外のエラーは元のメッセージのままパススルーされます。

## アトリビューションブロック

Claude Code はアトリビューション行をシステムプロンプトの先頭に付加します。Anthropic は処理前にそれを除去しますが、shunt はそれを変更せず転送するため、マッピングされたプロバイダーはそれを `instructions` の最初の行として受け取ります。無害ですが、非 Anthropic モデルにとっては無意味なノイズです。これを落とすには:

```bash
export CLAUDE_CODE_ATTRIBUTION_HEADER=0
```

これはグローバルなので、Anthropic パススルーのトラフィック（コスト追跡に使われる）からもアトリビューションを除去します — 別のプロバイダーへルーティングしているなら問題ありません。
