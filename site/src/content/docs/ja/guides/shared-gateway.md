---
title: Sharing a Gateway
description: 共有デプロイ向けのクライアント単位トークンと、プロキシやトンネル向けの SSE キープアライブ ping。
---

## インバウンドのクライアントトークン

デフォルトでは shunt にインバウンド認証はありません — ループバックのみの個人ゲートウェイなら問題ありませんが、VPN/トンネル越しに共有すると、そこに到達できる誰もが、マッピングされたモデルで**オペレーターの**アカウントを消費できてしまいます（shunt はそれらに自身の `api_key`/`chatgpt_oauth` 認証情報を注入します）。パススルーモデルは懸念ではありません。各呼び出し元自身の Anthropic 認証情報を転送します。

`[server.auth]` は、注入された認証情報を使うルートだけを、クライアント単位トークンでゲートします。

```toml
[server.auth]                        # both keys optional; defaults shown
header = "x-shunt-token"
tokens_env = "SHUNT_CLIENT_TOKENS"
```

```bash
# Gateway side: name:token pairs (names are labels for logging; tokens are secrets)
export SHUNT_CLIENT_TOKENS="minsu:$(openssl rand -hex 32),alice:$(openssl rand -hex 32)"
```

起動は、`[server.auth]` が存在するのに環境変数が未設定または不正な場合、**フェイルクローズ**します。有効なトークンなしでマッピングされたモデルへのリクエストは 401 `authentication_error` を受け取ります。`GET /v1/models`、`GET /routes`、`GET|HEAD /`、`GET /health`、およびパススルーモデルは開いたままです。`GET /routes` は `GET /v1/models` と同じ discovery エンドポイント設計により未認証です — これはルーティングのメタデータ（設定されたプロバイダー/上流モデルのマッピング）を公開しますが、認証情報は決して公開しません。認証情報はプロバイダー設定にのみ存在し、そのハンドラーによって読まれることはありません。

トークンヘッダーは転送前に常に除去され、マッチングは定数時間で、トークン値はログに記録されません（クライアント*名*はリクエストごとに記録されます）。

クライアント側は 1 行です（`ANTHROPIC_CUSTOM_HEADERS` は 1 行ごとに 1 つの `Name: Value` を取ります）。

```bash
export ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <your token>"
```

:::note
これはアプリケーションレイヤーの識別にすぎません — トランスポート暗号化はデプロイ側から来ます（WireGuard/Tailscale トンネル、または前段での TLS 終端）。shunt 自身は平文 HTTP を提供します。
:::

## SSE キープアライブ ping

中間装置は静かなストリームを切断します — Cloudflare のプロキシは **1 バイトも来ないまま 100 秒で 524** を返し（Enterprise 未満では固定）、長い reasoning の合間はそれだけ静かになりえます。そのため shunt は、ストリーミングレスポンスがアイドルになるたびに、Anthropic プロトコル自身の `ping` イベント（`api.anthropic.com` 自身が発行し、すべてのクライアントが無視するもの）を注入します。

```toml
[server]
sse_keepalive_seconds = 30   # default; 0 disables
```

Ping は完全な SSE イベントの間にのみ（半分送信されたフレームの内部には決して）、`text/event-stream` レスポンスにのみ注入され、上流ストリームと共に停止します。アイドルタイムアウトのないトンネル（WireGuard/Tailscale）の背後では ping は無害です。バイト単位で同一の中継が欲しい場合は `0` で無効化してください。
