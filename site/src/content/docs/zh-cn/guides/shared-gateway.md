---
title: 共享网关
description: 面向共享部署的按客户端 token,以及面向代理和隧道的 SSE keepalive ping。
---

## 入站客户端 token

默认情况下 shunt 没有入站认证 —— 对于一个只在回环的个人网关这没问题,但一旦你通过 VPN/隧道共享它,任何能触达它的人都能在映射模型上花费**运营者的**账户(shunt 为这些模型注入它自己的 `api_key`/`chatgpt_oauth` 凭据)。透传模型不在此列:它们转发每个调用方自己的 Anthropic 凭据。

`[server.auth]` 恰好用按客户端 token 门控那些注入凭据的路由:

```toml
[server.auth]                        # 两个键都可选;显示的是默认值
header = "x-shunt-token"
tokens_env = "SHUNT_CLIENT_TOKENS"
```

```bash
# 网关侧:name:token 对(name 是用于日志的标签;token 是密钥)
export SHUNT_CLIENT_TOKENS="minsu:$(openssl rand -hex 32),alice:$(openssl rand -hex 32)"
```

如果存在 `[server.auth]` 但环境变量未设置或格式错误,启动会**安全失败(fail closed)**。对映射模型的、不带有效 token 的请求会得到 401 `authentication_error`;`GET /v1/models`、`GET /routes`、`GET|HEAD /`、`GET /health` 以及透传模型保持开放。`GET /routes` 与 `GET /v1/models` 出于同样的发现端点设计而无需认证 —— 它暴露路由元数据(配置的提供方/上游模型映射),从不暴露凭据,凭据只存在于提供方配置中,且从不被该处理器读取。

token 头部在转发前总会被剥除,匹配是常量时间的,token 值从不被记录(客户端*名称*会,按请求)。

客户端侧,一行(`ANTHROPIC_CUSTOM_HEADERS` 每行接受一个 `Name: Value`):

```bash
export ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <your token>"
```

:::note
这仅是应用层识别 —— 传输加密仍来自部署(WireGuard/Tailscale 隧道,或前置的 TLS 终止);shunt 本身提供纯 HTTP。
:::

## SSE keepalive ping

中间盒会中断安静的流 —— Cloudflare 的代理在**100 秒无字节后返回 524**(Enterprise 以下固定如此),而长时间的推理过程可能安静那么久。因此,每当一个流式响应闲置时,shunt 会注入 Anthropic 协议自身的 `ping` 事件(`api.anthropic.com` 自己也会发出它,而每个客户端都会忽略它):

```toml
[server]
sse_keepalive_seconds = 30   # 默认;0 禁用
```

ping 只在完整的 SSE 事件之间注入(绝不在半发出的帧内部),只在 `text/event-stream` 响应上注入,并随上游流停止。在一个没有闲置超时的隧道后(WireGuard/Tailscale),这些 ping 是无害的;如果你想要逐字节相同的中继,用 `0` 禁用。
