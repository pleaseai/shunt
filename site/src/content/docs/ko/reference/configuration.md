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

지정된 환경 변수에는 하나 이상의 자격 증명이 있어야 합니다. 예: `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`. 테이블이 있는데 변수가 설정되지 않았거나, 비어 있거나, 형식이 잘못되면 시작은 닫힌 채로 실패(fail closed)합니다. 게이팅되는 라우트(매핑된 `/v1/messages` 추론과 `GET /v1/models` 디스커버리)는 구성된 헤더, `Authorization: Bearer`, `x-api-key`로 토큰을 받습니다 — 여러 슬롯에 유효한 토큰이 있으면 전용 헤더가 우선합니다.

## `[server.admin]` (선택)

이 테이블의 존재가 브라우저 계정 프로비저닝과 계정 풀 상태를 위한 관리자 웹 화면을 활성화합니다([상세](/ko/guides/admin-remote-provisioning/)). 테이블이 없으면 `/admin*` 라우트는 하나도 등록되지 않습니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | API/curl 호출용 관리자 토큰을 담는 헤더 |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | 쉼표로 구분된 `name:token` 쌍을 담는 env 변수 |
| `session_ttl_secs` | `3600` | 로그인 후 브라우저 세션 수명(초) |
| `pending_ttl_secs` | `600` | 시작된 프로비저닝 플로우를 끝낼 수 있는 시간(초) |

지정된 환경 변수에는 하나 이상의 자격 증명이 있어야 합니다. 예: `SHUNT_ADMIN_TOKENS="ops:<token>"`. 테이블이 있는데 변수가 설정되지 않았거나, 비어 있거나, 형식이 잘못되면 시작은 닫힌 채로 실패(fail closed)합니다.

관리자 토큰은 `[server.auth]` 아래에 구성되는 클라이언트 토큰과 별개의 자격 증명입니다; 하나의 자격 증명을 두 표면에 재사용하지 마세요.

## `[server.pool]` (선택)

Claude(Anthropic) 계정 풀을 위한 쿼터 인지 로드 밸런싱 튜닝([상세](/ko/guides/anthropic-multi-account/#선택-튜닝-serverpool)). 테이블이 없으면 선택은 이 테이블이 존재하기 이전과 동일하게 단일 내장 `0.98` 임계값을 사용합니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | 모든 쿼터 창에 대한 안전 백스톱; 이 값 이상인 계정은 사용 가능한 계정 중 항상 마지막으로 정렬됨 |
| `default_threshold` | 미설정 | 더 구체적인 값이 없는 모든 창에 적용되는 소프트 기본 임계값 |
| `default_threshold_5h` | 미설정 | 5시간 창의 소프트 기본값 |
| `default_threshold_7d` | 미설정 | 공유 주간(`7d`) 창의 소프트 기본값 |
| `default_threshold_fable` | 미설정 | fable 전용 주간(`7d_oi`) 창의 소프트 기본값 |
| `burn_rate_avoidance` | `false` | 창이 리셋되기 전에 소프트 임계값을 소진할 것으로 예측되는 계정도 함께 회피 |

각 창 `X`에 대해 유효 소프트 임계값은 다음 순서로 결정됩니다: 계정 `threshold_X` → 계정 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, 그리고 `hard_threshold`로 상한이 걸립니다. 모든 임계값은 `[0.0, 1.0]` 범위의 사용률 비율이며, 범위를 벗어나면 시작이 실패합니다. 쿼터 헤더는 Anthropic 백엔드에만 존재하므로 이 노브들은 Codex/ChatGPT 풀에서는 비활성 상태입니다 — 계정별 `priority`와 `disabled` 키는 그런 풀에도 여전히 적용됩니다(키 상세는 [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/) 참고).

## `[providers.<name>]`

각 프로바이더는 원하는 이름의 테이블입니다. 내장(`anthropic`, `openai`, `codex`, `xai`, `grok`, `cursor`)은 부분 오버라이드할 수 있습니다 — 구성 맵은 깊은 병합됩니다.

| 키 | 값 | 의미 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` | 업스트림 프로토콜 / 어댑터. `anthropic` = Messages API(패스스루, 선택적으로 키 재설정); `responses` = Anthropic Messages를 OpenAI Responses API로 변환; `cursor` = 네이티브 Cursor ConnectRPC/protobuf AgentService 어댑터. |
| `base_url` | URL | 업스트림 base; shunt가 엔드포인트 경로를 붙입니다. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` | `passthrough`는 클라이언트 본인의 credential을 전달; `api_key`는 `api_key_env`의 키를 주입; `chatgpt_oauth`는 `~/.codex/auth.json`을 재사용; `claude_oauth`는 명시적 Anthropic 계정에서 선택; `xai_oauth`는 `shunt login xai`의 `~/.shunt/xai-auth.json`을 재사용(HTTPS를 통한 x.ai/grok.com 호스트에만 전송); `cursor_oauth`는 `~/.shunt/cursor-auth.json`을 재사용(`shunt login cursor`). |
| `api_key_env` | env 변수 이름 | `auth = "api_key"`일 때 키를 읽어오는 곳. |
| `api_key_header` | `bearer`(기본) \| `x_api_key` | 주입된 키가 전송되는 헤더. |
| `accounts` | 계정 테이블 배열 | Anthropic OAuth 계정 풀. `kind = "anthropic"`이고 `auth = "claude_oauth"`일 때만 유효; 아래 참고. |
| `effort` | `low` … `max` | 선택적 기본 추론 노력(`responses` 프로바이더). |
| `count_tokens` | `tiktoken`(기본) \| `estimate` | `responses` 및 `cursor` provider: 로컬 tiktoken 카운트 대 `501 not_supported` fallback([상세](/ko/guides/effort-and-context/#token-counting-count_tokens)). |

이름만 있는 항목은 `shunt login claude --name <name> --mode <mode>`(`<mode>`는 `oauth`, `import`, `setup-token` 중 하나)로 만든 `~/.shunt/accounts/claude/<name>.json`을 읽습니다. 대화형 CLI는 이 세 mode를 묻고 갱신 가능한 OAuth를 권장합니다. `--long-lived`는 `--mode setup-token`의 deprecated alias입니다. `SHUNT_CLAUDE_ACCOUNTS_DIR`로 스토어 디렉터리를 재정의할 수 있습니다. `[[providers.<name>.accounts]]`에 명시적으로 나열된 계정 목록이 비어 있으면 스토어 디렉터리의 유효한 계정 파일을 모두 스캔합니다. 갱신 가능한 OAuth/import 파일은 provider가 refresh token을 회전할 때 제자리에서 갱신되므로 파일마다 활성 owner가 하나만 있어야 합니다. 실행 중인 여러 shunt 프로세스에서 파일을 공유하거나 독립적으로 복사하지 마세요. 프로세스마다 별도로 프로비저닝하거나, 적절한 경우 정적 setup token을 사용하세요.

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

## `[sentry]` (선택)

자체 Sentry 프로젝트로의 옵트인 오류 리포팅. `dsn`을 설정하지 않으면 꺼짐이며, `[otel]`과 독립적입니다. 게이트웨이 자체 진단만 보고합니다 — 치명적인 게이트웨이 시작/서빙 오류, 패닉, `error` 레벨 로그 이벤트(`warn`/`info`는 브레드크럼, 메시지만 포함); 요청/응답 본문, 헤더, 자격증명은 절대 전송되지 않습니다. 메트릭과 트레이싱은 각각 별도의 추가 옵트인입니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `dsn` | — | Sentry 프로젝트 DSN. 비우면 비활성화, 잘못된 DSN은 시작 오류. |
| `environment` | — | 보고되는 이벤트에 붙는 선택적 environment 태그 |
| `metrics` | `false` | 사용량 메트릭도 전송 — `shunt.requests` / `shunt.latency` 계열(집계값만) |
| `traces_sample_rate` | `0.0` | 성능 트레이스도 전송: 요청별 스팬이 Sentry 트랜잭션이 되며, `[0.0, 1.0]` 범위의 이 비율로 head 샘플링. `0.0`이면 스팬을 전혀 보내지 않음, 범위 밖은 시작 오류. |
| `include_session_id` | `false` | Sentry로 보내는 요청 스팬에 클라이언트 세션 id를 첨부 |

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
