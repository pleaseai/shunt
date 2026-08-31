---
title: Kimi (Moonshot)
description: マッピングしたモデルを、MOONSHOT_API_KEY を使って Moonshot の Anthropic 互換 Kimi エンドポイントへルーティングするか、OAuth で Kimi Code サブスクリプションを再利用する。
---

**Kimi** は Moonshot AI のモデルファミリーで、**Anthropic 互換**エンドポイントで提供されます —
shunt は Moonshot の API キーを注入し、Claude Code の Messages リクエストを転送します。Anthropic 以外の
上流 id では、遅延ツール関連のフィールドが取り除かれます（OpenRouter の stealth スラッグと同じルール）。
`kimi` プリセットは組み込みなので、設定は upstream エントリ 1 つとルートだけです。

このページは、認証情報が別々の 2 つの Kimi サービスを扱います。従量課金の Moonshot API（`kimi` プリセット、
API キー、以下）と、**Kimi Code** サブスクリプション（`kimi-code` プリセット、OAuth ログイン。ページ末尾の
[Kimi Code（OAuth サブスクリプション）](#kimi-codeoauth-サブスクリプション) を参照）です。両者は別のエンドポイントで
あり、互換ではありません。

## クイックスタート

コーディングエージェントにセットアップを任せることもできます — `shunt add` は組み込みのセットアップ
ブループリントを出力します（オフラインかつ読み取り専用で、設定を編集するのはエージェントです。この
コマンド自体は編集しません）。

```bash
shunt add upstream kimi --print | claude
```

または、以下の手順に沿って手動で設定してください。

## upstream を設定する

`kimi` プリセットは `kind = "anthropic"`、`base_url = "https://api.moonshot.ai/anthropic"`、および
`MOONSHOT_API_KEY` からの API キー認証を提供します。

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # ルーティングされないモデル（例 claude-*）のデフォルトとして Anthropic を残す

[[upstreams]]
name = "kimi"
provider = "kimi"

[[routes]]
model = "kimi-k3"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

順序付きの `[[upstreams]]` は shunt の組み込みプロバイダーを置き換えるため、`kimi` へルーティングする設定は、
依然として指し示している `anthropic` デフォルトも宣言する必要があります（`server.default_provider` の
デフォルトは `anthropic`）。`anthropic` エントリを外してよいのは、`default_provider` を宣言済みの upstream に
設定する場合だけです。

従来の `[providers.kimi]` テーブル形式も引き続きサポートされます（古い例では `api_key_env = "KIMI_API_KEY"` を
使っており、明示的に設定すれば今でも機能します） — ただし `[[upstreams]]` と `[providers.*]` を 1 つの
ファイルで混在させないでください。

## 認証情報

```bash
export MOONSHOT_API_KEY='...'
```

キーを設定ファイルに書き込まないでください。`shunt check` は設定の構造を検証しますが、キーの値は読み取り
ません — `MOONSHOT_API_KEY` が未設定の場合、`kimi` へルーティングされた最初のリクエストが認証エラーを
返します。

## モデル

| モデル ID | 備考 |
| :-- | :-- |
| `kimi-k3` | フロンティアティア。クライアントが Claude Code の `[1m]` コンテキストマーカーを付ける場合があります（`kimi-k3[1m]`） — shunt はマッチングの前にこれを取り除くため、サフィックスなしの id をルーティングしてください |
| `kimi-k2.7-code` | コーディング特化ティア |

ルーティング済みの id は、Claude Code で `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION`、または
サブエージェントの `model:` フロントマターから選択します。代わりに `/model` ピッカーへエントリを出したい
場合は、`[models.upstream_model]` マップとともに `claude` プレフィックス付きのエイリアスを広告してください —
[Model Discovery](/ja/guides/model-discovery/) を参照。

## 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"kimi-k2.7-code","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `kimi` を示すことを確認したら、
[Claude Code を shunt へ向け](/ja/guides/connect-claude-code/)てください。

## サブエージェントプラグイン

[`shunt-kimi` プラグイン](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-kimi)は、上記の各モデルに
つき 1 つずつ、既製の Claude Code サブエージェントを出荷しています。

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-kimi@shunt
```

## Kimi Code（OAuth サブスクリプション）

**Kimi Code** は、上記の従量課金 Moonshot API とは別の、サブスクリプション課金のサービスです —
ホストが異なり（`api.moonshot.ai` ではなく `api.kimi.com`）、認証情報も異なります（`MOONSHOT_API_KEY` では
なく、shunt が管理する OAuth トークン）。こちらも Anthropic Messages のワイヤー形状を話すため、同じ
アダプターを使い、プリセットだけが違います: `kimi-code`。

### クイックスタート

```bash
shunt add upstream kimi-code --print | claude
```

または、以下の手順に沿って手動で設定してください。

### 1. ログイン

```bash
shunt login kimi --name <account-name>
```

`--name` は必須です。このログインでは `--mode`、`--long-lived`、`--manual` は受け付けられません —
認証情報は常にリフレッシュ可能であり、手貼りのフォールバックはありません。shunt は
[RFC 8628](https://www.rfc-editor.org/rfc/rfc8628) のデバイス認可グラントを実行します。URL と短いコードを
表示し、あなたがブラウザ（この端末でも別の端末でも可）で承認すると、shunt は承認が完了するかコードが期限
切れになるまでポーリングします。保存されたアカウントは `~/.shunt/accounts/kimi/<account-name>.json` に
置かれ（0600、0700 のディレクトリ内）、`SHUNT_KIMI_ACCOUNTS_DIR` で変更できます。

Kimi はリフレッシュのたびにリフレッシュトークンを回転させ、アクセストークンの寿命は約 15 分しかないため、
リフレッシュは頻繁に発生します。Kimi アカウントファイル 1 つにつき shunt プロセスは 1 つだけ実行して
ください — 1 つのファイルを共有する 2 つのプロセスは、最初のリフレッシュで互いを無効化します。代わりに
プロセスごとに別のアカウントを用意してください。

### 2. upstream を設定する

`kimi-code` プリセットは `kind = "anthropic"`、`base_url = "https://api.kimi.com/coding"`、および
`auth = "kimi_oauth"` を提供します。

```toml
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# [[upstreams]] を宣言すると組み込みのプロバイダー集合が置き換わるため、末尾に
# anthropic のパススルーを残しておくこと — これがないと、デフォルトの
# server.default_provider を `shunt check` が拒否する。これは `shunt init` が
# 追加するのと同じエントリ。
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

`kimi_oauth` は `claude_oauth` や `chatgpt_oauth` とまったく同様にプール対応です。`account` の代わりに
`accounts = [...]` を使うと、名前付きの複数アカウントを 1 つの upstream にプールでき（この 2 つは排他です）、
両方を省略すると shunt 管理下の Kimi アカウントストア全体をスキャンします。

### モデル

shunt は Kimi Code 自身のモデル一覧エンドポイントに問い合わせません — `/v1/models` は shunt の組み込み
カタログから提供します。サブスクリプションが実際に entitle されているモデル ID をルーティングしてください。

```toml
[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

### 検証

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"<model-id-your-subscription-exposes>","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

レスポンスの `x-gateway-upstream` ヘッダーが `kimi-code` を示すことを確認してください。

`"We're unable to verify your membership benefits at this time"` を伴う `402 Payment Required` は、ログインは
成功したものの、そのアカウントに有効な Kimi Code メンバーシップがないことを意味します。認証情報に問題は
なく、対処が必要なのはサブスクリプションのほうです。

### プールされたアカウントと管理画面

`kimi_oauth` のプールは、Claude や Codex のプールと同じ負荷分散・フェイルオーバー・クォータを考慮した
アカウント回転に参加し、有効にしていれば `GET /admin/pool` と、サニタイズされた `GET /usage` の集計に
そのアカウントが現れます。他のプールにはない条件がもう 1 つあり、それによって回転します。上記の `402`
メンバーシップ応答です。メンバーシップが無効だと毎リクエスト 402 が返るため、shunt はこれをアカウント
レベルの障害として扱います — 健全なアカウントが遊んでいるのにクライアントへ 402 を渡すのではなく、その
アカウントをクールダウンさせて次のアカウントを試します。プール内の*すべて*のアカウントが無効な場合は、
Kimi 自身の 402 ステータスとメッセージがそのまま返るため、原因は見えたままになります。
[管理 Web 画面](https://shunt.dev/guides/admin-remote-provisioning/)でのブラウザ経由のアカウント
プロビジョニングは Kimi アカウントに対応していません — その画面のプールビューは Kimi については読み取り
専用です。Kimi アカウントは CLI で `shunt login kimi` を使ってプロビジョニングしてください。
