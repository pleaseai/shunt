---
title: HTTP 엔드포인트
description: shunt가 Claude Code LLM 게이트웨이로서 제공하는 엔드포인트.
---

| 메서드 | 경로 | 용도 |
| :-- | :-- | :-- |
| `HEAD` | `/` | Liveness 프로브 |
| `GET` | `/` | 사람이 읽을 수 있는 랜딩(버전 + 엔드포인트 목록) |
| `GET` | `/health` | 헬스체크 — `{"status":"ok","version":"x.y.z"}` |
| `GET` | `/v1/models` | [모델 디스커버리](/ko/guides/model-discovery/) — `[[models]]` 항목을 반환 |
| `GET` | `/routes` | shunt 네이티브 라우트 디스커버리 — 구성된 `[[routes]]` 테이블을 그대로 반환(model → provider/upstream_model/effort 매핑, claude 프리픽스 디스커버리 별칭 포함); 더 좁은 Anthropic 프로토콜 디스커버리 응답(`id`, `display_name`, 업스트림 모델 메타데이터)을 제공하는 `/v1/models`와 구별됨 |
| `POST` | `/v1/messages` | 추론 — 요청의 `model` id에 따라 라우팅 |
| `POST` | `/v1/messages/count_tokens` | [토큰 카운팅](/ko/guides/effort-and-context/#토큰-카운팅-count_tokens) |
| `GET` | `/managed/settings` | 게이트웨이 JWT별 Claude Code managed settings; `ETag`, `If-None-Match`, `304 Not Modified` 지원 |
| `GET` | `/v1/organizations/spend_limits` | 저장된 지출 제한을 방향성 커서 페이지네이션으로 조회 |
| `POST` | `/v1/organizations/spend_limits` | 하나의 `(scope, period)`에 대한 지출 제한을 생성하거나 교체 |
| `GET` | `/v1/organizations/spend_limits/{id}` | 저장된 지출 제한 하나를 조회 |
| `DELETE` | `/v1/organizations/spend_limits/{id}` | 저장된 지출 제한 하나를 삭제 |
| `POST` | `/v1/metrics` | 관리형 Claude Code 클라이언트의 인바운드 OTLP/HTTP 메트릭 — opt-in한 게이트웨이 텔레메트리 목적지로 verbatim relay |
| `POST` | `/v1/logs` | 인바운드 OTLP/HTTP log record — `logs = true`인 목적지에만 relay |
| `POST` | `/v1/traces` | 인바운드 OTLP/HTTP span — `traces = true`인 목적지에만 relay |
| `GET` | `/admin` | 관리자 대시보드(HTML); 로그인하지 않았으면 `/admin/login`으로 리다이렉트 |
| `GET`, `POST` | `/admin/login` | 관리자 토큰 로그인 폼과 브라우저 세션 생성 |
| `POST` | `/admin/api/logout` | 브라우저 세션 삭제 |
| `GET` | `/admin/api/accounts` | Claude 계정 스토어 메타데이터: 이름, 종류, 만료, UUID; 토큰 자체는 절대 반환하지 않음 |
| `GET` | `/admin/api/accounts/codex` | Codex 계정 스토어 메타데이터: 이름, 만료, ChatGPT 계정 ID; 토큰 자체는 절대 반환하지 않음 |
| `GET` | `/admin/api/pool` | `claude_oauth`, `chatgpt_oauth`, `kimi_oauth` 프로바이더별 풀 상태; 각 account 객체에는 선택적인 `plan` 문자열이 포함될 수 있고 파일에서 읽은 값은 이후 profile 조회로 더 정밀하게 보정될 수 있으며, Codex 행은 보고된 5시간/7일 사용량을 담으며 `7d_oi`에는 Codex 대응 항목이 없음; 각 account에는 불리언 `needs_relogin`도 실린다: 크리덴셜이 종결적으로 거부되었거나(`invalid_grant`), 리프레시 토큰이 아예 없거나, 회전된 토큰 쌍을 저장하지 못해 잃어버린 경우로, 어떤 재시도로도 되살릴 수 없고 운영자 재로그인만이 해결한다. 쿨다운 필드와 **독립적으로** 보고된다 — 쿨다운은 저절로 만료되지만 이 표식은 남는다 — 그리고 대시보드의 두 표 모두 쿼터 일시정지의 `cooling`이 아니라 **needs re-login**으로 표시한다. 인메모리라 재시작하면 초기화되고, 해당 계정의 다음 종결 실패에서 다시 세워진다. 어떤 provider 테이블도 선택한 적 없는 계정에도 — `has_state: false`와 함께 — 보고된다. admin refresh 프로브가 판정을 스토어 이름으로 기록하기 때문이다. |
| `POST` | `/admin/api/accounts/claude` | `{name, mode}`로 Claude 브라우저 프로비저닝 시작. `mode`는 `oauth` 또는 `setup_token`이며, 생략하면 `setup_token`; `{authorize_url}` 반환 |
| `POST` | `/admin/api/accounts/claude/{name}/complete` | `<code>#<state>`가 담긴 `{code}`로 Claude 프로비저닝 완료; 계정을 저장하고 실제 사용 여부(live)를 보고 |
| `POST` | `/admin/api/accounts/claude/{name}/refresh` | **imported** Claude 계정의 refresh 그랜트를 즉시 실행해 로그인이 아직 살아 있는지 보고. 프로바이더 토큰 엔드포인트를 호출하므로 rate limit이 걸리며, 공용 크리덴셜 스토어를 반드시 경유해 프록시 경로의 갱신과 경합하지 않는다. 새 `expires_at`만 반환하고 토큰 물질은 절대 반환하지 않으며, 프로브 자신의 clear 이후 풀에서 다시 읽은 `needs_relogin`을 함께 싣는다 — 풀이 여전히 죽은 것으로 보는 계정에도 그랜트는 성공할 수 있으므로, `/admin/api/pool`과 모순되는 복구를 주장하지 않고 그대로 보고한다. `setup_token` 계정(refresh 그랜트 없음)이나 모든 종결 판정에는 `400`, 일시적 실패에는 `502` |
| `DELETE` | `/admin/api/accounts/claude/{name}` | 해당 이름 Claude 계정의 스토어 파일 제거 |
| `POST` | `/admin/api/accounts/codex` | `{name}`으로 ChatGPT OAuth 시작; `{authorize_url}` 반환 |
| `POST` | `/admin/api/accounts/codex/{name}/complete` | 전체 localhost redirect URL 또는 `<code>#<state>`가 담긴 `{code}`로 Codex 프로비저닝 완료 |
| `DELETE` | `/admin/api/accounts/codex/{name}` | 해당 이름 Codex 계정의 스토어 파일 제거 |
| `GET` | `/admin/assets/{*path}` | 내장된 관리자 SPA 번들 파일. 확장자에 해당하는 `Content-Type`과 `X-Content-Type-Options: nosniff`를 함께 반환합니다. `--features ui`로 빌드한 바이너리에만 존재 |
| `GET` | `/admin/{*path}` | `/admin` 마운트 아래에서 다른 라우트에 걸리지 않은 경로에 대한 SPA 셸. 클라이언트 측 딥링크가 새로고침에도 유지됩니다. `--features ui`로 빌드한 바이너리에만 존재 |
| `POST` | `/backend-api/codex/responses` | 인바운드 Codex CLI 패스스루 — 실제 ChatGPT 백엔드 경로 미러 |
| `POST` | `/responses` | 인바운드 Codex CLI 패스스루 — bare `base_url` 형식 |
| `POST` | `/v1/responses` | 인바운드 Codex CLI 패스스루 — `/v1` 접미 `base_url` 형식 |
| `POST` | `/backend-api/codex/analytics-events/events` | Codex CLI 분석 sink — 수락 후 폐기하고 정제된 이벤트 이름 카운터만 기록 |
| `POST` | `/codex/analytics-events/events` | Codex CLI 분석 sink — 루트형 `chatgpt_base_url` 형식 |
| `GET` | `/usage` | 클라이언트용 정제된 풀 사용량 — 공유 계정 풀의 창별 잔여 여유와 리셋, 그리고 풀링되는 프로바이더별 동일 집계를 반환하며 계정 신원이나 용량은 반환하지 않음 |

`/admin*` 라우트는 [`[server.admin]`](/ko/reference/configuration/#serveradmin-선택)이 구성된 경우에만 존재합니다; 그 테이블이 없으면 하나도 등록되지 않습니다. 관리자 자격 증명은 구성된 헤더 또는 `x-api-key`로 받으며, `read_keys` 자격 증명은 위의 모든 GET을 통과하지만 모든 변경 작업에서는 `403`으로, `POST /admin/login`에서는 `401`로 거부됩니다. 다만 SPA 셸과 번들 파일은 예외로, `GET /admin/{*path}`와 `GET /admin/assets/{*path}`는 관리자 인증 없이 제공됩니다. 이 둘은 운영자 데이터를 담지 않고, SPA가 읽는 값은 모두 요청마다 인증하는 `/admin/api/*` 뒤에 있으므로 안전합니다. 두 라우트 역시 `[server.admin]`이 구성되고 `--features ui`로 빌드한 바이너리에서만 존재합니다.

### 관리자 SPA 번들(`--features ui`)

`/admin/assets/{*path}`와 `/admin/{*path}` SPA 폴백은 `--features ui`로 빌드한
바이너리에만 존재하며, 이 빌드는 `ui/` 패키지에서 만든 번들을 바이너리에
내장합니다. 기본 `cargo build`는 Node 툴체인이 필요 없고 번들도 없으며 두 라우트
모두 등록하지 않습니다. 미리 빌드된 릴리스 바이너리는 이 기능을 켜고 빌드합니다.

폴백은 `/admin` 마운트 안으로 한정됩니다:

- `/admin/` 아래에서 다른 라우트에 걸리지 않은 경로는 `200`과 `text/html`로 SPA
  셸을 반환하므로, 클라이언트 측 딥링크가 새로고침에도 유지됩니다.
- `/admin/api/` 아래에서 걸리지 않은 경로는 Anthropic 오류 형태의 `404`를
  반환합니다 — 이 네임스페이스는 UI가 아니라 JSON이며, HTML로 답하면 클라이언트의
  오류 처리가 깨집니다.
- 마운트 밖의 경로는 영향을 받지 않고 그대로 `404`입니다.

`GET /admin`은 계속해서 서버 렌더링 대시보드를 제공합니다.

### 관리자 경로 마이그레이션

**호환성이 깨지는 변경입니다.** 모든 관리자 JSON 라우트와 변경 라우트가 `/admin/*`에서
`/admin/api/*`로 이동했습니다. 이전 경로는 **별칭으로 남지 않고 제거되어** 이제 어떤 요청도
admin 핸들러에 도달하지 않습니다.

이전 경로에서 무엇이 돌아오는지는 빌드에 따라 다르므로, 그곳의 `200`을 성공으로 취급하면 안 됩니다:

- **기본 빌드** — 경로가 어디에도 등록되지 않아 `404`를 반환합니다.
- **`--features ui`** (미리 빌드된 릴리스 바이너리가 이쪽입니다) — `GET`은 마운트 하위의 다른 딥링크와
  마찬가지로 `/admin/{*path}` SPA 폴백으로 넘어가 `200 text/html` 셸을 반환합니다. `HEAD`도
  같습니다. 그 외 메서드는 `Allow: GET,HEAD`와 함께 `405`를 반환합니다 — 폴백이 `any`가 아니라
  `get`으로 등록돼 있기 때문입니다.

따라서 이전 `GET` URL에 남아 있는 스크립트는 마이그레이션 신호가 아니라 HTML을 받습니다.
`/admin/api/*`로 옮기세요. 응답의 `Content-Type`이 확실한 판별 기준입니다.

`/admin`은 대시보드 셸로 남고, `/admin/login`과 `/admin/oidc/callback`도 그대로입니다 — 이들은
API 라우트가 아니라 서버에서 렌더링되는 페이지입니다. 나머지는 모두 이동했으며, 이는 대시보드가
`/admin/*`을 탐색 가능한 딥 링크로 쓸 수 있게 하기 위해서입니다: 한 경로가 JSON 엔드포인트인
동시에 클라이언트 측 라우트일 수는 없으므로 호환용 별칭은 남기지 않았습니다.

스크립트로 호출하는 쪽은 기계적으로 한 번만 고치면 됩니다 — 경로에는 `/admin` 뒤에 `/api`
세그먼트 하나가 추가됩니다:

| 이전 | 이후 |
| :-- | :-- |
| `GET /admin/accounts` | `GET /admin/api/accounts` |
| `GET /admin/accounts/codex` | `GET /admin/api/accounts/codex` |
| `GET /admin/observed` | `GET /admin/api/observed` |
| `GET /admin/pool` | `GET /admin/api/pool` |
| `GET /admin/status` | `GET /admin/api/status` |
| `POST /admin/accounts/claude` | `POST /admin/api/accounts/claude` |
| `POST /admin/accounts/claude/{name}/complete` | `POST /admin/api/accounts/claude/{name}/complete` |
| `POST /admin/accounts/claude/{name}/refresh` | `POST /admin/api/accounts/claude/{name}/refresh` |
| `DELETE /admin/accounts/claude/{name}` | `DELETE /admin/api/accounts/claude/{name}` |
| `POST /admin/accounts/codex` | `POST /admin/api/accounts/codex` |
| `POST /admin/accounts/codex/{name}/complete` | `POST /admin/api/accounts/codex/{name}/complete` |
| `DELETE /admin/accounts/codex/{name}` | `DELETE /admin/api/accounts/codex/{name}` |
| `POST /admin/oidc/start` | `POST /admin/api/oidc/start` |
| `POST /admin/logout` | `POST /admin/api/logout` |

인증은 그대로입니다: 같은 관리자 자격 증명을 같은 헤더 또는 `x-api-key`로 보내며, 읽기/쓰기 등급도 동일합니다.

spend-limit 라우트는 부팅 시 [`[server.spend]`](/ko/reference/configuration/#serverspend-선택)가 구성된 경우에만 존재하며, [`[server.admin]`](/ko/reference/configuration/#serveradmin-선택) 자격 증명으로 인증하므로 `[server.gateway]`와는 무관합니다. 자격 증명은 구성된 관리자 헤더(기본값 `x-shunt-admin-token`) 또는 `x-api-key`로 보냅니다 — 두 슬롯 모두 허용됩니다. 쓰기 자격 증명(`write_keys` 항목 또는 `tokens_env`/`tokens_file` 쌍)은 모든 작업을 사용할 수 있고, `read_keys` 자격 증명은 GET만 사용할 수 있으며 변경 작업에서는 `403`을 받습니다. `POST`는 `user`와 `organization` scope, `daily`/`weekly`/`monthly` period, user scope에서는 1–256바이트인 `user_id`, 1–19자리의 음수가 아닌 USD 센트 정수 문자열 또는 `null`인 `amount`를 받아 `(scope, period)` 기준으로 upsert합니다. 목록 페이지네이션은 `limit`(1–1000, 기본값 20), `after_id`, `before_id`, `scope_type`을 받으며 두 커서는 함께 사용할 수 없습니다. 모든 응답에는 `request-id`가 포함되고 오류는 Anthropic 오류 형태를 사용합니다. 제한과 변경 감사 레코드는 구성한 버전 JSON 상태 파일에 함께 저장되며, 각 변경은 `admin-key:<id>` 또는 `admin-token:<name>`으로 귀속됩니다 — 두 슬롯이 같은 등급의 서로 다른 자격 증명을 실은 경우 구성된 관리자 헤더 쪽이 귀속 대상입니다. stage 1은 `/effective`나 `/audit`을 노출하지 않으며 추론 요청에 제한을 적용하지 않습니다.

`GET /managed/settings`와 `POST /v1/{metrics,logs,traces}` 텔레메트리 ingest 라우트는 부팅 시 `[server.gateway]`가 활성화된 경우에만 존재하며, 둘 다 같은 게이트웨이 bearer JWT를 요구합니다. ingest 라우트는 관리형 Claude Code 클라이언트가 export하는 OTLP/HTTP 페이로드를 받아([`[server.gateway.telemetry]`](/ko/reference/configuration/)가 그 exporter들을 게이트웨이로 향하게 합니다) 요청 바이트를 해당 signal에 opt-in한 모든 목적지로 그대로 relay합니다. 인바운드 `content-type`과 `content-encoding`은 유지되고 목적지에 구성된 headers가 그 위에 적용됩니다(구성된 키는 전달값을 대체하며 헤더를 중복시키지 않습니다). 클라이언트의 `Authorization` 헤더는 전달되지 않고 relay는 리다이렉트를 따르지 않습니다. 목적지는 signal별로 opt-in하며(`metrics` 기본 on, `logs`/`traces` 기본 off), 어떤 목적지도 opt-in하지 않은 signal은 수신 후 폐기됩니다. relay는 분리되어 실행되므로 목적지 상태와 무관하게 응답은 항상 즉시 `200`이고, 성공 바디는 OTLP/HTTP에 따라 요청 프로토콜을 미러링합니다(`application/json`에는 `{}`, 그 외에는 빈 `application/x-protobuf` 바디). 32 MiB 인바운드 상한을 넘는 바디는 `413`을 받습니다.

인바운드 Codex Responses 및 분석 라우트는 [`[server.codex_endpoint]`](/ko/reference/configuration/)가 구성된 경우에만 존재합니다. Responses 라우트는 OpenAI Responses 요청과 응답을 그대로 중계하며, `[[server.codex_endpoint.routes]]`로 모델별로 다른 Responses 호환 업스트림에 라우팅할 수도 있습니다. 두 분석 라우트는 같은 인바운드 인증 정책을 적용하고, 클라이언트 payload를 전달하거나 보관하지 않으며, 인증 후에는 잘못된 JSON이나 초과 크기 본문에도 `200 {}`를 반환합니다. 정제된 이벤트 이름만 `shunt.codex_client_events`에 기록되며, 메트릭 sink가 없으면 순수 폐기 sink로 동작합니다.

`/usage` 라우트는 [`[server.usage]`](/ko/reference/configuration/#serverusage-선택)가 구성된 경우에만 존재하며, [`[server.auth]`](/ko/guides/shared-gateway/)도 필요합니다. `GET /v1/messages`와 같은 클라이언트 토큰으로 인증하고 공유 계정 풀의 창별 잔여 여유(해당 창을 보고한 비활성 아님 계정들의 `mean(1 - utilization)`, 즉 풀 전체 용량 중 아직 쓰지 않은 비율), 그 계정들이 보고한 리셋 시각 중 가장 이른 값, `ok`/`degraded`/`exhausted` 상태를 반환합니다. 계정 신원, 수, priority, `disabled`, 임계값, 계정별 수치는 공개하지 않습니다. 비활성 계정이 아닌 계정 중 해당 창을 보고한 계정이 하나도 없을 때만 `null`입니다. Codex 응답의 `x-codex-*` 헤더와 선택적인 `wham/usage` 폴링은 관측된 5시간 및 공유 주간 창을 채웁니다. WebSocket 전송에서는 스트림 안의 `codex.rate_limits` 이벤트가 재사용 연결을 포함한 모든 턴에서 같은 창을 채웁니다. Codex에는 Fable 범위(`7d_oi`) 신호가 없지만 혼합 프로바이더 풀에서는 다른 프로바이더가 집계 Fable 값을 제공할 수 있습니다. `pool`은 풀링되는 모든 프로바이더를 통틀은 집계이고, `providers`는 같은 정제된 집계를 풀링되는 프로바이더별로 구성된 프로바이더 이름을 키로 담으므로, 특정 프로바이더로 라우팅하는 클라이언트는 풀 전체 평균 대신 해당 프로바이더의 여유분과 상태를 읽을 수 있습니다. 풀링되지 않는 인증 모드의 프로바이더는 생략됩니다. 전체 응답 형태는 [영문 엔드포인트 레퍼런스](/reference/endpoints/)를 참고하세요.

`GET /`와 `GET /health`는 [`[server.auth]`](/ko/guides/shared-gateway/)가 활성화되어 있어도 열린 채로 유지되며(헬스체크 도구는 보통 토큰을 첨부할 수 없음) 민감한 것을 노출하지 않습니다 — 오직 상태, 버전, 그리고 이미 공개된 엔드포인트 목록만입니다.

## 게이트웨이 프로토콜

shunt는 공식 [Claude Code LLM 게이트웨이 프로토콜](https://code.claude.com/docs/en/llm-gateway-protocol)을 구현합니다: 올바른 헤더 및 바디 필드 전달, 기능 패스스루, 시스템 프롬프트 어트리뷰션 처리. 게이트웨이 소유 오류는 Anthropic 오류 형태로 반환되고, 업스트림 컨텍스트 오버플로 오류는 Anthropic의 `prompt is too long` 표현으로 다시 쓰여 Claude Code의 [압축-재시도](/ko/guides/effort-and-context/#컨텍스트-오버플로-복구)가 발동하며, 스트리밍 응답은 버퍼링 없이 릴레이됩니다(선택적 [keepalive ping](/ko/guides/shared-gateway/#sse-keepalive-ping) 포함).
