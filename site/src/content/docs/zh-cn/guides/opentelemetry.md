---
title: OpenTelemetry
description: 可选启用的 OTLP 导出,将 trace、指标与日志发送到你自己的 collector/后端。
---

shunt 可以通过 OTLP/HTTP 将 **trace、指标和日志** 导出到你自己的 OpenTelemetry Collector(或任何 OTLP 兼容后端)。它是**可选启用、默认关闭**的 —— 没有 `[otel]` 段时,任何数据都不会离开本机 —— 并且与 Sentry 相互独立,你可以只启用其一或两者都启用。

## 启用

一个键即可启用 —— 指向你的 collector 的 OTLP/HTTP 接收端:

```toml
[otel]
endpoint = "http://localhost:4318"   # OTLP/HTTP 基础 URL;shunt 会追加 /v1/{traces,metrics,logs}
```

其余项都有合理的默认值:

```toml
[otel]
endpoint = "http://localhost:4318"
service_name = "shunt"     # (默认) service.name 资源属性
environment = "prod"       # 可选:deployment.environment.name
sample_ratio = 1.0         # (默认) 基于 head 的 trace 采样,0.0–1.0
traces = true              # (默认) 导出请求 span
metrics = true             # (默认) 导出用量指标
logs = true                # (默认) 导出日志事件(stderr 日志不受影响)
include_session_id = false # (默认) 将客户端 session id 排除在 span 之外

[otel.headers]             # 可选:每次请求附带的 header,例如托管 collector 的令牌
authorization = "Bearer <token>"
```

设置 `endpoint = ""`(例如 `SHUNT_OTEL__ENDPOINT=""`)可在不删除该段的情况下再次关闭导出。无效的 endpoint、非 `http(s)` 的 URL、或超出范围的 `sample_ratio` 都是**启动错误**,因此一个拼写错误不会悄无声息地丢弃所有导出。

## 三种信号

| 信号 | 导出内容 | 说明 |
| :-- | :-- | :-- |
| **Trace** | 每次请求的 `proxy_request` span | 通过 `sample_ratio` 进行 head 采样。低基数;不含请求/响应正文。 |
| **指标** | `shunt.requests`(计数)和 `shunt.latency`(ms) | 带 `provider`、`model`、`http.response.status_code` 标签 —— 与 shunt 发往 Sentry 的是同一组序列。 |
| **日志** | shunt 的 `tracing` 日志事件,桥接到 OTLP | stderr 日志不受影响。 |

每种信号都可通过 `traces` / `metrics` / `logs` 单独开关。

## 隐私

shunt 从不导出请求/响应正文、header 或凭据。

- **指标和 trace** 保持低基数且不含正文。请求 span 的客户端 **session id** 仅在 `include_session_id = true`(默认关闭)时附加,且仅在 trace 导出处于启用状态时。
- **日志** 会如实反映 shunt 自身的诊断事件,因此与 stderr 日志一样,可能包含源自请求的字段(上游错误正文、已认证的客户端 id)。若需要严格不含正文的导出,请将 `logs = false`,仅保留指标/trace。

导出的 resource 仅公布 `service.*` 和 `telemetry.sdk.*` —— 不运行 host 或 process detector,因此不会附带本机主机名 —— 再加上你通过标准 `OTEL_RESOURCE_ATTRIBUTES` 设置的内容。

:::caution
若 `[otel.headers]` 携带机密值(例如 collector 的 bearer 令牌),而 endpoint 是指向非回环主机的明文 `http://`,shunt 会在启动时记录一条警告:令牌将以明文传输。远程 collector 请使用 `https://`。
:::

## 标准 `OTEL_` 环境变量

- `endpoint` 和 `service_name` 来自本配置,并**优先于** `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_SERVICE_NAME`。
- 标准的 `OTEL_EXPORTER_OTLP_HEADERS` 和 `OTEL_RESOURCE_ATTRIBUTES` 仍会在 `[otel.headers]` 与内置资源属性之上**合并**进来。

:::note
导出器在启动时初始化一次。编辑 `[otel]` 后热重载会给出警告,且**需要重启**才能生效 —— 这与大多数可实时重载的配置不同。
:::

每个键的详情见 [`[otel]` 配置参考](/zh-cn/reference/configuration/)。
