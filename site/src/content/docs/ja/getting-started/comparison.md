---
title: 比較
description: shunt と他の Claude Code ゲートウェイ・LLM プロキシとの比較 — ピアグループ、機能マトリクス、強み、意図的なスコープ境界。
---

shunt に最も近いツールとの、根拠に基づいた比較です。目的は shunt の設計境界を明示することです。意図的に*やらない*ことは何か、スコープ内の実質的な改善機会はどこにあるか。

:::note[スコープ]
shunt に関する主張は [shunt リポジトリ](https://github.com/pleaseai/shunt)の `file:line` を引用します。CLIProxyAPI に関する主張は `router-for-me/CLIProxyAPI@main` で検証しました。一般的なゲートウェイ（LiteLLM/Portkey/bifrost）に関する主張は、各プロジェクト自身が謳うレベルにとどめ、shunt 自身の README の「Related work」の位置づけと一致させています。
:::

## 1. shunt とは何か（そして何でないか）

shunt は**仕様準拠の Claude Code LLM ゲートウェイ**です。Claude Code の公式 `ANTHROPIC_BASE_URL` ゲートウェイ契約（`/v1/messages`、`/v1/models` discovery、アトリビューション/ヘッダーのパススルー）を実装し、**`model` id 単位の選択的**な迂回を行います — メインセッションは Claude に保ったまま、指名したモデルだけを別のプロバイダー（ChatGPT/Codex、OpenAI、xAI）へ振り向けます。マッピングされたモデルについては Anthropic Messages ⇄ OpenAI Responses API を変換し、それ以外はすべて Anthropic へ無変更でパススルーします。ルーティングは純粋にリクエストの `model` id によるもので、プロンプト形状のフィンガープリンティングはありません（`README.md:104-131`）。

この焦点が、以下すべての比較の軸です。shunt は広範なマルチテナントのフリート運用ではなく、**変換の忠実さと Claude Code ネイティブな挙動**に最適化しており、モデルを認識したプロアクティブなクォータローテーションとリアクティブなフェイルオーバーを組み合わせた Anthropic OAuth アカウントプールを備えます。

## 2. ピアグループ

| グループ | 例 | shunt との関係 |
|---|---|---|
| **サブスクリプション型 CC プロキシ（同一クラス）** | **raine/claude-code-proxy** | **総合的に最も近いピア** — Rust 単一バイナリ、`model` 単位ルーティング、Codex WebSocket + `previous_response_id` continuation、サブスクリプション OAuth バックエンド 4 つ（Codex/Kimi/Grok/Cursor） |
| **Codex OAuth を持つ広範な Claude Code プロキシ** | **CLIProxyAPI**（router-for-me） | 最も近い*広範な*ピア — Codex/ChatGPT OAuth、Codex WebSocket v2、ツール変換で重なる |
| **狭い Claude Code → Codex スワップ** | insightflo/chatgpt-codex-proxy | 同じ推論レイヤーのスワップ、単一バックエンド |
| **一般的な Claude Code ルーター** | musistudio/claude-code-router、1rgs/claude-code-proxy、fuergaosi233/claude-code-proxy | 通常はモデル単位の迂回ではなく*グローバル*なモデルスワップ |
| **一般的な AI ゲートウェイ** | LiteLLM、Portkey、bifrost、modelgate | 隣接インフラ — *バックエンド*にはなり得るが Claude Code ネイティブではない |

## 3. 機能マトリクス

凡例: ● 完全 · ◐ 部分的 / 回避策 · ○ なし · — 設計上対象外

| 機能 | shunt | raine/ccp | CLIProxyAPI | 一般 CC ルーター | 一般ゲートウェイ |
|---|:--:|:--:|:--:|:--:|:--:|
| Claude Code ゲートウェイプロトコル準拠（`/v1/models` discovery、アトリビューションのパススルー） | ● | ◐ | ◐ | ◐ | ○ |
| `model` id 単位の選択的迂回、メインセッションは Claude のまま（グローバルスワップではない） | ● | ◐³ | ◐ | ○ | ◐ |
| Anthropic Messages ⇄ OpenAI Responses 変換 | ● | ● | ● | ◐（ほぼ chat-completions） | ◐（chat-completions） |
| ChatGPT/Codex **サブスクリプション**（OAuth）バックエンド | ● | ●⁴ | ● | まれ | ○ |
| Codex **WebSocket** Responses トランスポート | ● | ● | ● | ○ | ○ |
| **変換パスでの**アップロードトリミング（`previous_response_id` continuation） | ● | ● | ○（パススルーのみ） | ○ | ○ |
| tool-search / `defer_loading` / `tool_reference` の処理 | ◐（シム: 動作するがコンテキスト節約なし。ネイティブはオプトイン⁸） | ○⁵ | ◐（上流）/ ●（フォーク） | ○ | ○ |
| Claude Code `thinking` への reasoning ラウンドトリップ | ●（暗号化） | ◐（Kimi/Grok。**Codex は破棄**） | ◐ | ○ | ◐ |
| マルチアカウントのロードバランシング / フェイルオーバー | ◐⁷ | ○ | ● | 一部 | ● |
| バックエンドの幅 | プロバイダー 4 つ¹ | サブスクリプション 4 つ⁶ | バックエンド 11² | さまざま | 100–1600+ |
| 管理 API / ダッシュボード | ◐（オプトインの管理サーフェス） | ◐（モニター TUI） | ● | 一部 | ● |
| 使用量 / クォータ / コストのトラッキング | ○（Sentry メトリクスのみ） | ○ | ● | 一部 | ● |
| プラグイン / インターセプターシステム | ○ | ○ | ● | 一部 | ● |
| 言語 / フットプリント | Rust、バイナリ 1 つ | Rust、バイナリ 1 つ | Go | Node/Python | Go/Node/Python |
| 設定モデル | TOML + env、ホットリロード | env + 設定ファイル | YAML + 管理 API | さまざま | YAML/UI |

¹ shunt: 2 種類のアダプター（`anthropic` パススルー、`responses` 変換）と 4 つの組み込みプロバイダー（Anthropic、OpenAI、ChatGPT/Codex、xAI） — Anthropic-Messages または OpenAI-Responses のエンドポイントなら設定だけで追加できます（`src/config.rs:180-190,316-363`）。
² CLIProxyAPI: aistudio、antigravity、claude、codex、codex-ws、gemini、gemini-vertex、kimi、openai-compat、xai、xai-ws。
³ raine/ccp は shunt と同様に `ANTHROPIC_MODEL` でモデル単位にルーティングしますが、**Anthropic パススルーアダプターがありません** — 未知の model id は 400 を返すため、指名したモデルだけを迂回させつつメインセッションを Claude に保つことができません。
⁴ raine/ccp は**独自の** ChatGPT OAuth（PKCE ブラウザー + デバイスコードログイン）を実装しています。shunt は Codex CLI のログイン（`~/.codex/auth.json`）を再利用し、独自の PKCE フローは未解決の TODO です（`src/auth/mod.rs:18-19`）。
⁵ **raine/ccp のソースを読んで確認済み**（`fe80a6b`、2026-07-11）: tool-search の処理は存在しません（`defer_loading` / `tool_reference` / `tool_search` / `advanced-tool-use` へのマッチ 0 件）。ツールは `{name, description, parameters}` にホワイトリスト再構築されるため（`src/providers/codex/translate/request.rs:476-494`）、`defer_loading:true` は静かに落とされます — 400 にはなりませんが、コンテキストも節約されません。ToolSearch 結果内の `tool_reference` ブロックは、shunt のきれいな `"Loaded tool: X"` ではなく `[unsupported content block omitted: tool_reference]` としてレンダリングされます（`request.rs:836-842`）。ゆえに ○（shunt の ◐ に対して）: raine/ccp に対して `ENABLE_TOOL_SEARCH` を強制的に有効化すると、discovery ループの結果はプレースホルダーに劣化します。デフォルトでは Claude Code 自身のゲートがファーストパーティ以外の base URL の背後で tool search を無効にしているため、これは潜在的なままです。
⁶ raine/ccp のサブスクリプションバックエンド: Codex（ChatGPT Plus/Pro）、Kimi（kimi.com）、Grok（grok.com）、Cursor Agent — すべてサブスクリプション OAuth 経由。
⁷ shunt は Anthropic `claude_oauth` に対してのみ明示的なアカウントをプールします: セッションスティッキーな選択、プロバイダーごとのラウンドロビン、アカウント別 5h/7d クォータヘッダーによるモデルを認識したプロアクティブなローテーション、クールダウン、401 後の強制リフレッシュ、クォータ拒否の 429 と 5xx レスポンスに対するリアクティブなフェイルオーバー。ChatGPT/Codex は単一アカウントのままで、アカウント別の使用量レポートは未実装です。
⁸ **[#82]** はオプトインのプロバイダー別 `tool_search` フラグ（`src/config.rs:250-261,1041-1049`）を追加し、スキーマをテキストへ折り込む代わりに、Claude Code の tool search を OpenAI Responses API 自身のネイティブでクライアント実行の `tool_search` プロトコルへマッピングします — `ToolSearch` → `tool_search`、その `tool_use` → `tool_search_call`、`tool_reference` → ロードされたツールの完全スキーマを構造化 JSON として運ぶ `tool_search_output` アイテム（`src/model/responses_request.rs`）。デフォルトは無効: ストックの OpenAI または ChatGPT/Codex Responses フレーバーが gpt-5.4+ モデルへルーティングする場合にのみ適用され、特定のバックエンドが shunt の発する形状を受け入れるとライブプローブで確認されるまでフラグの背後にゲートされます。xAI/Grok のルートと gpt-5.2 以下のモデルは、フラグに関係なく #43 のシムを維持します。

> 「raine/ccp」 = [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy)。

## 4. shunt がリードするところ

- **Claude Code ネイティブな忠実さ。** shunt は、古い CC プロキシが使う「サブエージェントのシステムプロンプトをハッシュする」ヒューリスティックの代わりに、*公式の*ゲートウェイ契約を実装します。セッションは Claude Code のハーネス内にとどまり（同じツールループ、スキル、スクリプトパス）、外部化されるのはトークン生成だけです（`README.md:97-131`）。一般的なルーターやゲートウェイの多くは OpenAI chat-completions 中心で、Claude Code の discovery/アトリビューションのサーフェスを尊重しません。

- ***変換*パスでのアップロードトリミング。** shunt は Anthropic ⇄ Responses を変換するため（Claude Code は `previous_response_id` を決して送りません）、continuation を*合成*します: プールされた接続上にトランスクリプトを保存し、次のリクエストを型認識の正規化で差分し、`previous_response_id` + 入力デルタを注入します — Claude→Codex パスでの実質的なアップロードトリミングです（`src/adapters/codex_continuation.rs:79-114`）。これは**固有ではありません**: **raine/claude-code-proxy も同じクラスのことを行います**（オプトインの `CCP_CODEX_PREVIOUS_RESPONSE_ID`、セッションキー、追記のみ）。この 2 つの Rust サブスクリプションプロキシはこれを共有しています — 本当の対比は **CLIProxyAPI** のような**パススルー**プロキシとの対比です。CLIProxyAPI の Codex WS はトランスクリプト/response-id を保存せず、Codex CLI クライアントが `previous_response_id` を送ることに依存するため、*自身の*変換パスでは毎ターン全入力を再送します（加えて、tool-call のペアリング整合性を保つツール出力の「修復」キャッシュもあります）。

- **正規化の深さ + reasoning の忠実さ（最も近いピアに対して）。** continuation を共有するそのペアの中で、shunt は 2 つの軸で raine/claude-code-proxy より先を行きます: (1) continuation の正規化が `function_call.arguments` をパースし、reasoning の `encrypted_content`/signature をラウンドトリップさせるため、形状のみの比較なら途切れるツールターンをまたいで continuation が発火し続けます（`src/adapters/codex_continuation.rs:11-48`）。(2) **Codex の reasoning を `thinking` として Claude Code へ転送**しますが、raine/claude-code-proxy は **Codex の reasoning ブロックを丸ごと破棄します**（自身の README が制限として挙げています）。想定外の形状は今も全入力へフォールバックします — 誤ったコンテキストにはならず、最適化を逃すだけです。

- **小さく監査可能なフットプリント。** 単一の Rust バイナリ、フェイルクローズの起動時検証とホットリロードを備えた TOML+env 設定。保護すべきランタイムのプラグインサーフェスがありません。

## 5. shunt が後れを取るところ — そしてその理由

ギャップの大半は見落としではなく、**意図的なスコープ境界**です。shunt 自身の README が、一般的なゲートウェイ（LiteLLM/Portkey/bifrost）を同じ製品ではなく*隣接インフラ / バックエンド候補*として位置づけています。

- **Anthropic OAuth マルチアカウントは意図的に狭い。** shunt は `auth = "claude_oauth"` に対するプロアクティブかつリアクティブなアカウントプールを持ちます: `x-claude-code-session-id` のスティッキネス、プロバイダーごとのラウンドロビン、5 時間または適用される週次バケットが壁に達する前のモデルを認識したローテーション、アカウントのクールダウン、401 後の認証情報ファイルの強制リフレッシュ、クォータ拒否の 429 または 5xx レスポンス後のフェイルオーバー（[詳細](/ja/guides/anthropic-multi-account/)）。ChatGPT/Codex アカウントのプール、切り替え直後のアカウントの並行度ランプアップ、アカウント別使用量の公開は**行いません**。CLIProxyAPI、LiteLLM、Portkey はより広いフリート指向のバランシングと可視性を提供します。残るギャップは §6 の G–H を参照。
- **狭いバックエンドの幅。** Anthropic-Messages パススルーまたは OpenAI-Responses 変換のみ。この 2 つのプロトコルのどちらかを公開しない限り、ネイティブの Gemini/Bedrock/Azure/Ollama はありません。
- **完全な管理 API / 使用量-クォータ / コストトラッキングなし。** オプトインの[管理 Web サーフェス](/ja/guides/admin-remote-provisioning/)は、`claude_oauth` プロバイダーのブラウザーアカウントプロビジョニングと読み取り専用のアカウントプール健全性をカバーしますが、汎用の管理 API、リクエスト単位の使用量計測、コストトラッキングはありません。可観測性はオプトインの Sentry メトリクスのみです（`src/metrics.rs`）。HTTP サーフェスの全体は [HTTP エンドポイント](/ja/reference/endpoints/)に一覧があります。CLIProxyAPI は完全な管理 API + クォータ/使用量マネージャーとサードパーティのダッシュボードエコシステムを備えます。同一クラスのピアである raine/claude-code-proxy でさえ、shunt に相当物のない組み込み**モニター TUI**（ライブセッション、アクティブ/最近のリクエスト、エラーイベント）を備えています。
- **独自の ChatGPT OAuth ログインなし。** shunt は Codex CLI のログイン（`~/.codex/auth.json`）を再利用します。ファーストパーティの PKCE フローは未解決の TODO です（`src/auth/mod.rs:18-19`）。raine/claude-code-proxy はここでの先行事例です — 独自の `codex auth login`（PKCE）**と** `codex auth device`（デバイスコード）の両方を備え、Codex CLI なしで動作します。
- **プラグイン / インターセプターシステムなし。** アダプターの集合は固定された 2 バリアントの `match` です（`src/proxy.rs:152-163`）。CLIProxyAPI は完全なプラグインホスト（RPC ABI、認証プロバイダー、エグゼキューターのルーティング、リクエスト/レスポンスの変換器）を持ちます。
- **平文 HTTP のみ**（TLS はスコープ外、`docs/m4-inbound-auth.md:13`）。

## 6. 改善機会（この比較から）

shunt のミッションとの適合度順。**スコープ内**の項目は高忠実度の変換 / Claude Code ネイティブな挙動を前進させます。**スコープ境界**の項目は shunt をフリートゲートウェイの方向へ動かすため、先に意識的な判断が必要です。

### スコープ内

- **A. tool-search のコンテキスト節約（追跡中: [#43]）。** shunt は `tool_reference` を名前だけの `"Loaded tool: X"` テキストとしてレンダリングし、*すべての*遅延ツールスキーマを前払いで転送します（`src/model/responses_request.rs:393-403,475-508`） — ループは動きますが、デフォルトではコンテキストをまったく取り戻しません。サーバー側エミュレーション（遅延+未ロードのツールをフィルタリングし、`tool_reference` で完全スキーマを注入）を移植します — 参照実装: CLIProxyAPI PR #1892（`Adamcf123/CLIProxyAPI@main`）。**[#82] で部分的に対処済み**: オプトインのプロバイダー別 `tool_search = true` フラグが、テキストシムの代わりに tool search を Responses API のネイティブでクライアント実行の `tool_search` プロトコルへマッピングするようになりました（ストックの OpenAI または ChatGPT/Codex プロバイダーが gpt-5.4+ モデルへルーティングする場合。上の脚注 8 参照）。バックエンド受け入れのライブプローブまではデフォルト無効のため、オペレーターがオプトインするまではシム（および xAI/Grok と旧モデルのゼロ節約ギャップ）がベースラインのままです。

- **B. Codex WS: continuation 正規化のライブプローブ（追跡中: [#45]）。** Reasoning/`function_call` の正規化は 3 つのソースに対してスキーマ検証済みですが、まだライブプローブされていません（`docs/m7-codex-websocket.md:250-270`）。想定にないフィールドは静かに安全な全入力フォールバックへ落ちます — 正しさの面では安全ですが、*潜在的な最適化の取りこぼし*です。プローブの一巡で、continuation が本来の頻度で発火しているかを確認できます。

- **C. Codex WS: ストリーム途中の失敗からの再開（追跡中: [#46]）。** ストリーミング*前*の WS 失敗は透過的に HTTP へフォールバックしますが、*ストリーム途中*の失敗はフォールバックではなくエラー SSE イベントとして表面化します（`src/adapters/responses.rs:92-135`）。ターン途中で切れたソケットがエラーではなく HTTP へ降格するよう、再開/リプレイを検討します。

- **D. Codex WS: 投機的プリウォーム（`generate:false`）（追跡中: [#47]）。** 今日では明示的にスコープ外ですが（`docs/m7-codex-websocket.md:53-58`）、最初のトークンの前にソケット/コンテキストを温めておくのは実在する Codex のレイテンシ最適化です。continuation がライブプローブされたら再検討の価値があります。

- **E. 上流のリトライ/バックオフ（追跡中: [#48]）。** M4 で計画された有界のリトライ/バックオフは未実装です（`docs/implementation-plan.md:247`）。一時的な上流の 429/5xx エラーはそのまま表面化します。小さく冪等なリトライなら、スコープを広げずにレジリエンスを改善できます。

- **F. ドキュメントのドリフト: `GET /protocol`（追跡中: [#49]）。** README は `GET /protocol` の機械可読な仕様を謳っていますが（`README.md:110`）、`src/server.rs` にそのようなルートはありません。実装する（安価で、ゲートウェイプロトコルのストーリーの一部です）か、ドキュメントを修正します。

### スコープ境界（実行前に判断すること）

- **G. ChatGPT/Codex の最小限のマルチアカウント。** 完全な LB はスコープ外ですが、ヘビーユーザーは ChatGPT/Codex のローリングウィンドウ上限に達します。そこでは、少数の `~/.codex/auth.json` 型ログインをまたぐ *fill-first* ローテーション（1 つのアカウントのウィンドウを使い切ってから次へ移る）が不釣り合いなほど価値を持ちます。これは CLIProxyAPI に対する単一最大の機能ギャップであり、設計の議論に最も値する項目です。

- **H. アカウント別クォータ/使用量の可視性。** G の後続 — 複数のサブスクリプションアカウントが使われるなら、各アカウントの 5h/7d ウィンドウを（CLIProxyAPI のエコシステムのように）可視化することが有用になります。可観測性のギャップと結びつきます。

- **I. ネイティブの Gemini（およびその他）バックエンド。** shunt が Anthropic-Messages / OpenAI-Responses の二元構造を超えて広がる場合にのみ関係します。現在はスコープ外。

## 7. 一行のまとめ

shunt はスペクトラムの**高忠実度・Claude Code ネイティブ**の端です。最も近いピアは **raine/claude-code-proxy** — 同一クラス（Rust、サブスクリプション OAuth、`model` 単位ルーティング、Codex WS + `previous_response_id` continuation） — であり、これに対する shunt の強みは、より深い continuation 正規化、Codex reasoning の忠実さ（raine は破棄）、Anthropic パススルーの経路（メインセッションを Claude に保つ）、xAI OAuth です。raine の強みは組み込みのモニター TUI、ファーストパーティの ChatGPT OAuth ログイン、Kimi/Cursor の幅です。**CLIProxyAPI** に対しては、shunt は変換パスのアップロードトリミングで勝り（CLIProxyAPI の WS はパススルー）、フリート機能の大半（広範なマルチアカウント LB、完全な管理 API、プラグイン、バックエンドの幅）を設計として手放しています。現在は、モデルを認識したプロアクティブなクォータスケジューリングとリアクティブなフェイルオーバーを備えた狭い Anthropic OAuth アカウントプールを提供しますが、ChatGPT/Codex のプールは意図的なギャップのままです。スコープ内で最も価値の高い仕事は、tool-search のコンテキスト節約の完成（[#43]） — Codex/OpenAI 上のオプトインのネイティブ `tool_search` パス（[#82]）で部分的に対処済み — と Codex WS continuation の強化（ライブプローブ + ストリーム途中のフォールバック）です。天秤にかけるべき最大の意図的ギャップは、ChatGPT/Codex の最小限の fill-first マルチアカウントです。

[#43]: https://github.com/pleaseai/shunt/issues/43
[#82]: https://github.com/pleaseai/shunt/issues/82
[#45]: https://github.com/pleaseai/shunt/issues/45
[#46]: https://github.com/pleaseai/shunt/issues/46
[#47]: https://github.com/pleaseai/shunt/issues/47
[#48]: https://github.com/pleaseai/shunt/issues/48
[#49]: https://github.com/pleaseai/shunt/issues/49
