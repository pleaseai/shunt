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
| **指标** | 下方列出的低基数序列 | 与 `[sentry] metrics = true` 时 shunt 发往 Sentry 的序列相同。 |
| **日志** | shunt 的 `tracing` 日志事件,桥接到 OTLP | stderr 日志不受影响。 |

每种信号都可通过 `traces` / `metrics` / `logs` 单独开关。

### 指标序列

| 序列 | 类型 | 属性 | 含义 |
| :-- | :-- | :-- | :-- |
| `shunt.requests` | 计数器 | `provider`, `model`, `http.response.status_code` | 代理的推理请求。 |
| `shunt.latency` | 直方图(ms) | `provider`, `model`, `http.response.status_code` | 流式响应为到 header 的延迟；其他响应为完整延迟。 |
| `shunt.ttft` | 直方图(ms) | `provider`, `model` | 从请求开始到第一个 SSE body chunk 的时间。 |
| `shunt.stream_outcome` | 计数器 | `provider`, `model`, `outcome` | 每个 SSE 记录一个最终结果：`completed`、`error_event`、`upstream_cut` 或 `client_disconnect`。 |
| `shunt.tokens` | 计数器 | `provider`, `model`, `kind` | 最后报告的流式 token 用量（`input`、`output`、`cache_read`、`cache_creation`）；不记录非流式用量。 |
| `shunt.codex_continuation` | 计数器 | `provider`, `outcome` | Codex WebSocket continuation 的 hit 或 fallback。 |
| `shunt.codex_client_events` | 计数器 | `event` | 按净化后的事件名称统计 Codex CLI 分析事件；payload 和属性会被丢弃。 |
| `shunt.upstream_retries` | 计数器 | `provider`, `reason` | 有次数限制的临时上游重试。 |
| `shunt.pool.quota_utilization` | 仪表 | `provider`, `window` | `5h`、`7d` 或 `7d_oi` 窗口中已启用、已观测且未过期的 quota 值的最小使用率。 |
| `shunt.pool.rotations` | 计数器 | `provider`, `reason` | 离开账户的切换次数以及 pool 耗尽的请求数。 |

## 隐私

shunt 在**指标和 trace** 中从不导出请求/响应正文、header 或凭据。

- **指标和 trace** 保持低基数且不含正文。在 OTLP trace 导出中,请求 span 的客户端 **session id** 仅在 `include_session_id = true`(默认关闭)时、且仅在 trace 导出处于启用状态时才发送给 collector。同样的规则也适用于 Sentry 的 trace 导出(`[sentry] traces_sample_rate` / `include_session_id`)。当没有任何 span 导出处于启用状态时,该 id 仍会像以前一样只保留在本地请求 span 上。
- **日志** 会如实反映 shunt 自身的诊断事件,因此与 stderr 日志一样,可能包含源自请求的字段(上游错误正文、已认证的客户端 id)。若需要严格不含正文的导出,请将 `logs = false`,仅保留指标/trace。

导出的 resource 公布 `service.*`、`telemetry.sdk.*`,以及在设置了 `environment` 时的 `deployment.environment.name` —— 不运行 host 或 process detector,因此不会附带本机主机名 —— 再加上你通过标准 `OTEL_RESOURCE_ATTRIBUTES` 设置的内容。

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
