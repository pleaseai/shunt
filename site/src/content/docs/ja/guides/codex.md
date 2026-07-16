---
title: ChatGPT / Codex
description: ~/.codex/auth.json を再利用して Claude Code の推論を ChatGPT/Codex サブスクリプションへルーティングする — 認証、モデルスラッグ、エフォート、コンテキストウィンドウ。
---

**`codex`** プロバイダーは、マッピングされたモデルの推論を、API キーではなくあなたの **ChatGPT / Codex
サブスクリプション**へルーティングします。Codex CLI がすでに `~/.codex/auth.json` に書き込んだ認証情報を再利用するため、貼り付けるものは何もなく、トークン単位の課金もありません — リクエストはあなたの ChatGPT アカウントとして認証され、`codex` CLI が話すのと同じバックエンドによって応答されます。

このページはエンドツーエンドのセットアップです。各トピックを繰り返すのではなく、より深いトピックページ（[Effort & Context](/ja/guides/effort-and-context/)、[Model Discovery](/ja/guides/model-discovery/)、[Providers](/ja/guides/providers/)）へリンクします。

## 仕組み

`codex` は組み込みの **`kind = "responses"`** プロバイダーです。shunt は Claude Code の Anthropic
Messages リクエストを OpenAI **Responses API** へ変換し、ChatGPT アカウントの Codex バックエンドへ送信し、ストリーミングされる応答を変換して返します。これを単なる OpenAI ではなく「Codex」たらしめているのは、次の 3 点です。

| 側面 | 値 |
| :-- | :-- |
| エンドポイント | `<base_url>/codex/responses` |
| 認証 | `~/.codex/auth.json` からの ChatGPT OAuth。自動リフレッシュ |
| Responses の方言 | `Chatgpt` フレーバー — codex が決して送らないパラメータ（例 `max_output_tokens`）を落とし、`store: false` を送信し、暗号化された reasoning をラウンドトリップする |

この方言はプロバイダー名ではなく `auth = "chatgpt_oauth"` によってキー付けされます。

Codex アカウントをプールすると、成功したバックエンド応答の `x-codex-*` レート制限ウィンドウが管理画面の **Pool health** にも表示されます。約 5 時間のウィンドウは **5h**、約 7 日のウィンドウは **7d** に表示され、日次・月次など未対応のウィンドウは無視されます。Codex には `7d_oi` 相当はありません。この使用量は表示専用であり、Codex のアカウント選択は引き続きクールダウンベースです。

## 1. ログイン

Codex CLI で一度ログインします。shunt は書き込まれたファイルを読み込み、リフレッシュします — Codex 用に独自のログインを**実行することはありません**。

```bash
codex login
```

これにより `~/.codex/auth.json` が作成されます。そのファイルが存在しないか、トークンがないか、リフレッシュトークンが失われている場合、shunt は `authentication_error` を返し、再度 `codex login` を実行するよう伝えます。

:::note[認証ファイルの場所を変える]
shunt は最初に `$CODEX_AUTH_FILE`、次に `$HOME/.codex/auth.json`、次に `.codex/auth.json` を見ます。CI、サンドボックス、または 2 つ目のアカウント向けに別の場所を指定できます。

```bash
export CODEX_AUTH_FILE=/etc/shunt/codex-auth.json
```
:::

## 2. プロバイダーブロック（オプション）

`codex` は組み込みです — 宣言する必要はありません。以下が完全なデフォルトです。部分的なテーブルは、設定したキーだけをオーバーライドします（設定マップはディープマージします）。

```toml
[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"   # shunt appends /codex/responses
auth = "chatgpt_oauth"                          # read + auto-refresh ~/.codex/auth.json
# effort = "high"                               # optional default reasoning effort (§4)
# count_tokens = "tiktoken"                      # default; "estimate" opts out
```

よくあるオーバーライド: すべての Codex トラフィックに対してデフォルトの `effort` を固定する、または `count_tokens = "estimate"` を設定する。`api_key_env` / `api_key_header` は `chatgpt_oauth` には適用されません — 認証情報は認証ファイルから来ます。すべてのキーについては [Configuration Reference](/ja/reference/configuration/#providersname) を参照してください。

:::note[ApiKey モードは `openai` プロバイダーへ行く]
`~/.codex/auth.json` が **`ApiKey`** モード（ChatGPT アカウントではなく OpenAI API キーでログインした場合）にあると、`codex` の OAuth パスはトークンを見つけられずエラーになります。そのキーは代わりに、`OPENAI_API_KEY` が未設定のときのフォールバックとして **`openai`** プロバイダーによって拾われます。`codex` は特に ChatGPT サブスクリプションのパスです。
:::

## 3. モデルを `codex` へルーティングする

リクエストの `model` id がプロバイダーを選びます。優先順位: 厳密な `[[routes]]` →
`[[route_prefixes]]` → `server.default_provider`。

```toml
[[routes]]
model = "gpt-5.6-sol"        # the id Claude Code sends (see §4 below)
provider = "codex"
# upstream_model = "gpt-5.6-sol"   # optional: forward a different slug upstream
# effort = "high"                  # optional: pin effort for this route
```

`upstream_model` により、Claude Code が送る id とバックエンドが受け取るスラッグを別にできます — これは [discovery エイリアス](/ja/guides/model-discovery/)の背後にある仕組みであり、Claude Code の環境に触れずに実際のスラッグを差し替える方法でもあります。

:::caution[モデルスラッグ — `-codex` は不可]
ChatGPT アカウントのバックエンドは `gpt-*-codex` スラッグ（例 `gpt-5.2-codex`）を `400` で**拒否**します。アカウントが**ライブで entitle されている**スラッグのみを受け入れます。信頼できるカタログ（および各スラッグが受け入れる reasoning レベル）は openai/codex の
[`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) です。現在のスラッグ: `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`（フロンティア）と
`gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`。古いアカウントでは以前のものにしか entitle されていない場合があります（無料アカウントは `gpt-5.5` に解決されています）。shunt はバックエンド自身のエラー `detail` を表面化するため、間違ったスラッグは本当の理由を返します。
:::

:::note[`Model not found <slug>` は entitlement ではなくクライアントバージョンのゲーティング]
一部のスラッグは `minimal_client_version` を持ちます（例 `gpt-5.6-luna` は ≥ 0.144.0 が必要）。リクエストのクライアント identity が欠落しているか古すぎる場合、バックエンドは `Model not found <slug>` と応答します。shunt は固定された Codex CLI の identity ヘッダー（`originator: codex_cli_rs`、`user-agent`、`version`）を **openai/codex rust-v0.144.4** に固定して送ることで、これを回避します。[openai/codex#31967](https://github.com/openai/codex/issues/31967) を参照してください。
:::

## 4. Claude Code でモデルを選択する

Claude Code の `/model` ピッカーは `claude`/`anthropic` で始まる discovery id のみを尊重するため、生の `gpt-*` id には 2 つのパスのいずれかが必要です — それらは `claude-` プレフィックスで分かれ、重複しません。

| | `claude-…` discovery エイリアス | 非 `claude-` id（`gpt-5.6-sol`） |
| :-- | :-- | :-- |
| discovery 経由の `/model` ピッカー | ✅ 自動リスト、多数のモデル | ❌ Claude Code が落とす |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ 尊重されない | ✅ ピッカーに追加（1 つの id） |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` ウィンドウ | ❌ 無視 → 200k | ✅ 実際のウィンドウ |

**主なパス** — スラッグをピッカーに直接追加する:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

その id はまさに shunt がルーティングに使うものなので、`[[routes]]`/`[[route_prefixes]]` ルールにマッチする必要があります。これが推奨パスです — 正確なコンテキストウィンドウも設定できる唯一のパスです。代わりに複数の Codex モデルをピッカーに自動リストするには、`claude-` 命名の [discovery エイリアス](/ja/guides/model-discovery/)を使います（200k ウィンドウのトレードオフを受け入れる）。

#### サブエージェントを Codex スラッグに乗せる

サブエージェントは、メインセッションが Claude のまま残る一方で、Codex スラッグ上で走れます。`model:` フロントマターフィールドは**任意の文字列**を受け入れます（Agent/Task ツールの `model` パラメータとは異なり、後者は組み込みエイリアスのみを取ります）。**既存の**サブエージェントを `gpt-5.6-sol` に向けるには、その `.claude/agents/<name>.md` を編集し `model:` を設定します。

```markdown
---
name: researcher
description: Deep research agent.
model: gpt-5.6-sol        # was: sonnet (or absent → inherited)
---

<the agent's system prompt — unchanged>
```

`model` オーバーライド**なし**でスポーンします（ツールパラメータがフロントマターより優先されます）。解決順序:
`CLAUDE_CODE_SUBAGENT_MODEL` > ツールの `model` > フロントマター > `inherit`。**すべての**サブエージェントを 1 つのスラッグに強制するには、`export CLAUDE_CODE_SUBAGENT_MODEL="gpt-5.6-sol"` を設定します。

いずれの場合も、スラッグには `[[routes]]` エントリが必要で、非 `claude-` であるため
`CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` と `CLAUDE_CODE_MAX_CONTEXT_TOKENS` に従います — ウィンドウは id に自動的に追随します。

:::tip[既製のエージェント]
**[`shunt-codex` プラグイン](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-codex)** は
`gpt-5.6-sol` / `-terra` / `-luna` 向けのサブエージェントを出荷しています — `/plugin marketplace add pleaseai/shunt` の後に
`/plugin install shunt-codex@shunt` でインストールします。
:::

### tier エイリアスを Codex へリマップする

1 つのカスタム id を追加する代わりに、Claude Code の**組み込み tier エイリアス**を Codex スラッグへ再度向けることで、セッション全体の tier システムをあなたの ChatGPT サブスクリプションへ解決させます
（[model-config 環境変数](https://code.claude.com/docs/en/model-config#environment-variables)）。

| 環境変数 | 制御対象 |
| :-- | :-- |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | `haiku` エイリアス**とバックグラウンドの「small-fast」モデル** |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | `sonnet` エイリアス |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_FABLE_MODEL` | `opus` / `fable` エイリアス |

2 段階のセットアップ — `haiku → gpt-5.6-luna`、`sonnet → gpt-5.6-sol`:

```bash
export ANTHROPIC_DEFAULT_HAIKU_MODEL="gpt-5.6-luna"
export ANTHROPIC_DEFAULT_SONNET_MODEL="gpt-5.6-sol"

# nicer picker labels (the _NAME/_DESCRIPTION companions work on a gateway)
export ANTHROPIC_DEFAULT_SONNET_MODEL_NAME="GPT-5.6-Sol"
export ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION="ChatGPT/Codex Sol via shunt"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME="GPT-5.6-Luna"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION="ChatGPT/Codex Luna via shunt (background tier)"
```

```toml
# shunt.toml — both resolved ids need a route
[[routes]]
model = "gpt-5.6-luna"
provider = "codex"

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

`/model` で **Sonnet** を選ぶと Codex 経由で `gpt-5.6-sol` が走り、すべてのバックグラウンド/haiku タスクは
`gpt-5.6-luna` が走ります — 解決された id はまさに shunt がルーティングに使うものなので、
`ANTHROPIC_CUSTOM_MODEL_OPTION` は不要です。

:::note[正しく設定するには]
- 解決された id は `claude-` で始まらないため、エフォートのダイヤルには `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` を設定します。`gpt-5.6-sol` と `gpt-5.6-luna` は**どちらも 372k** なので、1 つのグローバルな
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000` が両方の tier に収まります。
- `_SUPPORTED_CAPABILITIES` のコンパニオンはサードパーティプロバイダー（Bedrock、…）向けに文書化されていますが、ゲートウェイでは確認されていません — shunt ではエフォートに `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` を使ってください。
- **haiku tier はバックグラウンドの「small-fast」モデル**です（要約、タイトル、素早い分類）。それを reasoning モデルへルーティングしても構いませんが、その頻繁なトラフィックに ChatGPT のクォータを消費し、遅くなる可能性があります — それが問題なら、最も安価な entitle されたスラッグをそこに選んでください。
- リマップは**グローバルかつセッション全体**です。allowlist（`availableModels` /
  `enforceAvailableModels`）があると、エイリアスをリストの外へリダイレクトすることはできません（Claude Code は **v2.1.176** 時点で tier エイリアスの環境変数にこれを強制します）。
:::

## 5. 推論エフォート

エフォートは Claude Code の通常のコントロール（`/effort`、`/model` のスライダー、`--effort`）で設定します。shunt はそれを Responses の `reasoning.effort` へマッピングし、`max` をサポートしないスラッグでは `max → xhigh` に折りたたみます（サポートするのは **gpt-5.6** ファミリーのみ）。

:::note[カスタム id には必須]
Claude Code がエフォート対応と認識しない id（`gpt-5.6-sol` など）には、以下を設定する必要があります。

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```

そうしないと Claude Code はエフォートフィールドを省略し、shunt は `medium` にフォールバックします。設定の
`route.effort` / `[providers.codex].effort` オーバーライドはクライアント値より優先されます。
:::

完全な優先順位とエフォートの表: [Effort & Context](/ja/guides/effort-and-context/#reasoning-effort)。

## 6. コンテキストウィンドウ

Claude Code はマッピングされた id に対して、コンテキストバーを固定の **200k** でサイズします。`gpt-5.6-sol` の実際のウィンドウは **372k** です（`gpt-5.5` は 272k）。したがって、非 `claude-` id にはこれを引き上げてください。

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

これは**グローバル**（セッションごとに 1 つの値）で、実際のウィンドウより大きく設定すると
`prompt is too long` のオーバーフローによる無駄なやり取りが発生します — マッピングされたモデルのうち最も小さい実ウィンドウに合わせてください。shunt はそのオーバーフローを書き換えて Claude Code が自動コンパクトして再試行するようにしますが、各ラウンドトリップは無駄なレイテンシです。詳細、ライブ検証された境界、`count_tokens` の挙動:
[Effort & Context](/ja/guides/effort-and-context/#context--usage-display-for-mapped-models)。

## 完全な例

`shunt.toml`:

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"

[providers.codex]
effort = "high"     # optional: pin high effort for all Codex traffic

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

シェル（shunt と Claude Code の両方をこれらで実行します）:

```bash
codex login                                          # one-time
./target/release/shunt run                           # start the gateway

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"   # add to /model picker
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1            # let the effort slider reach Codex
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000         # gpt-5.6-sol's real window
```

`/model` から **gpt-5.6-sol** を選びます。セッション内のそれ以外のすべては引き続き変更なしで Anthropic へ流れます。マッピングされたモデルの推論だけが、あなたの ChatGPT/Codex サブスクリプションによって応答されます。

## ウェブ検索

Claude Code の組み込み **ウェブ検索** は、追加設定なしで Codex 経路で動作します。有効にすると Claude Code
はホスト型の `web_search_20250305` ツールを送り、shunt はそれを Responses API のホスト型
**`web_search`** ツールとして登録します。そのため検索は、未処理のツール呼び出しとして返されるのではなく、
バックエンドで実際に実行されます。

- ドメインフィルターはそのまま引き継がれます — Claude Code の `allowed_domains` / `blocked_domains`
  が Responses `web_search` の `filters` になります。
- `codex`(ChatGPT)および `openai`(標準 Responses)プロバイダーに適用されます。
- **xAI / Grok ルートは非対応** — Grok の Responses API は関数ツールのみを受け付けるため、shunt は
  ホスト型ウェブ検索ツールを削除します。ウェブ検索には `codex` または `openai` ルートを使ってください。

## ツール検索

Claude Code の **ツール検索** — MCP / LSP のツールスキーマを遅延させ、`ToolSearch` ツールで必要なときだけ
明らかにすることで、呼び出さないツールにコンテキストを使わせない機能 — も Codex 経路で動作しますが、shunt
の背後では **デフォルトで無効** です。有効化するには:

```bash
export ENABLE_TOOL_SEARCH=true
```

Claude Code は base URL がファーストパーティの Anthropic ホストでない場合、楽観的なツール検索を無効化します。
shunt はそれに該当しません。したがってこのフラグがないと、最初のターンからすべてのツールの完全なスキーマが
アップストリームへ送られ、機能が無意味になります（動作はしますが、何も削減されません）。クライアント自身の
規約は、**プロキシが `tool_reference` ブロックを転送するなら** `ENABLE_TOOL_SEARCH=true` を設定せよ、
というものであり、shunt はこれを転送します。

有効にすると、Claude Code は遅延可能なツールをプロンプトに **名前** だけ列挙し、スキーマは保留します。shunt は
まだロードされていないこれらのツールを、モデルが `ToolSearch` でロードするまでアップストリームのツール集合から
除外し、その結果生成される `tool_reference` が当該ツールの完全なスキーマを必要なときに明らかにします。これに
より、遅延したスキーマが最初のターンから占有していたはずのコンテキストウィンドウを取り戻します — ツール検索の
本来の目的です。

- `shunt.toml` の変更は不要です — 純粋に Claude Code の環境変数です。
- `codex`(ChatGPT)および `openai`(標準 Responses)プロバイダーに適用されます。
- 遅延しないツール(および上記のホスト型 `web_search` ツール)は常に転送されます。段階的に明らかにされるのは
  遅延可能なツールだけです。

### オプトインのネイティブプロトコル

上のシムは `tool_reference` をスキーマのテキストとしてレンダリングすることで動作します — アップストリーム
のコンテキストは何も削減されず、完全なスキーマを送るタイミングを遅らせているだけです。**オプトインの代替**
（issue #82）として、shunt はツール検索を OpenAI Responses API 自身の **ネイティブなクライアント実行
`tool_search`** プロトコルへマッピングできます。Claude Code の `ToolSearch` ツールは `tool_search`
（`execution: "client"`）ツールになり、その `tool_use` は `tool_search_call` になり、`tool_reference`
の結果はロードされたツールの完全なスキーマを構造化 JSON として運ぶ `tool_search_output` アイテムになり
ます — スキーマをテキストへ折り込む代わりに、実際のツールロードのセマンティクスとキャッシュの挙動を保持
します。プロバイダーごとに有効化します:

```toml
[providers.codex]
tool_search = true
```

要件 — 非対応の組み合わせはエラーにならず、静かに #43 のシムのままになります:

- アップストリームは標準の OpenAI または ChatGPT/Codex 系の Responses バックエンドである必要があります。
  xAI / Grok ルートは常にシムのままです。
- ルーティング先のモデルは **gpt-5.4 以降**（`gpt-5.4`、`gpt-5.5`、または `gpt-5.6` ファミリー）である
  必要があります。それより前のスラッグ（`gpt-5.2` 以下）は `tool_search = true` を設定していてもシムに
  フォールバックします。
- Claude Code 側で `ENABLE_TOOL_SEARCH=true` は引き続き必要です — このフラグは shunt がその機能をどう
  アップストリームへ変換するかだけを変えるもので、Claude Code がそもそもツールを遅延させるかどうかは
  変えません。

`tool_search` はデフォルトで `false` です。ネイティブな形状は、特定のバックエンドがそれを受け入れることを
ライブプローブで確認するまでこのフラグの背後にゲートされているため、shunt がすべての Codex/OpenAI ルート
を自動的に切り替えるのではなく、プロバイダーごとの明示的なオプトインになっています。

## トラブルシューティング

| 症状 | 原因 / 対処 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | `~/.codex/auth.json` がない（または `$CODEX_AUTH_FILE` が違う）。`codex login` を実行。 |
| `ChatGPT auth tokens missing` | 認証ファイルが `ApiKey` モード — それは `openai` プロバイダーです。ChatGPT アカウントで再度 `codex login`。 |
| `400 … not supported when using Codex with a ChatGPT account` | `gpt-*-codex` スラッグを使った。entitle された非 `-codex` スラッグを使う。 |
| `Model not found <slug>` | クライアントバージョンのゲーティングか entitle されていないスラッグ — `models.json` で確認。 |
| `gpt-*` id でエフォートスライダーが無視される | `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` を設定する。または route/provider の `effort` オーバーライドが優先されている。 |
| コンテキストバーが過大報告 / 早期にコンパクト | `CLAUDE_CODE_MAX_CONTEXT_TOKENS` を設定する。discovery エイリアスはこれを取れない — 非 `claude-` id を使う。 |
| Grok ルートでウェブ検索が何も返さない | xAI/Grok の Responses API はウェブ検索に非対応で、shunt がツールを削除します。ウェブ検索には `codex` または `openai` ルートを使う。 |
| ツール検索が効かない / 毎ターン全ツールのスキーマが送られる | `ENABLE_TOOL_SEARCH=true` を設定。Claude Code はファーストパーティでない base URL の背後ではツール検索をデフォルトで無効化します。shunt は `tool_reference` ブロックを転送し、遅延スキーマを必要なときに明らかにします。 |
| ツール検索を遅延だけでなく実際にコンテキスト削減したい | ネイティブプロトコルのため `[providers.codex]` に `tool_search = true` を設定する — 標準 OpenAI/ChatGPT-Codex 系と gpt-5.4 以降のモデルが必要。上記の [ツール検索 → オプトインのネイティブプロトコル](#オプトインのネイティブプロトコル) を参照。 |

さらに詳しくは完全な [Troubleshooting](/ja/reference/troubleshooting/) リファレンスを参照してください。
