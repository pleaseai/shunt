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

モデル id をひとつ、自分で判断させることもできます。[ステージルーター](/ja/guides/stage-router/)は強力なティアと効率的なティアを指定し、会話の直近の tool-result メタデータ（`tool_use.name` と `tool_result.is_error` であり、プロンプトのテキストでは決してありません）からターンごとにどちらかを選びます。どのエントリも [`[models.subagents]`](/ja/reference/configuration/#modelssubagentsオプション) オーバーレイを併せて持て、委譲された作業だけを別のターゲットへ送り、親セッションは自分の宛先を保ちます。どちらも設定しなければ挙動は変わりません。

## NVIDIA Switchyard を土台にしたルーティング

ステージルーターとジャッジを使うルーターは、shunt が独自に考え出したヒューリスティックではありません。NVIDIA の [Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) プロジェクトのルーティングライブラリ [`switchyard-libsy`](https://github.com/NVIDIA-NeMo/Switchyard/tree/3ddea9d30174ad835cf505a93617ba251c8eb8dd/crates/libsy) を土台にしています。Switchyard は「各 LLM 呼び出しを、その仕事をこなせるもっとも安いモデルへ」ルーティングするプロジェクトです。そのおかげで、次の利点があります。

- **公開された結果を持つスコアラー。** shunt は Switchyard のステージスコアラーを改変せずに使います。上流の Terminal-Bench 2.1 の実行では、ステージルーティングは Opus 4.8 ベースラインの正解率の 95.7% を保ちながらコストを 30.5% 下げました。比較対象の単一固定モデル 4 つは、いずれも 56% 未満でした。これは shunt ではなく上流が出した数値です。shunt はこのベンチマークを実行しておらず、シグナルも独自の方法で抽出しています。設定するモデルの組に対する保証ではなく、このアプローチが割に合うことの根拠として読んでください。
- **複数のアルゴリズム。** shunt はステージ採点に加えて Switchyard のほかのアルゴリズムも備えており、いずれもモデルエントリ単位でオプトインします。エントリごとにルータータイプを 1 つ選びます。`stage_router`（ハンドオフノートを追加可能）、`llm_classifier`（`escalation` モードがエスカレーションルーティング）、`composite`、`advisor` などがあります。サブエージェントルーティングはこれとは別に `[models.subagents]` テーブルで設定します。
- **Claude Code のセッションに合わせた調整。** Switchyard はプロバイダー中立です。shunt は Claude Code ゲートウェイに必要なものを加えます。ツール名は Claude Code が実際に使う名前をそのまま照合します。セッションのティアは非対称ヒステリシスでピン留めし、ターンごとのティア切り替えで長いセッションが蓄積したプロンプトキャッシュを失わないようにします。委譲されたサブエージェントのルーティングは親と切り離します。
- **モデル id のままのターゲット。** ルーティングされたターゲットは shunt の通常のルーティング経路に戻るため、それぞれのフェイルオーバーチェーン、アカウントプール、アダプターをそのまま使えます。クライアントに見えるモデル id も、要求したとおりです。
- **小さな負担。** この依存がリリースバイナリに加えたサイズは約 0.3% です。レビュー済みの上流リビジョンに固定しているので、アップグレードもレビューを経た diff として入ります。`[models.router]` テーブルも `[models.subagents]` テーブルもないエントリは、これまでどおりにルーティングされます。
- **別サーバー不要のルーティング。** Switchyard の [Server Path](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/getting_started.md#server-path) では `switchyard-server` を独立したプロキシとして別途起動します。Claude Code のように Anthropic Messages でリクエストするクライアントなら、同じルートタイプ（`auto`、`stage_router`、`llm_classifier`）を shunt の `[models.router]` でそのまま使えるため、プロキシをもう 1 つ置く必要はありません。ただし shunt がルーターを適用するのは Anthropic Messages のリクエスト（`/v1/messages` とその `/v1/messages/count_tokens` プローブ）だけです。OpenAI Chat Completions や Responses のクライアントのルーティングは shunt の対象外です。

shunt が Switchyard から取り込んだものと置いてきたもの、ベンチマークの注意点、アルゴリズムの段階ごとの動作は [Switchyard 統合](/ja/guides/switchyard/)で扱います。

## shunt が実装するもの

- **`POST /v1/messages`** — 推論。リクエストの `model` id に従ってルーティングされます。マッピングされていないモデルは、呼び出し元自身の認証情報を使ってバイト単位でそのまま Anthropic へ転送されます。ただし shunt が他のプロバイダー向けに生成した [`thinking` の signature](/ja/providers/anthropic/) だけは、Anthropic が拒否する値のため取り除かれます。
- **Anthropic Messages ⇄ OpenAI Responses 変換** — マッピングされた OpenAI ファミリーのモデル向け。ストリーミングを含みます。
- **ChatGPT サブスクリプションの再利用** — `codex` プロバイダーは Codex CLI の `~/.codex/auth.json` ログインを再利用（かつ自動リフレッシュ）します。
- **`GET /v1/models`** — Claude 命名のエイリアス向けの [model discovery](/ja/guides/model-discovery/)。
- **トークンカウント** — 変換されるプロバイダーにはローカルの tiktoken カウント、パススルーには上流の正確なカウント。
- **ストリーミングの堅牢性** — [SSE キープアライブ ping](/ja/guides/shared-gateway/#sse-キープアライブ-ping) により、Cloudflare のようなプロキシが長い推論の合間を切断しないようにします。
- **オプションのインバウンド認証** — 共有デプロイ向けの[クライアント単位トークン](/ja/guides/shared-gateway/)。

試す準備はできましたか？ [Installation](/ja/getting-started/installation/) へ進んでください。
