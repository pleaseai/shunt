---
title: 구성 레퍼런스
description: 모든 shunt.toml 키 — server, providers, routes, models.
---

파일 위치, 우선순위, 주석이 달린 예시는 [구성](/ko/guides/configuration/)을 참고하세요. 전체 템플릿: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

## `[server]`

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt가 리슨하는 주소 |
| `default_provider` | `anthropic` | 일치하는 라우트가 없는 모든 모델의 프로바이더 |
| `sse_keepalive_seconds` | `30` | SSE `ping`이 주입되기 전의 유휴 초; `0`은 비활성화([상세](/ko/guides/shared-gateway/#sse-keepalive-pings)) |

## `[server.auth]` (선택)

이 테이블의 존재가 인바운드 클라이언트 토큰 인증을 활성화합니다([상세](/ko/guides/shared-gateway/)):

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | 클라이언트 토큰을 담는 헤더 |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | 쉼표로 구분된 `name:token` 쌍을 담는 env 변수 |

## `[providers.<name>]`

각 프로바이더는 원하는 이름의 테이블입니다. 내장(`anthropic`, `openai`, `codex`, `xai`, `grok`, `cursor`)은 부분 오버라이드할 수 있습니다 — 구성 맵은 깊은 병합됩니다.

| 키 | 값 | 의미 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` | 업스트림 프로토콜 / 어댑터. `anthropic` = Messages API(패스스루, 선택적으로 키 재설정); `responses` = Anthropic Messages를 OpenAI Responses API로 변환; `cursor` = 네이티브 Cursor ConnectRPC/protobuf AgentService 어댑터. |
| `base_url` | URL | 업스트림 base; shunt가 엔드포인트 경로를 붙입니다. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough`는 클라이언트 본인의 자격 증명을 전달; `api_key`는 `api_key_env`의 키를 주입; `chatgpt_oauth`는 `~/.codex/auth.json`을 재사용; `xai_oauth`는 `shunt login xai`의 `~/.shunt/xai-auth.json`을 재사용(HTTPS를 통한 x.ai/grok.com 호스트에만 전송); `cursor_oauth`는 `~/.shunt/cursor-auth.json`을 재사용(`shunt login cursor`). |
| `api_key_env` | env 변수 이름 | `auth = "api_key"`일 때 키를 읽어오는 곳. |
| `api_key_header` | `bearer`(기본) \| `x_api_key` | 주입된 키가 전송되는 헤더. |
| `effort` | `low` … `max` | 선택적 기본 추론 노력(`responses` 프로바이더). |
| `count_tokens` | `tiktoken`(기본) \| `estimate` | `responses` 프로바이더 전용: 로컬 tiktoken 카운트 대 404 폴백([상세](/ko/guides/effort-and-context/#token-counting-count_tokens)). |

## `[[routes]]`

정확히 일치하는 라우팅 항목 — 먼저 확인됩니다:

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `model` | ✅ | Claude Code가 보내는 정확한 `model` id |
| `provider` | ✅ | `[providers.<name>]` 테이블의 이름 |
| `upstream_model` | — | 업스트림으로 전달되는 모델 id를 다시 씀 |
| `effort` | — | 라우트별 추론 노력 오버라이드 |

## `[[route_prefixes]]`

프리픽스로 일치하는 라우팅 항목 — 정확한 라우트 이후에 확인됩니다:

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `prefix` | ✅ | 모델 id 프리픽스, 예: `gpt-` |
| `provider` | ✅ | `[providers.<name>]` 테이블의 이름 |

## `[[models]]`

[모델 디스커버리](/ko/guides/model-discovery/)를 위해 `GET /v1/models`가 반환하는 항목. id는 반드시 `claude` 또는 `anthropic`으로 시작해야 하며, 그렇지 않으면 Claude Code가 무시합니다.

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `id` | ✅ | Claude Code에 노출되는 모델 id |
| `display_name` | — | `/model` 선택기에 표시되는 레이블 |

## `[otel]` (선택)

트레이스·메트릭·로그를 자체 컬렉터로 내보내는 옵트인 OpenTelemetry(OTLP/HTTP) 익스포트([상세](/ko/guides/opentelemetry/)). `endpoint`를 설정하지 않으면 꺼짐이며, Sentry와 독립적입니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `endpoint` | — | OTLP/HTTP base URL(예: `http://localhost:4318`); shunt가 `/v1/{traces,metrics,logs}`를 덧붙임. 비우면 비활성화, `http(s)`가 아닌 URL은 시작 오류. |
| `service_name` | `shunt` | `service.name` 리소스 속성(`OTEL_SERVICE_NAME`보다 우선) |
| `environment` | — | 선택: `deployment.environment.name` |
| `sample_ratio` | `1.0` | `[0.0, 1.0]` 범위의 head-based 트레이스 샘플링; 범위 밖이면 시작 오류 |
| `traces` | `true` | 요청별 `proxy_request` 스팬 내보내기 |
| `metrics` | `true` | `shunt.requests` / `shunt.latency` 계열 내보내기 |
| `logs` | `true` | `tracing` 로그 이벤트 내보내기(stderr 로그는 영향 없음) |
| `include_session_id` | `false` | 요청 스팬에 클라이언트 세션 id 첨부 |

## `[otel.headers]` (선택)

모든 OTLP 요청에 붙는 추가 헤더(예: 호스팅 컬렉터 토큰). 표준 `OTEL_EXPORTER_OTLP_HEADERS` 아래로 병합됩니다.

| 키 | 의미 |
| :-- | :-- |
| 임의 | 헤더 이름 → 값, 예: `authorization = "Bearer <token>"` |

## 라우팅 우선순위

정확한 `[[routes]]` 일치 → `[[route_prefixes]]` 프리픽스 일치 → `server.default_provider`.
