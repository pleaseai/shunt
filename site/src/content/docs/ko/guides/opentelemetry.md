---
title: OpenTelemetry
description: 트레이스, 메트릭, 로그를 자체 컬렉터/백엔드로 내보내는 옵트인 OTLP 익스포트.
---

shunt는 **트레이스, 메트릭, 로그**를 OTLP/HTTP로 자체 OpenTelemetry Collector(또는 OTLP 호환 백엔드)에 내보낼 수 있습니다. **옵트인이며 기본적으로 꺼져 있습니다** — `[otel]` 섹션이 없으면 아무것도 머신 밖으로 나가지 않습니다 — 그리고 Sentry와 독립적으로 동작하므로 둘 중 하나 또는 둘 다 켤 수 있습니다.

## 활성화

키 하나로 켜집니다 — 컬렉터의 OTLP/HTTP 리시버를 가리키세요:

```toml
[otel]
endpoint = "http://localhost:4318"   # OTLP/HTTP base URL; shunt가 /v1/{traces,metrics,logs}를 덧붙임
```

나머지는 모두 합리적인 기본값을 가집니다:

```toml
[otel]
endpoint = "http://localhost:4318"
service_name = "shunt"     # (기본값) service.name 리소스 속성
environment = "prod"       # 선택: deployment.environment.name
sample_ratio = 1.0         # (기본값) head-based 트레이스 샘플링, 0.0–1.0
traces = true              # (기본값) 요청 스팬 내보내기
metrics = true             # (기본값) 사용량 메트릭 내보내기
logs = true                # (기본값) 로그 이벤트 내보내기 (stderr 로그는 영향 없음)
include_session_id = false # (기본값) 클라이언트 세션 id를 스팬에서 제외

[otel.headers]             # 선택: 요청마다 붙는 헤더, 예: 호스팅 컬렉터 토큰
authorization = "Bearer <token>"
```

`endpoint = ""`(예: `SHUNT_OTEL__ENDPOINT=""`)로 설정하면 섹션을 지우지 않고도 익스포트를 다시 끕니다. 유효하지 않은 엔드포인트, `http(s)`가 아닌 URL, 범위를 벗어난 `sample_ratio`는 **시작 오류**이므로, 오타 때문에 모든 익스포트가 조용히 사라지지 않습니다.

## 세 가지 시그널

| 시그널 | 내보내는 내용 | 참고 |
| :-- | :-- | :-- |
| **트레이스** | 요청별 `proxy_request` 스팬 | `sample_ratio` 기반 head 샘플링. 저(低)카디널리티이며 요청/응답 본문 없음. |
| **메트릭** | 아래에 나열된 저카디널리티 계열 | `[sentry] metrics = true`일 때 shunt가 Sentry로 보내는 것과 동일한 계열. |
| **로그** | shunt의 `tracing` 로그 이벤트를 OTLP로 브리지 | stderr 로그는 영향받지 않음. |

각 시그널은 `traces` / `metrics` / `logs`로 개별 토글합니다.

### 메트릭 계열

| 계열 | 유형 | 속성 | 의미 |
| :-- | :-- | :-- | :-- |
| `shunt.requests` | 카운터 | `provider`, `model`, `http.response.status_code` | 프록시된 추론 요청. |
| `shunt.latency` | 히스토그램(ms) | `provider`, `model`, `http.response.status_code` | 스트림은 헤더 지연 시간, 그 외에는 전체 지연 시간. |
| `shunt.ttft` | 히스토그램(ms) | `provider`, `model` | 요청 시작부터 첫 SSE 본문 청크까지의 시간. |
| `shunt.stream_outcome` | 카운터 | `provider`, `model`, `outcome` | SSE 최종 결과 하나: `completed`, `error_event`, `upstream_cut`, `client_disconnect`. |
| `shunt.tokens` | 카운터 | `provider`, `model`, `kind` | 마지막으로 보고된 스트리밍 토큰 사용량(`input`, `output`, `cache_read`, `cache_creation`). 비스트리밍 사용량은 기록하지 않음. |
| `shunt.codex_continuation` | 카운터 | `provider`, `outcome` | Codex WebSocket continuation hit 또는 fallback. |
| `shunt.codex_client_events` | 카운터 | `event` | 정제된 이벤트 이름별 Codex CLI 분석 이벤트. payload와 속성은 폐기됨. |
| `shunt.upstream_retries` | 카운터 | `provider`, `reason` | 제한된 일시적 업스트림 재시도. |
| `shunt.pool.quota_utilization` | 게이지 | `provider`, `window` | `5h`, `7d`, `7d_oi`별 활성 상태이고 관측됐으며 만료되지 않은 quota 값 중 최소 사용률. |
| `shunt.pool.rotations` | 카운터 | `provider`, `reason` | 계정에서 이동한 횟수와 pool이 소진된 요청 수. |

## 프라이버시

shunt는 **메트릭과 트레이스**에서 요청/응답 본문, 헤더, 자격 증명을 절대 내보내지 않습니다.

- **메트릭과 트레이스**는 저카디널리티이며 본문이 없습니다. OTLP 트레이스 익스포트에서 요청 스팬의 클라이언트 **세션 id**는 `include_session_id = true`(기본 꺼짐)일 때만, 그리고 트레이스 익스포트가 활성화된 동안에만 컬렉터로 전송됩니다. 같은 규칙이 Sentry 트레이스 익스포트(`[sentry] traces_sample_rate` / `include_session_id`)에도 적용됩니다. 어떤 스팬 익스포트도 활성화되지 않은 경우 id는 예전처럼 로컬 요청 스팬에만 남습니다.
- **로그**는 shunt 자체 진단 이벤트를 있는 그대로 반영하므로, stderr 로그처럼 요청에서 파생된 필드(업스트림 오류 본문, 인증된 클라이언트 id)를 포함할 수 있습니다. 엄격하게 본문 없는 익스포트를 원하면 `logs = false`로 두고 메트릭/트레이스만 유지하세요.

내보내는 리소스는 `service.*`, `telemetry.sdk.*`, 그리고 `environment`가 설정된 경우 `deployment.environment.name`을 광고합니다 — host나 process detector가 실행되지 않아 머신 호스트네임은 붙지 않습니다 — 여기에 표준 `OTEL_RESOURCE_ATTRIBUTES`로 설정한 값이 더해집니다.

:::caution
`[otel.headers]`가 비밀 값(예: 컬렉터 bearer 토큰)을 담고 있고 엔드포인트가 비루프백 호스트에 대한 평문 `http://`이면, shunt는 시작 시 경고를 남깁니다: 토큰이 평문으로 전송됩니다. 원격 컬렉터에는 `https://`를 사용하세요.
:::

## 표준 `OTEL_` 환경 변수

- `endpoint`와 `service_name`은 이 구성에서 오며 `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_SERVICE_NAME`보다 **우선합니다**.
- 표준 `OTEL_EXPORTER_OTLP_HEADERS`와 `OTEL_RESOURCE_ATTRIBUTES`는 `[otel.headers]` 및 내장 리소스 속성 위에 **병합됩니다**.

:::note
익스포터는 시작 시 한 번 초기화됩니다. `[otel]`을 편집하고 핫 리로드하면 경고가 나며 적용에는 **재시작이 필요합니다** — 라이브로 리로드되는 대부분의 구성과 다릅니다.
:::

모든 키는 [`[otel]` 구성 레퍼런스](/ko/reference/configuration/)를 참고하세요.
