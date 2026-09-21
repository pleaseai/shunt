---
title: 구성 레퍼런스
description: 모든 shunt.toml 키 — server, providers, routes, models.
---

파일 위치, 우선순위, 주석이 달린 예시는 [구성](/ko/guides/configuration/)을 참고하세요. 전체 템플릿: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

## Secret 참조

설정 파일의 문자열 값은 리터럴 대신 `${VAR}` 또는 `${file:/절대/경로}`로 쓸 수 있습니다. `${VAR}`는 환경 변수 `VAR`의 값으로 치환되며 `"Bearer ${TOKEN}"`처럼 더 긴 문자열 안에 포함될 수 있습니다(변수가 없으면 로드 실패). `${file:/절대/경로}`는 해당 파일의 내용(trim)으로 치환되며, 반드시 절대 경로여야 하고 필드의 값 전체여야 합니다 — 다른 문자열에 포함될 수 없습니다(파일을 읽을 수 없거나, 상대 경로이거나, 다른 문자열에 포함되어 있으면 로드 실패). `$${`는 리터럴 `${`로 이스케이프됩니다. 치환은 재귀적이지 않습니다 — 치환된 값은 다시 스캔되지 않습니다. 이 치환은 설정 파일에만 적용되며 `SHUNT_*` 환경 변수 오버라이드는 그대로 사용됩니다. 부팅, `shunt check`, [핫 리로드](https://github.com/pleaseai/shunt/blob/main/docs/config-reload.md)(SIGHUP과 파일 감시)를 포함해 매 설정 로드마다 다시 실행되므로, `${file:}`로 참조한 시크릿은 파일을 다시 쓰고 리로드를 트리거하는 것만으로 재시작 없이 교체할 수 있습니다. 다만 교체한 값이 실제로 적용되는지는 해당 필드 자신의 리로드 동작을 따릅니다. `[sentry]`와 `[otel]`은 시작 시 한 번만 초기화되므로, 이 두 섹션의 시크릿을 교체하면 설정은 갱신되지만 적용하려면 재시작이 필요합니다.

`[sentry] dsn`, `[otel.headers]` 값, `[server.gateway.telemetry] forward_to[].headers` 값, `[server.gateway.session] jwt_secret`, 그리고 `[[server.admin.write_keys]]`·`[[server.admin.read_keys]]` 각 항목의 `key` — 이 여섯 경로는 redacting secret 타입으로 진단 출력에서 `[redacted]`로 표시됩니다. 앞의 네 필드는 리터럴 값을 적어도 이전과 완전히 동일하게 동작하며, 리터럴을 담고 있으면 shunt는 부팅 시 해당 필드 경로만(값은 절대 포함하지 않음) 알리는 권고성 경고를 한 번 기록합니다. 관리자 key 배열 두 개는 예외로, 리터럴을 적으면 경고가 아니라 **설정 로드 자체가 실패**합니다.

기존 `tokens_env`, `jwt_secret_env`, `client_secret_env`, `api_key_env`, `users_env`, `token_env`, `tokens_file` 필드는 이 변경의 영향을 받지 않으며 그대로 환경 변수(또는 `tokens_file`의 경우 파일 경로)를 가리킵니다(`jwt_secret_env`는 별도로 [`session.jwt_secret`](#servergatewaysession-선택)로 대체되어 deprecated됨).

## `[server]`

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | shunt가 리슨하는 주소 |
| `default_provider` | `anthropic` | 일치하는 라우트가 없는 모든 모델의 프로바이더 |
| `shutdown_timeout_seconds` | `30` | 첫 SIGTERM/SIGINT 뒤 진행 중인 HTTP/SSE/WebSocket 작업을 드레인한 후 나머지를 취소하기까지의 초. `1`–`3600`이어야 하며 변경 후 재시작 필요 |
| `max_concurrent_requests` | `1024` | 응답 본문이 끝날 때까지 진행 중으로 계산하는 인바운드 요청의 최대 수. 초과 요청은 대기열에 넣지 않고 즉시 `503`과 `Retry-After: 1`로 거부합니다. `0`은 제한을 비활성화하며 `/`와 `/health`는 제한에서 제외됩니다. 이 키를 변경한 뒤에는 재시작해야 합니다 |
| `sse_keepalive_seconds` | `30` | SSE `ping`이 주입되기 전의 유휴 초; `0`은 비활성화([상세](/ko/guides/shared-gateway/#sse-keepalive-ping)) |

## HTTP 튜닝 테이블

`[server.access_control]`은 `allow_cidrs = []`, `deny_cidrs = []`, `trust_forwarded_for = false`를 제공합니다. deny 규칙이 우선하며 `/`와 `/health`에도 적용됩니다. allow 목록이 비어 있지 않으면 기본 거부가 되지만 두 상태 경로는 allow 검사만 면제됩니다. 전달 헤더 신뢰는 클라이언트가 보낸 값을 덮어쓰는 신뢰할 수 있는 프록시 뒤에서만 활성화하세요. 변경 후 재시작해야 합니다.

이 `trust_forwarded_for` 설정은 `[server.gateway] trust_forwarded_for`와 독립적입니다. access-control 설정은 CIDR 허용/거부 규칙에만 적용되고 gateway 설정은 디바이스 플로 속도 제한에만 적용됩니다. 두 표면 모두 신뢰할 수 있는 리버스 프록시 뒤에서 실행한다면 두 설정을 모두 활성화해야 합니다. 하나만 설정하면 다른 표면은 소켓 피어 주소를 계속 사용합니다.

`[server.limits]`의 `max_request_bytes`는 Anthropic Messages 및 인바운드 Codex Responses 요청 본문에 적용되며 기본값은 `33554432`(32 MiB)입니다. 초과 시 `413`을 반환합니다. 그 외 게이트웨이, 관리, 텔레메트리 및 분석 경로는 각 엔드포인트별 본문 제한을 유지합니다. `max_request_header_bytes`와 `max_url_length`는 기본적으로 설정되지 않으며 각각 `431`과 `414`를 반환합니다. 헤더 크기는 파싱된 모든 헤더의 이름 길이와 값 길이의 합입니다. 본문 제한은 핫 리로드되지만 헤더/URL 제한은 재시작해야 합니다.

`[server.timeouts] upstream_ttfb_ms` 기본값은 `120000`이며 `0`으로 비활성화합니다. 추론 업스트림 HTTP 응답 헤더를 기다리는 시간만 제한하므로 응답 본문과 긴 SSE 스트림에는 전체 시간 제한이 없습니다. SSE 응답을 아직 커밋하지 않은 요청은 `504 timeout_error`를 반환하고, 커밋된 스트리밍 요청은 동일한 엔벨로프를 담은 하나의 터미널 SSE `error` 이벤트로 타임아웃을 표시합니다 — 타임아웃이 체인을 진행시키는 일은 없습니다. Anthropic Messages, OpenAI Responses HTTP(웹소켓 폴백 포함), Gemini HTTP, 인바운드 Codex Responses 패스스루를 포함하며 Codex 웹소켓, Cursor, Antigravity와 보조 HTTP 호출은 포함하지 않습니다.

`[server.rate_limits.device_authorization]` 기본값은 `max = 30`, `window_seconds = 600`이고 `[server.rate_limits.device_verify]`는 `max = 10`, `window_seconds = 600`입니다. 두 per-IP 제한은 서로 독립적이며 `[server.gateway]`가 없으면 비활성 상태입니다. 변경 후 재시작해야 합니다.

## `[server.auth]` (선택)

이 테이블의 존재가 인바운드 클라이언트 토큰 인증을 활성화합니다([상세](/ko/guides/shared-gateway/)):

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | 클라이언트 토큰을 담는 헤더 |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | 쉼표로 구분된 `name:token` 쌍을 담는 env 변수 |

지정된 환경 변수에는 하나 이상의 자격 증명이 있어야 합니다. 예: `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`. 테이블이 있는데 변수가 설정되지 않았거나, 비어 있거나, 형식이 잘못되면 시작은 닫힌 채로 실패(fail closed)합니다. 게이팅되는 라우트(매핑된 `/v1/messages` 추론과 `GET /v1/models` 디스커버리)는 구성된 헤더, `Authorization: Bearer`, `x-api-key`로 토큰을 받습니다 — 여러 슬롯에 유효한 토큰이 있으면 전용 헤더가 우선합니다.

`tokens_env` 자신의 값도 다른 설정 파일 문자열과 마찬가지로 `${VAR}` / `${file:...}`로 쓸 수 있습니다([Secret 참조](#secret-참조) 참고) — shunt가 토큰을 읽어오는 환경 변수 이름을 가리키는 역할은 그대로입니다.

## `[server.admin]` (선택)

이 테이블의 존재가 브라우저 계정 프로비저닝과 계정 풀 상태를 위한 관리자 웹 화면을 활성화합니다([상세](/ko/guides/admin-remote-provisioning/)). 테이블이 없으면 `/admin*` 라우트는 하나도 등록되지 않습니다. 같은 자격 증명이 [`[server.spend]`](#serverspend-선택) spend-limit API도 인증합니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | API/curl 호출용 관리자 자격 증명을 담는 헤더. 관리자·spend-limit 라우터에서는 `x-api-key`도 함께 허용됩니다 |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | 쉼표로 구분된 `name:token` 쌍을 담는 env 변수. **쓰기(write)** 티어입니다 |
| `tokens_file` | _(설정 안 함)_ | `name:token` 쌍을 담는 파일 경로(한 줄에 하나, 또는 쉼표로 구분). `tokens_env`가 설정되지 않았거나 비어 있을 때 사용합니다. 이것도 **쓰기(write)** 티어입니다 |
| `session_ttl_secs` | `3600` | 로그인 후 브라우저 세션 수명(초) |
| `pending_ttl_secs` | `600` | 시작된 프로비저닝 플로우를 끝낼 수 있는 시간(초) |

관리자 토큰은 환경 변수나 파일에서 가져올 수 있습니다. 지정된 환경 변수에는 하나 이상의 자격 증명이 있어야 합니다. 예: `SHUNT_ADMIN_TOKENS="ops:<token>"`. 또는 `tokens_file`에 경로(`~`는 확장됩니다)를 지정하고 그 파일에 쌍을 넣어도 됩니다 — `shunt dashboard setup`이 `~/.shunt/admin-token`에 쓰는 것이 바로 이 파일이므로, 실행 환경에 비밀 값을 두지 않아도 됩니다. 둘 다 설정되면 비어 있지 않은 `tokens_env`가 우선합니다. 테이블이 있는데 세 자격 증명 소스(`tokens_env`/`tokens_file`, `write_keys`, `read_keys`)가 **모두** 비어 있거나 형식이 잘못되면 시작은 닫힌 채로 실패(fail closed)합니다. `tokens_env`를 설정하지 않고 key 배열만 쓰는 구성은 정상적으로 부팅됩니다.

관리자 자격 증명은 `[server.auth]` 아래에 구성되는 클라이언트 토큰과 별개의 자격 증명입니다; 하나의 자격 증명을 두 표면에 재사용하지 마세요. 관리자 자격 증명은 `/admin*`과 spend-limit 라우트만 인증하며 추론 라우트는 절대 인증하지 않습니다 — 그곳의 `x-api-key`는 호출자 자신의 Anthropic 자격 증명 슬롯입니다. 또한 이들 라우터가 어떤 슬롯에서 받아들인 값이든 업스트림 요청 전에 그 슬롯에서 제거되므로, 관리자 자격 증명이 provider로 전달되는 일은 없습니다.

`[server.auth]`의 `tokens_env`와 마찬가지로, 이 `tokens_env`와 `tokens_file`의 값도 `${VAR}` / `${file:...}`로 쓸 수 있습니다([Secret 참조](#secret-참조) 참고).

### `[[server.admin.write_keys]]` / `[[server.admin.read_keys]]` (선택)

`{ id, key }` 테이블을 원소로 갖는 두 개의 key 배열입니다. `id`는 로그에 남겨도 안전하며 spend-limit 감사 기록에 `admin-key:<id>`로 기록됩니다. `tokens_env`/`tokens_file` 쌍은 대신 `admin-token:<name>`으로 기록됩니다.

```toml
[[server.admin.write_keys]]
id = "terraform"
key = "${SHUNT_ADMIN_KEY_TERRAFORM}"

[[server.admin.read_keys]]
id = "reporting"
key = "${file:/run/secrets/shunt-reporting-key}"
```

| 배열 | 접근 권한 | 의미 |
| :-- | :-- | :-- |
| `write_keys` | `write` | 전체 접근. `write`는 `read`를 포함합니다. `tokens_env`/`tokens_file`과 같은 티어입니다 |
| `read_keys` | `read` | 관리자 화면과 spend-limit API의 모든 `GET`을 통과하며, 모든 변경 작업에서는 `403 permission_error`로 거부됩니다. 대시보드에는 읽기 전용 세션으로 로그인할 수 있습니다: `POST /admin/login`이 이를 받아들이고, 세션이 `read` 등급을 기록하며, 그 쿠키로 보내는 모든 변경 작업은 여전히 `403`으로 거부됩니다 |

자격 증명의 권한은 매칭된 모든 집합에 대한 **최댓값**이므로, 집합을 검사하는 순서가 권한을 바꿀 수 없습니다. 각 `id`는 공백이 아니어야 하고 각 key는 32자 이상이어야 합니다. id와 key 값은 각각 세 자격 증명 집합(`tokens_env`/`tokens_file`, `write_keys`, `read_keys`) 전체에서 고유해야 하며, 충돌하면 key 값을 로그에 남기지 않고 충돌한 id만 알립니다. 32자보다 짧은 기존 `tokens_env` 토큰은 이 규칙보다 먼저 존재했기 때문에 실패가 아니라 경고로 처리됩니다.

각 `key`는 redacting secret이며([Secret 참조](#secret-참조) 참고), 리터럴이 경고가 아니라 **설정 로드 실패**로 이어지는 유일한 필드입니다. `${VAR}`, `${file:/절대/경로}`, 또는 `SHUNT_*` 환경 변수 오버라이드로 공급하세요.

## `[server.spend]` (선택)

이 테이블의 존재가 `/v1/organizations/spend_limits` 아래의 spend-limit Admin API를 등록합니다. **정책만** 담는 최상위 섹션으로 key 자료는 전혀 갖지 않습니다: 라우트는 [`[server.admin]`](#serveradmin-선택) 자격 증명으로 인증하므로 spend limit을 켜는 데 gateway 로그인 표면이 필요하지 않습니다. `[server.admin]` 없이 `[server.spend]`만 두면 설정 검증에 실패합니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `blocked_message` | 미설정 | 향후 제한 오류에 사용할 메시지. stage 1은 사용하지 않음 |
| `audit_retention_days` | `365` | 향후 감사 레코드 보존 일수 |
| `spend_retention_months` | `13` | 향후 지출 데이터 보존 개월 수 |
| `identity_retention_days` | `90` | 향후 아이덴티티 보존 일수 |
| `group_limit_mode` | `min` | 향후 그룹 제한 결정 모드. `min` 또는 `max` |
| `state_path` | `~/.shunt/gateway-spend.json` | 제한과 감사 레코드를 저장하는 버전이 있는 JSON. `""`은 메모리 전용 |

관리자 자격 증명은 구성된 `[server.admin] header` 또는 `x-api-key`로 보냅니다. `read_keys` 자격 증명은 `GET`만 사용할 수 있습니다. 상태 파일은 변경할 때마다 비공개 임시 파일로 원자적으로 교체됩니다. 홈 디렉터리를 확인할 수 없으면 기본값은 메모리 전용입니다. 테이블의 추가·제거와 상태 경로는 모두 부팅 시 고정되며, 구성 리로드는 적용 대신 경고를 기록합니다.

### `[server.spend.enforcement]` (선택)

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `fail_closed_on_error` | `false` | 향후 제한 단계용 설정. stage 1은 읽지 않음 |

stage 1은 이 보존 설정, `blocked_message`, `group_limit_mode`, `fail_closed_on_error`를 받지만 추론 제한, 사용량 계측, `/effective`, `/audit`, 보존 sweep, group scope는 아직 구현하지 않습니다.

## `[server.gateway]` (선택)

이 테이블은 Claude Code의 managed `forceLoginMethod: "gateway"`에서 사용하는 [OAuth device-flow gateway 로그인](/ko/guides/gateway-login/)을 활성화합니다. 테이블이 없으면 shunt는 `/.well-known/oauth-authorization-server`, `/oauth/device_authorization`, `/oauth/token`, `/device`, `/managed/settings`를 등록하지 않습니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `public_url` | 필수 | JWT issuer와 OAuth endpoint 기준으로 사용하는 외부 공개 HTTPS origin. `http`는 loopback에서만 허용 |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | 32 bytes 이상의 HS256 signing secret을 담는 env 변수. **Deprecated**, 단독 사용 시 계속 완전히 지원됨 — [`session.jwt_secret`](#servergatewaysession-선택)로 대체됨 |
| `users_env` | `SHUNT_GATEWAY_USERS` | 쉼표로 구분된 `email:secret` approval user를 담는 env 변수 |
| `token_ttl_seconds` | `3600` | access token 수명. `expires_in`으로 반환. **Deprecated**, 단독 사용 시 계속 완전히 지원됨 — [`session.ttl_hours`](#servergatewaysession-선택)로 대체됨. 다만 시간 미만 수명을 지정할 수 있는 유일한 방법으로는 계속 남아 있음 |
| `trust_forwarded_for` | `false` | `/device` rate-limit identity로 `X-Forwarded-For`/`X-Real-IP`를 신뢰. client 제공 값을 교체하는 trusted proxy 뒤에서만 활성화 |
| `state_path` | `~/.shunt/gateway-sessions.json` | 재시작 후에도 refresh session을 유지하는 파일. token은 SHA-256 hash로 저장하고 Unix에서는 소유자 전용 권한(`0600`)으로 원자적으로 기록. `""`로 설정하면 memory-only session 사용(home directory를 찾지 못한 경우에도 동일) |

URL이 경로 등을 포함하지 않은 HTTPS origin이 아니거나(`http`는 loopback에서만 허용), TTL이 0이거나, secret이 없거나 32 bytes 미만이거나, user list가 비었거나 잘못되면 시작은 fail closed합니다. secret에는 `:`를 포함할 수 있으며 첫 번째 colon만 email과 secret을 구분합니다. `jwt_secret_env`와 `users_env`의 값도 다른 설정 파일 문자열과 마찬가지로 `${VAR}` / `${file:...}`로 쓸 수 있습니다([Secret 참조](#secret-참조) 참고). env-backed secret과 user 변경은 config reload 시 반영되지만, route tree는 boot 시 고정되므로 테이블 추가·제거에는 restart가 필요합니다.

deprecated 키와 그에 대응하는 `[server.gateway.session]` 대체 키를 함께 설정하면 키별로 시작이 실패합니다: `jwt_secret_env`와 `session.jwt_secret`를 함께 쓰면 오류이고, `token_ttl_seconds`와 `session.ttl_hours`를 함께 쓰면 오류입니다. 두 쌍을 교차해서 섞는 것(예: `session.jwt_secret`과 함께 `token_ttl_seconds`를 쓰는 것)은 문제없습니다. shunt는 deprecated 키가 설정 파일이든 `SHUNT_*` 환경 변수 override든 명시적으로 설정될 때마다 deprecation 경고를 한 번 기록하며, 그 키 자체가 전혀 설정되지 않아 기본값이 적용될 때만 조용히 넘어갑니다 — `jwt_secret_env`를 설정하지 않고 `SHUNT_GATEWAY_JWT_SECRET` env 변수에 secret 값만 담아 두는 설정은 그 변수가 deprecated 키 자체가 아니라 secret의 값을 담고 있을 뿐이므로 여전히 경고하지 않습니다. 쌍 중 한쪽만 설정된 경우 `session.*`이 있으면 그 값이, 없으면 deprecated 키가, 둘 다 없으면 기본값이 우선합니다.

발급된 bearer는 선택된 provider가 server-side credential을 주입할 때 `/v1/models`, `/v1/messages`, `/v1/messages/count_tokens`를 인증합니다. passthrough provider는 open 상태를 유지합니다. `[server.auth]`도 있으면 어느 credential이든 access를 허용합니다. refresh session은 기본적으로 재시작 후에도 유지됩니다. boot 시 `state_path`의 token hash를 복원하므로 사용자는 계속 silent refresh할 수 있습니다. 이 파일을 여러 shunt process가 동시에 공유하면 안 됩니다. `state_path = ""`이면 session은 memory-only이며, config reload에서는 유지되지만 shunt를 재시작하면 access JWT 만료 후 다시 로그인해야 합니다. Device grant와 rate-limit counter는 항상 memory-only이므로 로그인 도중 재시작하면 해당 시도만 손실됩니다. 만료된 grant와 idle rate-limit identity는 opportunistic하게 정리되며 각각 최대 4,096개로 제한됩니다. 사용한 refresh-token tombstone은 30일 동안 family당 최대 64개 유지되고, 30일 동안 사용하지 않은 active refresh token은 만료됩니다.

### `[server.gateway.session]` (선택)

Claude 앱의 gateway `session:` 블록과 대응됩니다:

```toml
[server.gateway.session]
jwt_secret = "${SHUNT_GATEWAY_JWT_SECRET}"
ttl_hours = 1
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `jwt_secret` | 이 테이블이 있으면 필수 | HS256 signing secret, 32 bytes 이상의 entropy 필요(예: `openssl rand -base64 32`). 단일 문자열이거나, rotation을 위한 배열도 가능 — index 0이 새 토큰에 서명하고 모든 항목이 검증에 쓰임 |
| `ttl_hours` | `1` | access token 수명(시간 단위, 정수) |

`jwt_secret`은 `Secret` 타입 필드입니다: 다른 설정 파일 문자열과 마찬가지로 `${VAR}` / `${file:/절대/경로}`를 쓸 수 있고([Secret 참조](#secret-참조) 참고), 진단 출력에서는 redact됩니다. 기존 세션을 무효화하지 않고 rotate하려면 새 secret을 배열 앞에 추가하고, `ttl_hours`만큼 기다려 기존 access token이 만료되게 한 뒤, 이전 항목을 제거하세요:

```toml
[server.gateway.session]
jwt_secret = ["new-secret-value", "old-secret-value"]
```

### `[[server.gateway.policies]]` (선택)

`[server.gateway]`가 있으면 인증된 `GET /managed/settings`가 등록되고, 순서가 있는 비어 있지 않은 policy 목록은 이 managed document를 제공합니다. 각 policy는 선택적 `[server.gateway.policies.match]`와 필수 open-schema `[server.gateway.policies.cli]` object를 가집니다. `match` 생략, `match = {}`, 또는 `emails` 없음은 catch-all입니다. 명시적으로 빈 `emails` 목록이나 빈 entry는 시작 오류입니다.

모든 catch-all policy를 순서대로 merge한 뒤, 첫 번째 정확한(case-sensitive) email policy를 위에 merge합니다. object는 재귀 merge하고 array는 교체하되, key에 `deny`가 포함된 array는 중복 없는 union으로 합칩니다. 알려진 key는 시작과 hot reload 때 검증합니다. `availableModels`는 string만 담은 array여야 하고, `env`는 string·number·boolean scalar value만 담은 table이어야 합니다. 알려지지 않은 key는 open-schema로 유지하지만, 모든 value는 JSON으로 표현할 수 있어야 하며 non-finite float는 거부됩니다.

`policies`가 없으면 endpoint는 `404`를 반환합니다. policy가 구성됐지만 일치하는 user-specific 또는 catch-all settings가 없으면 telemetry 활성 시 telemetry 전용 `settings.env`를, 비활성 시 `settings: {}`를 담은 `200`을 반환합니다. response에는 `uuid`, `checksum`, checksum을 담은 quoted `ETag`가 있으며, 일치하는 `If-None-Match`에는 `304`를 반환합니다.

해석된 `cli.availableModels`는 gateway JWT request의 `/v1/messages`와 `/v1/messages/count_tokens`에 적용됩니다. top-level `model` 끝의 Claude Code context-window hint(`[1m]` 또는 `[1M]`) 하나를 제거한 뒤 비교하며, 목록에 없으면 `400 invalid_request_error`로 거부합니다. static `[server.auth]` credential은 gateway policy user를 식별하지 않으므로 이 제한을 받지 않습니다.

### `[server.gateway.telemetry]` (선택)

`forward_to`는 필수 base OTLP/HTTP `url`, 선택적 string `headers` map, signal별 opt-in boolean(`metrics` 기본 `true`, `logs`/`traces` 기본 `false`)을 가진 destination array입니다. `headers`의 각 값은 redacting secret 타입으로 진단 출력에서 `[redacted]`로 표시됩니다([Secret 참조](#secret-참조) 참고). signal을 하나 이상 opt-in한 목록은 managed `settings.env`에 값 6개를 주입합니다. `CLAUDE_CODE_ENABLE_TELEMETRY=1`, 각 `OTEL_METRICS_EXPORTER`/`OTEL_LOGS_EXPORTER`/`OTEL_TRACES_EXPORTER`는 해당 signal을 opt-in한 destination이 있으면 `otlp`, 없으면 `none`, `OTEL_EXPORTER_OTLP_ENDPOINT=public_url`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`입니다. 어떤 signal도 opt-in되지 않았으면 아무것도 주입하지 않습니다. 충돌 시 policy env value가 우선합니다. 같은 목록이 inbound ingest도 구동합니다(M-C, #189). `[server.gateway]`가 있으면 항상 등록되는 `POST /v1/{metrics,logs,traces}` 라우트가 클라이언트의 OTLP payload를 받아 해당 signal을 opt-in한 모든 destination에 verbatim으로 relay하고, opt-in한 destination이 없는 signal은 수신 후 폐기합니다. `logs`/`traces`가 기본 off인 이유는 Claude Code log record와 span에 command line, prompt, 파일 경로가 담길 수 있기 때문입니다.

```toml
[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
headers = { "x-api-key" = "..." }
```

기본적으로 `/device`는 forwarding header를 무시하고 socket peer를 rate limit합니다. shunt가 client 제공 forwarding header를 제거하고 자체 값을 설정하는 trusted reverse proxy를 통해서만 도달 가능한 경우에만 `trust_forwarded_for = true`를 설정하세요. 직접 노출된 gateway에서는 활성화하지 마세요.

## `[server.codex_endpoint]` (선택)

이 테이블은 **Codex CLI**가 `base_url`을 shunt로 지정하고 ChatGPT/Codex OAuth 계정 풀 사이에서 load balancing될 수 있도록 inbound OpenAI Responses passthrough를 활성화합니다([상세](/ko/guides/inbound-codex-endpoint/)). 테이블이 없으면 해당 route는 등록되지 않습니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `provider` | `codex` | 어떤 route에도 `model`이 일치하지 않는 inbound request를 처리할 `[providers.<name>]` 테이블 이름. `auth = "chatgpt_oauth"`를 사용해야 함 |
| `routes` | `[]` | 선택적인 모델별 라우팅(아래 참고) |

`POST /backend-api/codex/responses`, `POST /responses`, `POST /v1/responses`를 등록하며, 모두 지정한 provider의 account pool이 처리합니다. `[server.auth]`가 있으면 다른 server-side credential route처럼 유효한 client token을 요구합니다. `[server.auth]`가 없으면 operator의 Codex credential을 주입하면서도 접근 가능한 누구에게나 **open** 상태이므로 loopback 외 환경에서는 반드시 보호하세요. `/v1/messages`와 달리 request는 Anthropic Messages로 변환하거나 그 반대로 변환하지 않고 upstream과 verbatim relay합니다.

### `[[server.codex_endpoint.routes]]` (선택)

각 항목은 위의 고정 `provider` 대신 특정 모델 하나를 다른 Responses 호환 upstream으로 보냅니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `model` | *(필수)* | Codex 클라이언트가 Responses 본문에 보내는 공개 모델 id. **정확 일치**이며 **대소문자를 구분**합니다 — prefix 매칭도, `[1m]` 제거도, 문자 집합 제한도 없으므로 `MiniMax-M3`, `openai/gpt-5.6-sol`, `~openai/gpt-latest` 같은 벤더 슬러그도 적은 그대로 라우팅됩니다 |
| `provider` | *(필수)* | 이 모델을 제공할 provider. `kind = "responses"`여야 하며 자격 증명이 없는 auth 모드(`passthrough` 또는 `none`)를 쓰면 안 됩니다 |
| `upstream_model` | `model` | upstream으로 보낼 모델 id. `model`과 다르면 shunt가 본문 최상위 `model`만 바꾸고 나머지 필드는 그대로 둡니다 |

알 수 없는 provider, `responses`가 아닌 provider, 자격 증명이 없는 인증 모드(`passthrough` 또는 `none`)를 쓰는 provider로 향하는 route는 검증에서 거부되며 중복된 `model`이나 빈 필드도 거부됩니다. route는 라이브 config 스냅샷에서 읽으므로 추가·수정·삭제가 **리로드** 시점에 반영됩니다. 재시작이 필요한 것은 `[server.codex_endpoint]` 테이블 자체를 켜고 끌 때뿐입니다. ChatGPT가 아닌 provider로 라우팅된 request는 새로 만든 헤더 허용 목록(`content-type`, `accept`, flavor 게이트를 통과한 `OpenAI-Beta`, 그리고 `xai_oauth` 라우트의 경우 Grok CLI identity 헤더)과 identity 인코딩 본문, 자격 증명 하나만 사용하며 풀도 페일오버도 없습니다.
같은 옵트인이 `GET /models`와 `GET /backend-api/codex/models`를 등록하며, 이 경로들은 일반 모델 탐색 인증 게이트 후 유효한 Codex 폴백 `{"models":[]}`를 반환합니다. 공유 `GET /v1/models`에서도 `client_version` 쿼리가 있으면 Anthropic 형태의 헤더보다 우선하여 Codex 빈 형태를 선택합니다. `client_version`이 없으면 기존 Anthropic 탐색 응답은 변경되지 않습니다. shunt는 불완전한 Codex `ModelInfo` 행을 만들지 않습니다.

## `[server.usage]` (선택)

이 테이블은 공유 계정 풀의 쿼터 상태를 정제해 집계한 클라이언트용 `GET /usage`를 등록하므로, 관리자 화면 없이도 클라이언트가 스로틀링을 예상할 수 있습니다([엔드포인트 상세](/ko/reference/endpoints/)). 테이블이 없으면 라우트도 등록되지 않습니다.

현재 이 테이블에는 키가 없으며, 존재만으로 활성화됩니다. [`[server.auth]`](#serverauth-선택)가 필수입니다. 엔드포인트는 클라이언트 토큰으로 호출자를 식별하므로 `[server.auth]` 없이 `[server.usage]`를 설정하면 시작이 실패하고, 인증 없이 풀 텔레메트리를 제공하지 않습니다.

`GET /usage`는 `/v1/messages`와 같은 클라이언트 토큰(구성된 헤더, `x-api-key`, `Authorization: Bearer`)으로 인증하고 창별 잔여 여유(해당 창을 보고한 비활성 아님 계정들의 `mean(1 - utilization)`, 즉 풀 전체 용량 중 아직 쓰지 않은 비율 — 소진된 계정 9개와 새 계정 1개면 `0.1` — 풀 전체 집계이지 다음 요청이 통과될지에 대한 예측은 아님), 그 계정들이 보고한 리셋 시각 중 가장 이른 값, `ok`/`degraded`/`exhausted` 상태를 반환합니다. 계정 이름, 수, priority, `disabled`, 임계값, 계정별 수치는 노출하지 않습니다. 비활성 계정이 아닌 계정 중 해당 창을 보고한 계정이 하나도 없을 때만 창이 `null`입니다. Codex 응답의 `x-codex-*` 헤더는 5시간 및 공유 주간 창을 채웁니다. Codex 자체에는 Fable 범위(`7d_oi`) 신호가 없지만 혼합 프로바이더 풀에서는 다른 프로바이더가 집계 Fable 값을 제공할 수 있습니다. 양수 `usage_refresh_seconds`를 설정하면 선택적인 `wham/usage` 폴러도 imported이며 갱신 가능한 `chatgpt_oauth` 계정의 해당 창을 채웁니다. 폴링은 기본적으로 꺼져 있습니다.

응답은 풀 전체 집계를 `pool`에 담고, `providers`에는 풀링되는 프로바이더별로 같은 정제된 집계를 구성된 프로바이더 이름(`[providers.<name>]`의 `<name>` 또는 `[[upstreams]]` 항목의 `name`이며, 계정 신원이 아님)을 키로 담습니다. 모델을 그 키에 대응시키는 것은 클라이언트의 몫입니다. [`GET /routes`](/ko/reference/endpoints/)는 `[[routes]]`에 명시된 모델만 다루고, `[[models]].upstream_model`, `[[route_prefixes]]`, `server.default_provider` 매핑을 노출하는 엔드포인트는 없으며, `GET /v1/models` 항목에는 프로바이더 필드가 없습니다. 혼합 풀에서 `pool`은 모든 프로바이더의 계정을 하나의 평균으로 섞어 보고하므로, 특정 프로바이더로 라우팅하는 클라이언트는 해당 프로바이더의 여유분과 상태를 `providers.<name>`에서 읽어야 합니다. 풀링되지 않는 인증 모드의 프로바이더는 생략되며, Fable 범위 신호가 없는 프로바이더의 `fable` 창은 `pool`이 값을 보고하더라도 `null`입니다. 전체 형태는 [엔드포인트 레퍼런스](/ko/reference/endpoints/)를 참고하세요.

## `[server.pool]` (선택)

계정 풀을 위한 쿼터 인지 로드 밸런싱 튜닝 — Claude(Anthropic)([상세](/ko/guides/anthropic-multi-account/#선택-튜닝-serverpool))와, 이슈 #195부터는 Codex/ChatGPT([상세](/ko/guides/codex-multi-account/)). 테이블이 없으면 선택은 이 테이블이 존재하기 이전과 동일하게 단일 내장 `0.98` 임계값을 사용합니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | 모든 쿼터 창에 대한 안전 백스톱; 이 값 이상인 계정은 사용 가능한 계정 중 항상 마지막으로 정렬됨 |
| `default_threshold` | 미설정 | 더 구체적인 값이 없는 모든 창에 적용되는 소프트 기본 임계값 |
| `default_threshold_5h` | 미설정 | 5시간 창의 소프트 기본값 |
| `default_threshold_7d` | 미설정 | 공유 주간(`7d`) 창의 소프트 기본값 |
| `default_threshold_fable` | 미설정 | fable 전용 주간(`7d_oi`) 창의 소프트 기본값 |
| `burn_rate_avoidance` | `false` | 창이 리셋되기 전에 소프트 임계값을 소진할 것으로 예측되는 계정도 함께 회피 |
| `usage_refresh_seconds` | 비활성(`0`/미설정) | Claude `GET /api/oauth/usage`와 Codex `GET /wham/usage`의 폴링 간격(초); 60 미만의 양수 값은 60초 하한으로 올림 |
| `state_path` | 미설정 | 풀의 계정별 쿼터 상태를 저장할 파일; 재시작 시 빈 풀 대신 마지막으로 관측된 사용률에서 워밍업. 미설정이면 영속화 비활성(기본값) |
| `ramp_initial_concurrency` | 비활성(`0`/미설정) | 폭주 제어: 방금 트래픽을 받기 시작한 계정 아이덴티티의 초기 동시 허용치. `0` 또는 미설정이면 허용 게이팅 비활성 |
| `reprobe_seconds` | 이 테이블이 존재하면 `900`; `0`이면 비활성 | 오래된 근접 쿼터 Codex/ChatGPT 계정을 위한 기회적 재탐침 간격(초); 60 미만의 양수 값은 60초 하한으로 올림. `0`이면 재탐침 비활성; `[server.pool]` 자체가 없으면 이 값과 무관하게 재탐침 비활성(#135 이전 동작). 비WS outbound Responses 선택과 선택형 inbound Codex HTTP 엔드포인트는 재탐침을 유지하며, WebSocket을 켠 outbound 선택은 비활성화 |

각 창 `X`에 대해 유효 소프트 임계값은 다음 순서로 결정됩니다: 계정 `threshold_X` → 계정 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, 그리고 `hard_threshold`로 상한이 걸립니다. 모든 임계값은 `[0.0, 1.0]` 범위의 사용률 비율이며, 범위를 벗어나면 시작이 실패합니다. 임계값과 번-레이트 노브는 두 풀 계열 모두를 관장합니다: Anthropic 풀은 `anthropic-ratelimit-unified-*` 헤더로부터, Codex/ChatGPT 풀은 `x-codex-*` 5시간/주간 윈도우로부터 동작합니다(Codex에는 Fable 범위의 `7d_oi` 창이 없어 `default_threshold_fable`은 그곳에서 무력화됩니다). `usage_refresh_seconds`는 `claude_oauth` 계정뿐 아니라, 비공식 `wham/usage` 엔드포인트를 통해 Codex/ChatGPT 백엔드 `chatgpt_oauth` 계정도 폴링합니다.

양수 `usage_refresh_seconds`는 추가로 백그라운드 폴러를 시작해, 각 계열의 usage API와 대조해 계정 풀의 쿼터 상태를 재보정합니다: `claude_oauth` 계정은 공식 Anthropic OAuth usage API와, Codex/ChatGPT 백엔드 `chatgpt_oauth` 계정은 비공식 `wham/usage` 엔드포인트와 대조합니다. 미설정 또는 `0`이면 비활성(기본값)입니다. 두 계열 모두 imported(갱신 가능) 계정만 폴링되며 — 장기 `claude setup-token`이나 어느 계열이든 `token_env` 계정은 usage 엔드포인트가 비갱신 토큰을 거부하므로 건너뜁니다. Claude 폴러는 보고된 창의 사용률, 창 고유 리셋 시각과 사용률 관측 시각을 갱신합니다. 창별 및 집계 status의 freshness와 status 관측 때 캡처한 리셋 경계만 헤더에서 유지하며, shunt 외부의 동일 계정 소비까지 포함한 권위 있는 사용량과 대조하지만 status 수명은 연장하지 않습니다. Codex 폴러는 사용률과 사용률 관측 시각을 갱신하며, 리셋 메타데이터는 응답에서(`x-codex-*` 헤더와 WebSocket `codex.rate_limits` 이벤트), status 메타데이터는 헤더에서 유지합니다. 보고된 창에서는 미래의 저장된 리셋을 유지하고, 저장된 리셋이 이미 지났으면 새 사용률을 쓰기 전에 그 리셋만 지웁니다. wham의 `reset_at`은 실제 리셋 메타데이터로 채택하지 않습니다. 비공개 스키마는 lenient하고 fail-soft하게 해석되며, 간격은 부팅 시 고정되고 설정 리로드는 폴러를 시작·중지·재조정하지 않습니다.

`state_path`는 풀의 쿼터 상태(모든 provider 계정의 창별 사용률과 각 창의 고유 리셋 시각, 사용률과 status의 독립 관측 시각 및 캡처한 status 리셋 경계)를 디스크에 저장합니다. 없으면 재시작이 빈 풀로 시작해, 각 계정이 재시작 후 첫 응답 전까지 미관측 상태로 보이면서 burn-rate 회피가 비활성화되고 `GET /usage`가 트래픽으로 풀이 다시 채워질 때까지 빈 값을 반환합니다. 이 파일은 권위 있는 소스가 아니라 best-effort 캐시입니다 — 쿼터는 어차피 업스트림 응답에서 재도출되므로, 파일이 없거나·오래됐거나·손상돼도 cold start만 발생할 뿐 부팅 실패로 이어지지 않습니다. 쓰기는 비공개 temp 파일(Unix에서 `0600`)을 대상 위로 원자적으로 rename하는 방식이며, 쿼터가 변경됐을 때만 백그라운드 타이머로 이뤄집니다. 쓰기에 실패하면 다음 tick에서 재시도합니다. 쿨다운은 저장되지 않고(재시작 시 소멸), 복원된 창 중 이미 리셋이 지난 것은 복원 시 import 단계에서 첫 선택이나 snapshot보다 먼저 폐기됩니다. 사용률은 자체 관측 시각 상한과 해당 창의 리셋 중 이른 시각에 만료되고, 상한만 지났으면 해당 창의 미래 리셋을 남깁니다. status는 자체 관측 시각 상한과 관측 때 캡처한 status 리셋 경계 중 이른 시각에 만료되며 캡처한 경계도 함께 지워집니다. 버전 2 파일은 명시적 migration 경로로 버전 3으로 다시 쓰며, `observed_at_status`가 없는 집계 `status`는 저장된 `reset_5h`, `reset_7d`, `reset_7d_oi` 중 가장 이른 리셋을 변경할 수 없는 기한으로 포착합니다. 그 리셋이 이미 지났으면 만료된 리셋, stamp가 없는 집계 `status`, 합성한 stamp를 같은 import에서 함께 제거합니다. 7일이라는 타당한 범위를 넘는 미래 리셋은 부팅 시각부터 7일 후를 상한으로 삼고, 리셋이 없으면 부팅 시각부터 7일 cap을 시작합니다. 이미 stamp된 v2 값은 리셋으로 다시 해석하지 않지만, 일반 import는 고아 메타데이터를 정규화하고 경과한 신호를 만료시키며 미래 시각을 부팅 시각으로 보정하고, 남은 stamp 없는 집계에는 필요하면 부팅 시각을 넣습니다. 이후 reset-only나 usage 갱신은 포착한 기한을 연장하지 않으며 v3으로 다시 쓴 뒤 두 번째 복원에서도 같은 상태를 유지합니다. 버전 3의 리셋 없는 status는 reset-only 갱신 뒤에도 리셋 없는 상태로 유지됩니다. 경로는 부팅 시 고정되며, 설정 리로드는 영속화를 시작·중지하거나 경로를 바꾸지 않습니다.

양수 `ramp_initial_concurrency`는 모든 계정 풀에 **폭주 제어**(storm control)를 활성화합니다: 페일오버 전환 후에는 진행 중인 동시 요청이 방금 선택된 계정에 한꺼번에 몰릴 수 있습니다. 게이트를 켜면, 방금 트래픽을 받기 시작한 아이덴티티(신규, 쿨다운에서 복귀, 또는 60초간 유휴)는 최대 구성된 개수만큼의 동시 요청만 허용합니다; 성공 응답마다 허용치가 두 배로 늘고(슬로 스타트), 페일오버에 해당하는 실패는 램프를 다시 시작하며, 거부된 요청은 선택 순서상 다음 계정으로 넘어갑니다. 마지막 남은 후보는 게이트와 무관하게 항상 시도되므로, 게이팅은 요청을 미룰 수는 있어도 게이트가 없었다면 서빙됐을 요청을 실패시키는 일은 절대 없습니다. 이는 곧 풀의 모든 계정이 하나의 업스트림 아이덴티티로 귀결되면 사실상 게이트가 없는 것과 같다는 뜻이기도 합니다: 유일한 후보가 곧 마지막 후보이므로, 이 설정은 서로 다른 계정 아이덴티티가 둘 이상일 때만 효력이 있습니다.

`reprobe_seconds`는 out-of-band usage 폴러가 없거나 다음 폴링을 기다리는 Codex/ChatGPT 풀을 위한 안전망입니다. rotation 대표 계정이 Codex/ChatGPT 계열이고 근접 쿼터이며 쿨다운이 아니고 최신 관측 시각이 이 간격보다 오래됐으면 간격당 한 번 선택 순서 맨 앞으로 승격하고 예약합니다. 신선도는 네 논리 값으로 판단합니다. 5h, 공유 7d, Fable 7d_oi에서는 각각 사용률 관측 시각과 status 관측 시각 중 최신 값을 사용하고, 네 번째 값으로 독립된 aggregate status 관측 시각을 사용합니다. 사용률만 갱신하는 폴링은 사용률 신선도만 갱신하고 창별 status 신선도는 갱신하지 않습니다. admission이나 자격 증명 확인에 실패하면 예약을 취소하고 첫 실제 HTTP 전송이 시작될 때 probe 시각과 `shunt.pool.reprobes`를 커밋합니다. 그러면 다음 실제 요청이 그 계정의 쿼터를 갱신하므로 먼 미래의 주간 리셋까지 계정이 계속 배제 상태로 남는 일을 막습니다. Codex/ChatGPT 계정만 대상입니다. Claude와 Kimi는 일반 429 거부 시 더 느린 쿨다운 복구(`PauseSame`, 최대 5분)를 쓰므로 기회적 탐침이 실제 요청을 지연시킬 위험이 있고 Claude 계정에는 대신 위의 `usage_refresh_seconds`가 있습니다. 구성한 폴러는 imported이며 갱신 가능한 `chatgpt_oauth` 계정에만 조기 복구를 제공하고, 폴러가 없거나 대상이 아닌 계정의 outbound 마크는 관측시각 기반 창 수명 경계에서 만료됩니다. 재탐침은 out-of-band 메타데이터 폴링인 `usage_refresh_seconds`와 달리 승격마다 실제 업스트림 요청 하나만큼의 트래픽 비용이 듭니다. 프로바이더의 WebSocket 전송을 켜면 outbound Responses 풀은 예약을 만들지 않고 재탐침을 억제합니다. 선택형 inbound Codex HTTP 엔드포인트는 계속 탐침하며 해당 프로바이더의 `shunt.pool.reprobes`는 inbound 탐침만 셉니다.

## `[server.status]` (선택)

provider Statuspage `summary.json` 엔드포인트를 관측 목적으로만 백그라운드 폴링합니다. 이 정보는 라우팅, 페일오버, pool/cooldown 동작에 영향을 주지 않습니다. 공유 상태는 `shunt.upstream.status` 메트릭과 admin dashboard의 "Upstream status" 영역에만 표시됩니다. 테이블이 없거나 `sources`가 비어 있으면 poller가 시작되지 않습니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `refresh_seconds` | `300` | polling 간격(초). 60 미만의 양수는 60초로 올림. `0`이면 polling 비활성 |
| `sources` | `[]` | polling할 Statuspage `summary.json` 엔드포인트별 `{ provider, url }` 테이블 배열 |

```toml
[server.status]
refresh_seconds = 300

[[server.status.sources]]
provider = "claude"
url = "https://status.claude.com/api/v2/summary.json"
```

각 source에는 비어 있지 않은 고유 `provider` label과 query, fragment, embedded credential이 없는 `http`/`https` URL이 필요합니다. 잘못된 설정은 시작 시 거부됩니다. 아직 첫 polling이 끝나지 않은 source는 `unknown`으로 표시되며, polling 실패·non-2xx response·1 MiB 초과 body·잘못된 JSON·알 수 없는 indicator도 false all-clear 대신 `unknown`으로 저장됩니다.

## `[[upstreams]]` (순서가 있는 페일오버)

`[[upstreams]]`는 이름이 지정된 업스트림의 순서 있는 배열입니다. 선언 순서가 전역 페일오버 순서이며, 모델의 `[models.upstream_model]` 맵이 참여할 항목을 선택합니다. 맵에 적힌 텍스트 순서는 라우팅에 영향을 주지 않습니다.

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "kimi-overflow"
provider = "kimi"

[[upstreams]]
name = "codex-fallback"
provider = "codex"

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
kimi-overflow = "kimi-k2"
codex-fallback = "gpt-5.2"
```

이 예시는 `anthropic-primary`, `kimi-overflow`, `codex-fallback` 순서로 시도합니다. 모델 맵에 없는 업스트림은 참여하지 않습니다.

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `name` | 예 | 비어 있지 않은 고유 업스트림 이름. 라우트, 모델 맵, `server.default_provider`, 메트릭, 관리자 화면에서 사용합니다. |
| `provider` | `kind`와 `base_url`을 직접 설정하지 않은 경우 | 내장 preset. `kind`, `base_url`, 기본 auth를 제공합니다. 명시한 필드가 preset 값을 덮어씁니다. |
| `kind` | preset이 없는 경우 | `anthropic`, `responses`, `cursor`, `gemini`, `antigravity`, `antigravity_cli` 중 하나. 뒤의 세 종류는 아래 preset 표에 항목이 없으므로 — 내장 `[providers.gemini]`, `[providers.antigravity]`, `[providers.antigravity-cli]` 테이블은 preset이 아니라 별개의 레거시 방식입니다 — 정렬 업스트림에서 `kind`를 직접 지정해야 합니다. CLI provider의 테이블 이름은 하이픈을 쓰는 `antigravity-cli`이지만, `kind` 값은 밑줄을 쓰는 `antigravity_cli`입니다. |
| `base_url` | preset이 없는 경우 | 업스트림 base URL. `kind = "cursor"`에서는 로그인/토큰 갱신 엔드포인트에만 사용됩니다. 추론은 고정 에이전트 호스트인 `https://agentn.global.api5.cursor.sh`를 사용하며, `SHUNT_CURSOR_AGENT_BASE_URL`로만 재정의할 수 있습니다. |
| `auth` | 아니요 | auth mode 문자열 또는 mode별 맵. 기본값은 preset의 auth이며, preset도 없으면 `passthrough`입니다. |
| `effort`, `classifier_model`, `count_tokens`, `websocket`, `tool_search`, `request_compression`, `retry` | 아니요 | 레거시 provider에 설명된 것과 같은 업스트림별 설정. preset은 `count_tokens`를 덮어쓰지 않습니다. Cursor 업스트림에서도 `retry`는 정규화되지만 Cursor 스트리밍 턴에는 적용되지 않습니다. |

사용 가능한 preset은 다음과 같습니다.

| Preset | Kind | Base URL | 기본 auth |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `https://api.anthropic.com` | `passthrough` |
| `codex` | `responses` | `https://chatgpt.com/backend-api` | `chatgpt_oauth` |
| `openai` | `responses` | `https://api.openai.com/v1` | `api_key`, env `OPENAI_API_KEY` |
| `xai` | `responses` | `https://api.x.ai/v1` | `api_key`, env `XAI_API_KEY` |
| `grok` | `responses` | `https://cli-chat-proxy.grok.com/v1` | `xai_oauth` |
| `kimi` | `anthropic` | `https://api.moonshot.ai/anthropic` | `api_key`, env `MOONSHOT_API_KEY` |
| `cursor` | `cursor` | `https://api2.cursor.sh` | `cursor_oauth` |
| `kimi-code` | `anthropic` | `https://api.kimi.com/coding` | `kimi_oauth` |
| `zhipu` | `anthropic` | `https://open.bigmodel.cn/api/anthropic` | `api_key`, env `ZHIPUAI_API_KEY` |
| `minimax-cn` | `anthropic` | `https://api.minimax.cn/anthropic` | `api_key`, env `MINIMAX_API_KEY` |
| `opencode` | `anthropic` | `https://opencode.ai/zen` | `api_key`, env `OPENCODE_API_KEY`, 헤더 `x_api_key` |

`auth = "claude_oauth"` 같은 문자열은 `auth = { mode = "claude_oauth" }`의 축약형입니다. `api_key` 맵은 `env`(preset이 제공하지 않으면 필수)와 `header`를 받습니다. `header`를 생략하면 기본값(`bearer`, 또는 `opencode` preset의 `x_api_key`)이 그대로 사용됩니다. `claude_oauth`와 `chatgpt_oauth` 맵은 `account = "name"` 또는 `accounts = [...]`로 범위를 좁힐 수 있지만 둘을 함께 쓸 수는 없습니다. `accounts`에는 스토어 항목 이름 문자열과 전체 계정 테이블을 넣을 수 있습니다. 명시적인 `accounts = []`는 거부되며, 두 범위 필드를 모두 생략하면 전체 스토어를 스캔합니다. ChatGPT 스토어가 비어 있으면 `chatgpt_oauth`는 기존 `~/.codex/auth.json` fallback을 유지합니다. `passthrough`, `xai_oauth`, `cursor_oauth`, `antigravity_oauth` 맵에는 `mode`만 사용할 수 있으며, mode별로 알 수 없는 키는 오류입니다.

구성 파일에서 `[[upstreams]]`와 `[providers.*]`를 함께 선언하지 마세요. 파일 계층에 두 선언 형식이 모두 있으면 시작에 실패합니다. 환경 변수는 어느 형식에서든 정규화된 업스트림/provider 이름을 기준으로 `SHUNT_PROVIDERS__<name>__<field>`를 사용해 개별 필드를 재정의할 수 있습니다. 순서가 있는 `[[upstreams]]` 배열 자체는 하나의 환경 변수로 합성하려 하지 말고 구성 파일에 선언하세요. 레거시 `[providers.<name>]`는 계속 지원되며 이름순의 암시적 업스트림으로 정규화됩니다. 이 형식은 페일오버 순서를 선언하지 않으므로 모델 맵은 항목이 없거나 하나만 있어야 합니다. 모델 맵에 여러 항목을 추가하기 전에 `[[upstreams]]`로 전환하세요.

### 페일오버 동작

여러 항목이 있는 모델 맵에서는 선언한 업스트림 순서에서 맵에 포함된 이름만 남겨 체인을 만듭니다. 업스트림 상태가 `429`, `401`, `403`, `404`, 임의의 `5xx`이거나 업스트림 응답 헤더를 받기 전에 실패하면 다음 항목으로 진행합니다. auth 설정 오류나 어댑터 자체의 검증·헤더 생성 오류처럼 업스트림 시도를 나타내지 않는 게이트웨이 로컬 오류는 즉시 반환하여 잘못된 설정이 페일오버에 가려지지 않게 합니다. `2xx` 헤더를 반환한 뒤에는 스트리밍 본문이 나중에 실패하더라도 페일오버하지 않습니다. Responses 어댑터의 스트리밍 경로는 업스트림 바이트를 받기 전에 응답을 커밋합니다. `Anthropic`/`Responses` 요소만 있고 WebSocket 트랜스포트가 없는 체인은 커밋된 스트림 안에서 페일오버를 수행하며(헤더 전 트랜스포트 실패와 전진 상태는 합성 시작이 나가기 전에 다음 업스트림을 시도), 전진할 수 없는 라우트(종단 비 2xx, Anthropic 종류 승자의 SSE가 아닌 성공 본문)는 실패를 하나의 터미널 SSE `error` 이벤트로 표시합니다. 승자의 터미널 프레임이 릴레이된 뒤의 스트리밍 본문 실패는 대신 스트림을 조용히 끝냅니다 — 턴이 이미 완료됐고, 덧붙는 error 이벤트가 완료된 응답을 손상시키기 때문입니다. TTFB 타임아웃은 절대 전진하지 않습니다. 설정된 타임아웃은 답이며 터미널 `504 timeout_error` 이벤트로 표시됩니다. 이 커밋된 경로의 응답에는 `content-type`과 `x-gateway-model`이 실리고, `[models.router]` 항목이 라우팅했거나 `[models.subagents]` 오버레이가 전환한 요청이라면 라우터 헤더 두 개(`x-gateway-routed-model`/`x-gateway-route-source`)도 함께 실립니다 — 첫 시도 전에 정해지는 값이라 어느 업스트림이 이기는지에 의존하지 않습니다. 승자에 따라 달라지는 `x-gateway-upstream`과 `x-gateway-upstream-model`은 생략됩니다 — 헤더가 커밋과 함께 나갈 때 승자를 아직 모르기 때문입니다 — 그리고 업스트림 응답 헤더(요청 id, `anthropic-ratelimit-*` 할당량 메타데이터 포함)는 Anthropic 종류 승자라도 클라이언트에 도달하지 않습니다. `x-gateway-model`은 유지되며(클라이언트가 요청한 id를 가리킴), 요청 메트릭은 시도마다 분류된 상태로 기록되고 스트림 귀속은 스트림이 승자를 알게 되는 시점부터 승자를 따릅니다.

체인을 모두 시도하면 `429` → `401`/`403` → `404` → 기타 `5xx` 우선순위로 가장 적합한 릴레이 실패를 반환합니다. 헤더 이전 실패는 최종 후보로 기억하지 않습니다. 기억한 릴레이 응답이 없으면 `all upstreams failed (N attempted)` 메시지의 `502 api_error`를 반환합니다.

`passthrough` 업스트림에서는 클라이언트 자신의 `authorization` / `x-api-key`가 페일오버 시도에서 전달되는 것은 **기본(primary)** 라우트 자체가 `passthrough`이고 해당 시도의 대상 origin이 그 기본 라우트와 일치하는 경우에 한합니다. 이때 자격 증명은 기본 라우트에 origin 전용인 클라이언트 자신의 업스트림 자격 증명이므로, **다른** origin으로의 `passthrough` 페일오버 시도는 이를 제거하고 페일클로즈(fail closed)하여 호스트 전용 토큰을 다른 출처로 재전송하지 않습니다. 동일 origin 폴백(예: 한 호스트의 passthrough 항목 2개)은 계속 자격 증명을 유지합니다. 기본 라우트가 대신 자체 자격 증명을 주입하는 경우, 클라이언트 헤더는 업스트림 자격 증명이 아니라 게이트웨이/클라이언트 시크릿이므로 모든 `passthrough` 폴백은 origin과 무관하게 이를 제거합니다. `api_key`/OAuth 업스트림은 위치와 무관하게 자체 서버 측 자격 증명을 주입합니다.

origin과 무관하게, 유지된 각 슬롯은 그 슬롯이 실제로 담고 있는 값으로도 검사됩니다. `authorization`과 `x-api-key`는 각각 그 슬롯 자신의 값이 shunt 자체가 발급한 JWT와 **모양이 같거나** — `aud` 클레임이 `"shunt"`이거나, `iss` 클레임이 이 게이트웨이의 아이덴티티이거나, `shunt_token_use` 클레임이 `"gateway-session"`(shunt만 발급하는 전용 마커)인 세 세그먼트 구조 — 설정된 `[server.auth]` 클라이언트 토큰과 일치할 때에만 제거됩니다. JWT 검사는 의도적으로 "지금 이 토큰이 인증되는가"가 아니라 "모양이 같은가"로 판단합니다: 만료된 토큰, 다른 `public_url`을 쓰는 형제 인스턴스가 발급한 토큰, `jwt_secret` 로테이션 이후 더 이상 검증되지 않는 토큰도 여전히 shunt 자신의 크리덴셜이므로 여전히 제거됩니다. 이 마커는 모양 검사에 추가된 분기일 뿐 필수 조건이 아닙니다: 마커가 존재하기 전에 발급된 토큰도 `aud`/`iss`로 여전히 일치하며, `verify` 자체도 마커를 요구하지 않으므로 이전 버전의 shunt가 발급한 토큰은 TTL 내에 있는 한 계속 인증됩니다. `apiKeyHelper`는 두 슬롯을 같은 값으로 채우므로 어느 크리덴셜이든 한쪽 또는 양쪽 슬롯에 들어올 수 있습니다. 다른 슬롯이 게이트웨이 JWT나 정적 클라이언트 토큰을 담고 있어도, 진짜 업스트림 크리덴셜을 담은 슬롯은 그대로 전달됩니다. 게이트 크리덴셜을 담은 슬롯만 제거됩니다. `[server.auth] header`에는 `authorization` 자신을 포함해 어떤 헤더 이름이든 지정할 수 있으며, 그렇게 설정하면 클라이언트는 접두사 없는 `Authorization: <token>` 형태로 인증합니다. 따라서 이 슬롯은 `Bearer` 페이로드뿐 아니라 값 전체로도 검사되며, 그런 토큰은 업스트림으로 전달되지 않습니다. 이 설정에는 한 가지 유의점이 있습니다: 추론 요청에서 shunt는 라우팅 전에 설정된 헤더를 조건 없이 제거하므로, 그 슬롯은 업스트림으로 아무것도 싣지 않습니다 — 게이트 토큰뿐 아니라 호출자 자신의 크리덴셜도 함께 사라집니다. `header`를 기본값인 전용 `x-shunt-token`으로 두면 이 충돌을 피할 수 있습니다.

프록시한 성공 응답과 최종 실패에는 모두 `x-gateway-upstream`(선택한 업스트림 이름), `x-gateway-model`(클라이언트가 요청한 id), `x-gateway-upstream-model`(매핑된 백엔드 id)이 포함됩니다 — 커밋된 스트리밍 체인 경로는 예외로, 응답에는 `content-type`과 `x-gateway-model`, 그리고 라우터가 라우팅했거나 오버레이가 전환한 요청이라면 아래의 라우터 헤더 두 개가 실리고 승자에 따라 달라지는 `x-gateway-upstream`과 `x-gateway-upstream-model`은 생략되며 업스트림 응답 헤더는 클라이언트에 도달하지 않습니다. [`[models.router]`](#modelsrouter-선택) 항목이 라우팅한 응답에는 `x-gateway-routed-model`(라우터가 고른 타깃)과 `x-gateway-route-source`(그것을 고른 이유)가 추가로 붙습니다. [스테이지 라우터](/ko/guides/stage-router/)뿐 아니라 모든 라우터 `type`에 붙습니다. [`[models.subagents]`](#modelssubagents-선택) 오버레이가 전환한 위임 턴에도 같은 헤더 두 개가 붙으며, 이때 `x-gateway-route-source`는 `subagent_type` 또는 `subagent`입니다. 둘 다 붙지 않는 경우는 라우터도 오버레이도 그 턴을 결정하지 않았을 때뿐입니다. `count_tokens`는 체인의 첫 항목만 사용하고 페일오버하지 않으며, 이 헤더 두 개는 붙이지 않습니다. `[server.codex_endpoint]`는 `[[server.codex_endpoint.routes]]` 항목이 없는 모든 모델에 대해 설정된 업스트림 하나에 고정되며, 어느 쪽이든 이 체인에 참여하지 않습니다.

### 기존 설정 마이그레이션

기존 설정은 **변경할 필요가 없습니다**. 레거시 provider의 라우팅과 이름순 선택 동작은 유지됩니다. 업그레이드 시 다음 세 가지 추가 또는 의도된 동작 변경이 적용됩니다.

1. 같은 물리적 OAuth 계정으로 해석되는 레거시 provider는 이제 quota window, health, cooldown, refresh lock, in-flight admission 상태를 공유합니다. 풀 영속화 키 스키마의 버전이 올라가며, 버전 2 쿼터 캐시는 사용률과 status freshness를 분리한 버전 3으로 한 번 migration합니다.
2. 모든 프록시 응답에 위의 `x-gateway-*` metadata 헤더 세 개가 추가됩니다.
3. Anthropic Messages 경로(`/v1/messages`)에서 Claude 또는 Codex OAuth 풀의 크기와 관계없이 모든 시도가 응답 헤더 전에 실패하면, 이제 풀별 메시지인 `all Claude OAuth accounts failed before receiving an upstream response` 또는 `all Codex OAuth accounts failed before receiving an upstream response` 대신 `all upstreams failed (N attempted)`를 반환합니다. 별도의 `[server.codex_endpoint]` 인바운드 경로는 영향을 받지 않으며 Codex 전용 메시지를 유지합니다.

순서 있는 페일오버를 사용하려면 각 `[providers.<name>]` 테이블을 같은 이름의 `[[upstreams]]` 항목으로 바꾸고, `api_key_env`, `api_key_header`, OAuth `accounts`를 `auth` 맵 안으로 옮긴 뒤, 선호 순서대로 항목을 배치하고 모델의 `upstream_model` 맵에 참여할 이름을 각각 추가하세요.

`kimi` preset은 `MOONSHOT_API_KEY`를 읽습니다. `api_key_env = "KIMI_API_KEY"`를 명시한 이전 예제는 레거시 형식에서 계속 동작하며, 업스트림에서도 `auth = { mode = "api_key", env = "KIMI_API_KEY" }`로 기존 이름을 유지할 수 있습니다. preset 기본값에 의존하는 사용자만 `MOONSHOT_API_KEY`를 export해야 합니다.

## `[providers.<name>]` (레거시)

각 프로바이더는 원하는 이름의 테이블입니다. 내장(`anthropic`, `openai`, `codex`, `xai`, `grok`, `cursor`, `gemini`, `antigravity`, `antigravity-cli`)은 부분 오버라이드할 수 있습니다 — 구성 맵은 깊은 병합됩니다.

| 키 | 값 | 의미 |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` \| `gemini` \| `antigravity` \| `antigravity_cli` | 업스트림 프로토콜 / 어댑터. `anthropic` = Messages API(패스스루, 선택적으로 키 재설정); `responses` = Anthropic Messages를 OpenAI Responses API로 변환(Responses API에는 `stop` 파라미터가 없으므로 `stop_sequences`는 조용히 버려지지 않고 변환 과정에서 게이트웨이 측으로 흉내 냅니다); `cursor` = 네이티브 Cursor ConnectRPC/protobuf AgentService 어댑터; `gemini` = Anthropic Messages를 Google Code Assist 백엔드의 Gemini `generateContent`/`streamGenerateContent`로 변환; `antigravity` = Google Antigravity 백엔드에 HTTP로 접속하며, `gemini`와 동일한 Code Assist 프로토콜을 사용하되 Antigravity 구독 토큰으로 인증하고 프로젝트 디스커버리에서 `ideType: ANTIGRAVITY`로 자신을 식별; `antigravity_cli` = **더 이상 사용되지 않음** — 업스트림 없이 로컬 Antigravity CLI 바이너리(`agy`)를 서브프로세스로 실행. `agy`가 자체 도구 호출을 처리하며 `tool_use` 블록을 반환할 수 없기 때문에, 실제로 도구 호출을 요구하는 요청 — 비어 있지 않은 `tools` 배열 또는 `any`나 `tool` 값의 `tool_choice` — 은 텍스트로 조용히 응답하지 않고 `400 invalid_request_error`로 거부됩니다. `tool_choice: none`(`tools`와 함께 있어도), 도구가 없는 `tool_choice: auto`, 빈 `tools: []`는 도구 호출을 요구하지 않으므로 모두 허용됩니다. |
| `base_url` | URL | 업스트림 base; shunt가 엔드포인트 경로를 붙입니다. `kind = "cursor"`에서는 로그인/토큰 갱신 엔드포인트에만 사용되며 에이전트/추론 호스트를 선택하지 않습니다. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` \| `google_oauth` \| `antigravity_oauth` \| `none` | `passthrough`는 클라이언트 본인의 credential을 전달; `api_key`는 `api_key_env`의 키를 주입; `chatgpt_oauth`는 `~/.codex/auth.json`을 재사용; `claude_oauth`는 명시적 Anthropic 계정에서 선택; `xai_oauth`는 `shunt login xai`의 `~/.shunt/xai-auth.json`을 재사용(HTTPS를 통한 x.ai/grok.com 호스트에만 전송); `cursor_oauth`는 `~/.shunt/cursor-auth.json`을 재사용(`shunt login cursor`); `google_oauth`는 gemini CLI 로그인의 `~/.gemini/oauth_creds.json`을 재사용하며 `kind = "gemini"`에서만 유효; `antigravity_oauth`는 `shunt login antigravity`의 `~/.shunt/antigravity-auth.json`을 재사용하며 `kind = "antigravity"`에서만 유효하고, `google_oauth`와 **호환되지 않습니다** — Antigravity는 Gemini CLI 토큰에 없는 두 스코프(`cclog`, `experimentsandconfigs`)를 요청합니다; `none`은 인증할 업스트림이 없는 어댑터(`kind = "antigravity_cli"`)를 위해 크리덴셜을 전혀 보내지 않습니다. |
| `api_key_env` | env 변수 이름 | `auth = "api_key"`일 때 키를 읽어오는 곳. 이 값 자신도 `${VAR}` / `${file:...}`로 쓸 수 있음([Secret 참조](#secret-참조) 참고). |
| `api_key_header` | `bearer`(기본) \| `x_api_key` | 주입된 키가 전송되는 헤더. |
| `accounts` | 계정 테이블 배열 | Anthropic OAuth 계정 풀. `kind = "anthropic"`이고 `auth = "claude_oauth"`일 때만 유효; 아래 참고. |
| `effort` | `low` … `max` | 선택적 기본 추론 노력(`responses` 프로바이더). `kind = "antigravity"`에도 적용되며, 접미사가 없는 `gemini-*` `upstream_model`에 카탈로그의 effort 접미사로 붙습니다. |
| `count_tokens` | `tiktoken`(기본) \| `estimate` | `responses` 및 `cursor` provider: 로컬 tiktoken 카운트 대 `501 not_supported` fallback([상세](/ko/guides/effort-and-context/#토큰-카운팅-count_tokens)). |
| `classifier_model` | 모델 id | `anthropic` provider 전용. Claude Code 자동 모드 권한 분류기 요청이 사용할 업스트림 모델이며, 대상은 요청 모양만으로 식별합니다 — 나머지 요청은 모두 클라이언트가 요청한 모델을 그대로 씁니다. **이 provider 안에서의** 리맵이지 다른 provider로 가는 라우트가 아닙니다 — 이 키는 `anthropic` 업스트림에서만 받습니다. 기본값은 설정 안 함. [Anthropic → 자동 모드 분류기](/ko/providers/anthropic/#자동-모드-분류기) 참고. |
| `tool_search` | 미설정("auto", 기본) \| `true` \| `false` | gpt-5.4+ 모델이면서 계열이 xAI/Grok이 아닐 때 Claude Code의 도구 검색에 네이티브 클라이언트 실행 `tool_search` 프로토콜을 사용합니다. 미설정 시에는 이미 검증된 호스트 — ChatGPT/Codex 백엔드와 `api.openai.com` — 에서만 기본으로 네이티브를 사용하고, LiteLLM·vLLM·OpenRouter·자체 호스팅 프록시 등 그 외 모든 OpenAI 호환 엔드포인트는 텍스트 shim을 유지합니다. 검증된 커스텀 엔드포인트를 네이티브에 옵트인하려면 `true`로, shim을 항상 강제하려면 `false`로 설정하세요. [Codex → 도구 검색](/ko/guides/codex/#네이티브-프로토콜)을 참고하세요. |

이름만 있는 항목은 `shunt login claude --name <name> --mode <mode>`(`<mode>`는 `oauth`, `import`, `setup-token` 중 하나)로 만든 `~/.shunt/accounts/claude/<name>.json`을 읽습니다. 대화형 CLI는 이 세 mode를 묻고 갱신 가능한 OAuth를 권장합니다. `--long-lived`는 `--mode setup-token`의 deprecated alias입니다. `SHUNT_CLAUDE_ACCOUNTS_DIR`로 스토어 디렉터리를 재정의할 수 있습니다. `[[providers.<name>.accounts]]`에 명시적으로 나열된 계정 목록이 비어 있으면 스토어 디렉터리의 유효한 계정 파일을 모두 스캔합니다. 갱신 가능한 OAuth/import 파일은 provider가 refresh token을 회전할 때 제자리에서 갱신되므로 파일마다 활성 owner가 하나만 있어야 합니다. 실행 중인 여러 shunt 프로세스에서 파일을 공유하거나 독립적으로 복사하지 마세요. 프로세스마다 별도로 프로비저닝하거나, 적절한 경우 정적 setup token을 사용하세요.

## `[[routes]]`

레거시 exact-match 라우팅 항목 — 일치하는 `[models.upstream_model]` 항목 다음에 확인됩니다:

> **레거시:** 정확한 모델 id에는 `[[models]]` 항목과 `[models.upstream_model]`을 사용하는 편을 권장합니다. 하나의 원본에서 id를 라우팅하는 동시에 노출할 수 있습니다. `[[routes]]`는 계속 지원되지만 더 이상 권장되는 exact routing 형식은 아닙니다.

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `model` | ✅ | Claude Code가 보내는 정확한 `model` id |
| `provider` | ✅ | 설정된 업스트림 이름 |
| `upstream_model` | — | 업스트림으로 전달되는 모델 id를 다시 씀 |
| `effort` | — | 라우트별 추론 노력 오버라이드. `antigravity` 라우트에서는 접미사가 없는 `gemini-*` `upstream_model`에 붙일 effort 접미사를 고정합니다. |

## `[[route_prefixes]]`

프리픽스로 일치하는 라우팅 항목 — 정확한 라우트 이후에 확인됩니다:

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `prefix` | ✅ | 모델 id 프리픽스, 예: `gpt-` |
| `provider` | ✅ | 설정된 업스트림 이름 |

## `[[models]]`

[모델 디스커버리](/ko/guides/model-discovery/)를 위해 `GET /v1/models`가 반환하는 항목. id는 반드시 `claude` 또는 `anthropic`으로 시작해야 하며, 그렇지 않으면 Claude Code가 무시합니다.

최상위 `auto_include_builtin_models` 키의 기본값은 `true`입니다. 활성화하면 shunt는 관리자가 선별한 `[[models]]` 항목을 먼저 반환한 뒤, 스스로 발견한 모델을 추가합니다. id가 정확히 같은 항목은 선별된 항목을 우선하여 중복을 제거합니다. `[[models]]` 목록만 노출하려면 `false`로 설정하세요 — 아래의 업스트림 호출도 함께 비활성화됩니다.

발견된 모델은 shunt가 실제 업스트림 목록을 가져올 수 있으면 거기서 옵니다. `server.default_provider`가 Anthropic 종류일 때 해당 업스트림에 `GET /v1/models`를 호출하며, 그 인증 방식에 맞는 크리덴셜을 사용합니다. `auth = "passthrough"`에서는 호출자가 전달한 크리덴셜을 사용하므로 호출자마다 해당 크리덴셜로 사용할 수 있는 목록을 보게 됩니다. 단, 어떤 슬롯에 실제 업스트림 크리덴셜이 아니라 shunt 자체의 `[server.gateway]` JWT나 설정된 `[server.auth]` 클라이언트 토큰이 담겨 있다면 그 슬롯은 전달되지 않습니다. `authorization`과 `x-api-key`는 각각 독립적으로 필터링되므로 다른 슬롯에 담긴 진짜 크리덴셜은 그대로 전달되며, 두 슬롯 모두 전달할 크리덴셜이 남지 않았을 때만 디스커버리가 내장 스냅샷으로 폴백합니다. `api_key`에서는 설정된 키를 사용합니다. `claude_oauth`에서는 추론 경로와 동일한 유효 계정 집합에서 가장 먼저 해석되는 비활성화되지 않은 계정을 사용합니다. 이 집합에는 계정 저장소에서 검색된 계정이 포함되며 `account_scope` 순서를 따릅니다. 디스커버리는 풀 선택, 쿨다운, 할당량 기록을 수행하지 않습니다. 따라서 게이트웨이 소유 크리덴셜을 사용하는 이 두 방식에서는 모든 호출자가 해당 크리덴셜 범위의 카탈로그를 공유합니다. shunt는 캐시하지 않습니다. `server.default_provider`가 Anthropic 종류가 아니거나, 크리덴셜이 없거나, 호출이 실패·타임아웃(2초 상한)하면 내장 Claude 카탈로그 스냅샷으로 폴백합니다. 어느 쪽이든 이 id들은 전용 `[[routes]]` 항목이 필요하지 않습니다. 일반 라우팅 규칙으로 해석되며, `[[routes]]`나 `[[route_prefixes]]` 어느 것에도 매칭되지 않을 때 `server.default_provider`로 폴백합니다.

선별한 항목에 `[models.upstream_model]`을 추가하면 하나의 선언으로 id를 노출하고, 라우팅하고, 업스트림 id로 변환할 수 있습니다. 정확한 id를 라우팅할 때는 `[[routes]]` 대신 이 형식을 권장합니다. 순서가 있는 `[[upstreams]]`를 사용하면 맵에 하나 이상의 `upstream = "backend-id"` 쌍을 넣을 수 있으며, `[[upstreams]]` 선언 순서에 따라 페일오버 체인이 됩니다. 레거시 `[providers.*]`에는 선언된 순서가 없으므로 정확히 한 쌍만 허용됩니다. 해당 id에 대해서는 맵이 `[[routes]]`, `[[route_prefixes]]`, `server.default_provider`보다 우선하며 각 업스트림의 기본 `effort`가 해당 체인 항목에 적용됩니다. 빈 맵, 비어 있거나 공백으로만 이루어진 업스트림 이름 또는 백엔드 id, 알 수 없는 업스트림, 같은 id의 `[[routes]]` 항목, `[1m]` 또는 `[1M]`으로 끝나는 맵 보유 id, 한쪽이라도 맵을 보유한 중복 `[[models]]` id는 시작 오류입니다. 클라이언트가 매칭 전에 context-window hint를 제거하므로 맵 보유 id에 이 suffix를 포함하면 해당 항목에 도달할 수 없습니다. 맵이 없는 항목끼리의 중복은 기존 동작을 유지하지만, 그중 하나가 `[models.router]` 테이블을 가진 경우는 예외입니다 — 아래를 참고하세요.

```toml
[[models]]
id = "claude-opus-4-8"
display_name = "Claude Opus 4.8"

[models.upstream_model]
codex = "gpt-5.2"
```

| 키 | 필수 | 의미 |
| :-- | :-- | :-- |
| `id` | ✅ | Claude Code에 노출되는 모델 id |
| `display_name` | — | `/model` 선택기에 표시되는 레이블 |
| `upstream_model` | — | 설정된 업스트림 이름에서 백엔드 모델 id로 이어지는 맵. 순서 있는 `[[upstreams]]`는 여러 항목의 페일오버 체인을 허용하고, 레거시 provider는 한 항목만 허용 |

### `[models.router]` (선택)

광고하는 id 하나에 대한 요청 단위 라우팅입니다. 목적지를 하나만 지정하는 대신
`[models.router]` 테이블을 두고, 그 `type` 키가 라우팅 알고리즘을 고르면 알고리즘이
목적지를 고릅니다. 이 테이블과 [`[models.subagents]`](#modelssubagents-선택) 오버레이가 모두 없으면
`[[models]]` 항목은 이전과 똑같이 동작하며, 어디에도 둘 다 설정하지 않으면
라우팅은 바뀌지 않습니다.

shunt가 보통 쓰는 `kind`나 `mode`가 아니라 `type`을 쓰는 것은 **shunt 자체 명명 규칙에
대한 의도적인 예외**이며, 레퍼런스에서 이 점을 밝히는 곳은 여기 한 곳뿐입니다. 라우팅
알고리즘은 [NVIDIA-NeMo/Switchyard](https://github.com/NVIDIA-NeMo/Switchyard)에서
가져왔고, 키 이름을 그대로 두면 업스트림 스키마 문서와 `type` 값을 다시 옮겨 적지 않고
그대로 쓸 수 있습니다.

어떤 라우터가 지정하는 타깃이든 모두 평범한 공개 model id이므로 각각 일반 사다리를 따라
해석되고 페일오버 체인, 계정 풀, 어댑터, `effort`, `service_tier`를 그대로 유지합니다.
클라이언트에 보고되는 id는 요청한 id 그대로이고, 선택된 타깃은 업스트림으로만 전달됩니다.

| `type` | 선택 기준 | 요청 본문 읽기 |
| :-- | :-- | :-- |
| `stage_router` | 최근 tool-result 메타데이터로 턴마다 | 읽음 — `tool_use.name`과 `tool_result.is_error`만 |
| `auto` | 같은 라우터를 업스트림 프리셋으로 | 위와 같음 |
| `random` | 가중치 추첨, 기본은 세션 고정 | 읽지 않음 |
| `noop` | 고르지 않음 — 빈 메시지로 응답 | 읽지 않음 |
| `prefill_router` | 가장 최근 사용자 턴을 읽는 학습형 분류기(`prefill-router` 빌드 필요) | 읽음 — 사용자 턴의 텍스트 |

판정에 LLM을 호출하는 알고리즘(`llm_classifier`, `composite`, `advisor`)과
[`[models.subagents]`](#modelssubagents-선택) 오버레이의 `llm_classifier` 형태는
**아직 사용할 수 없습니다**. 이들을 지정하면 시작 오류가 나며, 이후 릴리스에서
추가됩니다. `prefill_router`는 구현되어 있지만 **컴파일 타임에 게이트됩니다**.
기본으로 꺼져 있는 `prefill-router` 카고 피처를 켜고 빌드한 바이너리에서만 쓸 수
있습니다 — [아래](#type--prefill_router)를 보세요.
판정 모델을 쓰는 형태 가운데 지금 제공되는 것은 스테이지 라우터 자신의
[`[models.router.classifier]`](#modelsrouterclassifier-선택) 폴백 하나뿐입니다.

한 항목에 `[models.router]`와 `[models.upstream_model]`을 함께 선언할 수 없습니다.

#### `type = "stage_router"`

콘텐츠 인지 티어 선택입니다. 항목이 **둘**(강한 티어와 효율 티어)을 지정하고, 요청의 최근
tool-result 이력이 턴마다 둘 중 하나를 고르게 합니다. 신호와 히스테리시스가 어떻게
동작하는지는 [스테이지 라우터 가이드](/ko/guides/stage-router/)를 참고하세요.

```toml
[[models]]
id = "claude-auto"
display_name = "Auto (stage router)"

[models.router]
type = "stage_router"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `type` | ✅ 필수 | `stage_router` |
| `capable_target` | ✅ 필수 | 어려운 추론·조사·오류 복구를 맡는 모델 id |
| `efficient_target` | ✅ 필수 | 계획이 정해진 뒤 정형 작업을 맡는 모델 id |
| `picker` | `efficient_first` | 신호가 불확실할 때 사용할 티어. `efficient_first` 또는 `capable_first` |
| `confidence_threshold` | `0.5` | 신호에 따라 결정하기 위한 최소 스코어러 확신도, `(0.0, 1.0]` 범위 |
| `recent_turn_window` | `3` | 스코어러에 전달하는 어시스턴트 툴 결과 턴 수. 최소 `1` |
| `min_dwell_turns` | `3` | 하향 전환이 가능해지기까지 티어를 유지하는 턴 수. 티어를 고른 턴부터 세므로 `0`과 `1`은 모두 하한 없음을 뜻합니다 |
| `deescalate_threshold` | `0.75` | 티어를 *내릴* 때 필요한 확신도. 기본값이 `confidence_threshold` 기본값보다 높아 내려가는 쪽이 더 어렵지만, 두 값은 각각 독립적으로 범위 검사되므로 `confidence_threshold`보다 낮은 값도 허용되며, 로드 시점에 경고가 남습니다 |
| `session_ttl_seconds` | `3600` | 조용한 세션의 고정 티어가 유지되는 시간 |
| `capable_hold_turns` | `0` | 신호로 상향 전환한 뒤 강한 티어를 유지하는 턴 수. 그 턴들은 라우트 소스 `capable_hold`로 보고되며, 유지는 증거가 아니어서 고정된 티어를 어느 방향으로도 옮기지 못합니다. 기본값 `0`은 고정 동작을 기존 그대로 두며, 업스트림 기본값은 `2`입니다 |

#### `[models.router.tool_semantics]` (선택)

라우터 하나에 대해 shunt 내장 Claude Code 툴 어휘를 넓히는 네 개의 목록입니다. 내장
테이블을 대체하는 것이 아니라 그 **뒤에** 적용되므로, 내장 테이블이 분류하지 않고 남겨 둔
이름 — `Bash`, `Skill`, `mcp__*` 서버 툴 — 에만 닿습니다. 내장 테이블이 이미 observe,
mutate, plan으로 분류한 이름(`Read`, `Edit`, `TodoWrite` 등)을 지정하면 **시작 오류**입니다.
공백이 섞인 이름(`" Read "`, `"some tool"`)도 마찬가지입니다. 이름은 정확히 일치해야 하므로
앞뒤에 공백이 붙은 이름은 런타임에 아무것도 매칭하지 못합니다.

```toml
[models.router.tool_semantics]
observe = ["mcp__jbcontext__code_search"]
mutate = []
plan = []
new = []
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `observe` | `[]` | 아무것도 바꾸지 않고 읽기만 하는 툴 이름 |
| `mutate` | `[]` | 상태를 바꾸는 툴 이름. 파일 전체 쓰기로 채점됩니다 |
| `plan` | `[]` | 계획하거나 위임하는 툴 이름 |
| `new` | `[]` | 스코어러가 새로 도입된 툴로 세는 이름 |

넷 중 하나라도 고치면 다음 로드에서 그 라우터의 기존 세션 고정이 사라집니다. 문턱값을
고칠 때와 같습니다.

#### `[models.router.handoff_notes]` (선택)

신호가 티어를 옮긴 턴에 한해, **업스트림으로 보내는** 요청에 시스템 블록 하나를 덧붙여
새로 들어온 모델에게 왜 넘겨받았는지 알려 줍니다.

```toml
[models.router.handoff_notes]
escalation_note = "the previous model was stalling; pick up the diagnosis"
deescalation_note = "routine work resumes"
only_on_wrong_signal_escalation = true
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `escalation_note` | — | 신호가 턴을 강한 티어로 올릴 때 덧붙입니다 |
| `deescalation_note` | — | 스코어러가 작업을 효율 티어로 되돌릴 때 덧붙입니다 |
| `only_on_wrong_signal_escalation` | `true` | `escalation_note`를 신호가 주도한 상향 전환(라우트 소스 `override`, `dimensions`)으로 한정합니다. `false`로 두면 스코어러가 내린 모든 상향 전환에 덧붙입니다 |

노트는 `system` 배열의 **맨 뒤**에 새 블록으로 들어갑니다. Claude Code의 attribution
블록은 첫 번째 원소이며 건드리지 않습니다. 고정 상태로 이어진 턴, 신호가 없는 턴,
`count_tokens` 프로브에는 노트가 붙지 않습니다. 넘겨준 것이 없는 턴도 마찬가지입니다 —
세션의 첫 턴, 그리고 이미 고정된 티어를 다시 확인하기만 한 턴이 여기 해당합니다.
빈 노트는 시작 오류입니다.

**전환할 때마다 프롬프트 캐시 미스를 한 번씩 치릅니다.** `system` 배열은 캐시된 프리픽스의
일부여서, 노트를 붙이거나 떼면 프리픽스가 무효가 됩니다. 티어 전환 자체가 이미 포기하는
모델별 프리픽스에 더해지는 비용입니다. 이 테이블이 옵트인인 이유이자,
`only_on_wrong_signal_escalation`의 기본값이 더 좁은 쪽인 이유입니다.

#### `[models.router.classifier]` (선택)

신호만으로 판정할 수 없는 턴을 위한 LLM **판정 모델**입니다. 이 테이블을 두면 해당 항목은
판정 호출을 하는 레인으로 옮겨 갑니다. 스코어러가 결론을 내지 못한 턴 — 원래라면 라우트
소스 `fall_open`으로 보고될 턴 — 가운데 고정(pin)이 잡고 있지 않은 세션의 턴에서 shunt는
판정 모델에게 묻고 그 판정으로 라우팅합니다. `type = "stage_router"`에서만 받습니다.

```toml
[models.router.classifier]
target = "claude-haiku-4-5"
base_threshold = 0.5
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `target` | ✅ 필수 | 판정 모델의 공개 model id. 물어보기만 하고 클라이언트에게는 제공하지 않습니다 |
| `base_threshold` | `0.5` | 지원되는 작업을 효율 티어에 두는 `p_solve` 하한. `(0.0, 1.0]` 범위 |

판정 타깃도 평범한 공개 model id이며 티어 타깃과 똑같이 한 홉 규칙을 지킵니다. 여기에
조건이 하나 더 붙습니다. **passthrough** 라우트로 해석되면 안 됩니다.
`auth = "passthrough"`는 *호출자의 자격 증명을 그대로 전달한다*는 뜻인데, 판정 호출이
제거하는 것이 바로 그 호출자의 자격 증명입니다. 그래서 그런 타깃은 아무것도 없이 도착하고
시작 오류가 됩니다. 나머지 auth 모드는 모두 허용되며 `auth = "none"`도 포함됩니다.
이 모드는 해당 엔드포인트가 자격 증명을 전혀 요구하지 않는다는 뜻이므로, 인증 없이 도는
로컬·자체 호스팅 판정 모델은 오류가 아니라 지원되는 구성입니다.
호출자의 자격 증명 슬롯은 하나도 함께 가지 않습니다 — 예약된 `x-shunt-*` 슬롯과 `cookie`,
`authorization`, `x-api-key`, `anthropic-beta`가 모두 제거됩니다. 호출은 그 타깃 자신의
계정 풀 쿼터를 씁니다. 판정 모델에 자기 `[[models]]` 항목을 따로 두라고 하는 이유입니다.

판정이 내려진 턴은 라우트 소스 `llm-classifier`로 보고되고 다른 결정과 똑같이 세션을
고정합니다. 판정 실패는 종류를 가리지 않고 — 타임아웃, 응답 크기 초과, 업스트림 오류,
해석할 수 없는 판정, 예산 소진 — picker 기본값인 `fall_open`으로 해결됩니다.
`count_tokens` 프로브에는 판정 모델을 부르지 않으며, 요청이 인증과 정책 검사를 통과하기
전에도 부르지 않습니다. 판정 모델을 부르는 턴에서는 인바운드 인증이 요청된 id에 더해 이
항목이 지정할 수 있는 모든 타깃과 판정 모델을, 각각의 페일오버 체인 전체까지 포함해
대상으로 삼습니다. 그래서 passthrough 응답 타깃에 자격 증명을 주입하는 판정 모델이 붙으면
클라이언트 자격 증명이 필요해지고, 인증에 실패했거나 정책이 거부한 요청은 판정 호출을 한
번도 만들지 않습니다. 판정 모델을 부르지 않는 턴은 실제로 해석된 체인으로만 인증합니다 —
신호가 스스로 결정한 턴과,
[`[models.subagents]`](#modelssubagents-선택) 오버레이가 라우터보다 먼저 돌려보낸
턴입니다.

#### 호출당 한도

`[models.router]`의 여섯 개 키가 이 항목이 만드는 모든 내부 호출에 한도를 겁니다. 한도를
넘기면 업스트림 호출을 취소합니다. 각 값은 최소 `1`이어야 하며, `0`은 해당 키를 알려 주는
시작 오류입니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `judge_timeout_ms` | `30000` | 스트리밍하지 않는 판정 호출의 종단 간 기한. 헤더*와* 본문을 모두 덮으므로 `200`을 보낸 뒤 멈춰 버린 응답도 여기서 끊깁니다 |
| `judge_max_response_bytes` | `65536` | 수집하는 판정 응답의 최대 크기. 이를 넘기면 `fall_open`으로 해결됩니다 |
| `gated_max_bytes` | `8388608` | 보관하는 턴의 최대 크기 |
| `gated_idle_ms` | `60000` | 보관하는 턴에서 완성된 콘텐츠 프레임 사이의 최대 간격. SSE ping 프레임은 이 타이머를 되돌리지 않으며, 청크 경계에서 나뉜 프레임은 분류 전에 다시 합쳐집니다 |
| `gated_max_duration_ms` | `600000` | 보관하는 턴의 벽시계 상한 |
| `max_judge_calls` | `8` | 한 세션이 만들 수 있는 판정 호출 수 |

`gated_*` 세 키는 값을 받고 검증하며 보관 수집기가 실제로 적용하지만, **아직 보관되는 턴
자체가 없습니다** — 이 키들이 대비하는 버퍼링된 상향 전환 턴과 advisor 턴은 이후
릴리스에서 추가됩니다. 그때까지 런타임에서 이 세 키가 거는 한도는 없습니다.

#### `type = "auto"`

업스트림의 스테이지 라우터 프리셋입니다. `picker = "efficient_first"`와
`confidence_threshold = 0.5`를 쓰고 나머지 스테이지 키는 모두 shunt 기본값입니다. `type`
외에는 두 타깃만 받으므로, 다른 스테이지 키를 지정하려면 `type = "stage_router"`를 쓰세요.
여기에는 `[models.router.classifier]`와 호출당 한도 키도 포함됩니다 — 프리셋에는 판정
모델이 없으므로 `auto` 항목에 classifier 테이블을 두면 시작 오류입니다.

```toml
[[models]]
id = "claude-quick"

[models.router]
type = "auto"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `type` | ✅ 필수 | `auto` |
| `capable_target` | ✅ 필수 | `stage_router`와 같습니다 |
| `efficient_target` | ✅ 필수 | `stage_router`와 같습니다 |

#### `type = "random"`

타깃 둘 이상에 가중치를 둬 트래픽을 나눕니다. 카나리 배포용입니다. 기본값에서는 Claude
Code 세션 하나가 같은 갈래에 머무릅니다.

```toml
[[models]]
id = "claude-canary"

[models.router]
type = "random"
targets = ["claude-sonnet-4-6", "gpt-5.6-terra"]
weights = [9, 1]
# seed = 0
# affinity = "session"
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `type` | ✅ 필수 | `random` |
| `targets` | ✅ 필수 | 트래픽을 나눌 model id 목록 |
| `weights` | 균등 | 타깃마다 0 이상의 가중치 하나. `0`은 그 타깃을 비활성화합니다. 각 가중치는 유한해야 하며 합계도 유한해야 합니다 — 합이 무한대로 넘치는 목록은 로드 시 거부됩니다 |
| `seed` | `0` | `session` 친화도에서는 해시 솔트, `request` 친화도에서는 추첨 시드 |
| `affinity` | `session` | `session`은 한 세션을 한 갈래에 묶고, `request`는 요청마다 추첨합니다 |

`affinity = "session"`에서 갈래는 `sha256(seed ‖ model ‖ 세션 id)`를 가중치 범위로
환산한 값입니다. 저장하는 것이 없으므로 재시작해도 갈래가 유지되고, 같은 설정을 읽은
레플리카끼리도 동일합니다. 대신 `seed`, `targets`, `weights`를 바꾸면 갈래가 움직일 수
있습니다. `x-claude-code-session-id`를 보내지 않은 요청은 한 갈래를 공유하는 대신 그
요청만의 가중치 추첨을 새로 합니다. 세션을 보내지 않는 클라이언트에서도 90/10 분배가
90/10으로 유지됩니다. `affinity = "request"`에서는 요청마다 추첨하며, `seed`를 지정하면
추첨 순서를 재현할 수 있습니다.

세션 친화도는 **접근 제어가 아니라 고정(stickiness)입니다.** 세션 id는 클라이언트가
정하므로, id를 바꿔 가며 재시도하는 호출자는 원하는 갈래로 스스로를 몰아갈 수 있습니다.
그래도 막는 것은 없습니다 — 모든 타깃이 그 호출자가 이름으로 직접 요청할 수 있는 공개
model id이기 때문입니다. 접근 제어는 managed model 정책의 몫입니다.

요청 본문이 없는 표면 — `GET /routes`, `/v1/models` 디스커버리, `shunt check` — 은 가중치가
양수인 첫 번째 타깃을 보고합니다.

#### `type = "noop"`

업스트림을 전혀 호출하지 않고 응답합니다. 호출자가 쓴 모드 그대로 비어 있는 종료
어시스턴트 메시지를 만들어 줍니다. `stream: true`에는 올바른 SSE 시퀀스(`message_start`,
`stop_reason: "end_turn"`을 담은 `message_delta`, `message_stop`)를, 그렇지 않으면 Message
JSON 객체 하나를 보냅니다. `count_tokens`는 `input_tokens: 0`으로 답합니다. 이 라우트도
다른 라우트와 똑같이 인증을 거치므로 인증 구멍이 아닙니다 — 토큰을 전혀 쓰지 않고 인바운드
경로 전체를 확인하는 클라이언트 연결 점검용입니다.

```toml
[[models]]
id = "claude-noop"

[models.router]
type = "noop"
```

받는 키는 `type` 하나뿐입니다.

#### `type = "prefill_router"`

학습형 라우터입니다. 업스트림의 분류기가 가장 최근의 텍스트 사용자 턴을 채점해 항목의
타깃 중 하나를 고르며, 업스트림 판정 모델을 호출하는 대신 이 프로세스 안에서 모델을
돌립니다.

**이 타입만은 빌드를 가립니다.** `prefill_router`는 `prefill-router` 카고 피처를 켰을
때만 컴파일에 들어가고, 이 피처는 **기본으로 꺼져 있으며** 릴리스 워크플로가 켜는 일도
없습니다. 그래서 릴리스 바이너리에도 Homebrew 설치본에도 들어 있지 않습니다. 소스에서
빌드하세요.

```sh
cargo build --release --features prefill-router          # 대시보드가 필요하면 ,ui 를 붙입니다
```

아래 설정은 어느 빌드에서나 파싱됩니다. 다른 것은
로드입니다. 피처가 없는 바이너리에서는 로드가 실패하므로, 설정한 알고리즘 없이 게이트웨이가
떠 버리는 대신 `shunt check`가 이를 보고합니다. 이 피처 오류는 해당 테이블에 대한 다른 모든
지적보다 먼저 보고되므로, 키 단위 규칙(빈 타깃, 비어 있는 `checkpoint`, 0 이하의
`max_length`·`batch_size`)은 피처를 켠 빌드가 보고하는 것입니다.

```text
models entry <id> router type = "prefill_router" is not compiled into this binary: it needs the `prefill-router` cargo feature, which is off by default and absent from release binaries; build from source with `cargo build --features prefill-router` (docs/routing-algorithms.md)
```

```toml
[[models]]
id = "claude-learned"

[models.router]
type = "prefill_router"
targets = ["claude-sonnet-4-6", "claude-opus-4-8"]
checkpoint = "/models/router.pt"
# device = "cpu"
# cache_dir = "/var/cache/huggingface"
# max_length = 2048
# batch_size = 32
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `type` | ✅ 필수 | `prefill_router` |
| `targets` | ✅ 필수 | 고를 대상 model id 목록. 체크포인트의 헤드 순서대로 적습니다 |
| `checkpoint` | ✅ 필수 | 텐서만 담긴 라우터 체크포인트 경로. 상대 경로는 프로세스의 작업 디렉터리를 기준으로 해석됩니다 |
| `device` | 자동 감지 | 라우터를 돌릴 torch 디바이스 — `cpu`, `cuda`, `cuda:0` |
| `cache_dir` | — | 인코더와 토크나이저를 위한 Hugging Face 캐시 디렉터리 |
| `max_length` | `2048` | 인코더 입력의 최대 토큰 길이. 그보다 긴 입력은 잘립니다. `0`보다 커야 하며, 지정하지 않으면 업스트림 기본값을 그대로 씁니다 |
| `batch_size` | `32` | 인코더 forward 한 번에 넣는 프롬프트 최대 개수. `0`보다 커야 하며, 지정하지 않으면 업스트림 기본값을 그대로 씁니다 |

**운영자가 직접 준비해야 하는 것.** 이 피처는 PyO3로 Python을 임베드하므로 빌드가
libpython을 링크하고, 실행 중인 게이트웨이는 임베드한 인터프리터에서 `torch`,
`transformers`, `numpy`, `accelerate`를 import할 수 있어야 합니다. 빌드할 때
`PYO3_PYTHON`을 그 인터프리터(3.10 이상, 공유 libpython 포함)로 지정하세요. 지정하지 않으면
PyO3가 `PATH`에서 처음 찾은 `python3`를 씁니다. 라우터 체크포인트도 필요합니다. Switchyard
v0.3.0은 체크포인트도, 익스포터도, 인코더 애셋도 제공하지 않으므로 호환되는 체크포인트를
구하거나 학습시키는 일은 운영자의 몫입니다. 둘 중 하나라도 없으면 게이트웨이는 기동을
거부하고, 핫 리로드에서 같은 문제를 만나면 리로드를 거부해 돌아가던 설정을 그대로 둡니다.

```text
models entry <id> router type = "prefill_router" failed to load: <upstream error>
```

리로드는 라우터를 다시 만들고 세션별 친화도는 라우터 안에 있으므로, 리로드하면 각 세션이
어느 타깃에 있었는지 잊습니다.

**턴을 어떻게 결정하는가.** 알고리즘에 넘기는 것은 `user`와 `assistant` 역할, 그리고
`text`와 `tool_result` 블록뿐입니다. 알고리즘은 가장 최근의 텍스트 사용자 턴을 채점하고,
블록이 전부 `tool_result`인 메시지는 새 사람의 턴이 아니라 툴 연속으로 봅니다. 세션 식별은
`x-claude-code-session-id`에서, 위임된 자식이면 `x-claude-code-agent-id`도 함께 읽습니다.
그래서 연속 요청은 추론을 다시 돌리지 않고 그 턴의 결정을 재사용합니다. 둘 다 보내지 않는
호출자는 업스트림 규칙대로 첫 사용자 메시지의 해시로 돌아갑니다. 추론은 블로킹 워커에서
항목당 한 번에 하나씩 실행됩니다. `count_tokens` 프로브도 같은 방식으로 결정되며, 연속
요청일 때는 추론이 아니라 친화도 적중입니다.

`x-gateway-route-source`와 `shunt.router.decisions{algorithm="prefill_router"}`의 `source`
라벨은 셋 중 무엇이 일어났는지 알려 줍니다.

| 소스 | 의미 |
| :-- | :-- |
| `prefill` | 라우터가 턴을 결정함 — 추론이거나 세션 친화도 적중 |
| `prefill_fail_open` | 라우팅 호출이 실패해 기본 타깃으로 보냄. 업스트림 규칙대로 `targets`의 첫 항목입니다 |
| `prefill_default` | 요청 본문이 없는 표면 — `/v1/models` 디스커버리, `GET /routes`, 모델 해석 — 은 채점할 턴이 없으므로 첫 번째 타깃을 보고합니다 |

`GET /routes`는 해당 항목을 `algorithm: "prefill_router"`와 타깃 목록으로 보여 줍니다.
`shunt.stage_router.*` 메트릭은 시그널 전용 라우터의 것으로 남고 prefill 행은 생기지
않습니다.

#### 검증

타깃이 그 자체로 라우터인 경우, 빈 타깃, `(0.0, 1.0]`을 벗어난 문턱값,
`recent_turn_window`가 `0`인 경우, 라우터 **id**가 `[1m]` 또는 `[1M]`으로 끝나는 경우, 어느 한쪽이
라우터 테이블을 가진 중복 `[[models]]` id, 이 빌드가 구현하지 않은 `type`, 같은 항목이
`[models.upstream_model]`도 선언한 경우는 시작 오류입니다. `prefill_router` 항목에서는 빈 `targets`,
같은 타깃을 두 번 적은 경우, 빈 `checkpoint`, `0`인 `max_length`나 `batch_size`도 시작
오류입니다. 이 검사들은 피처를 **켠** 빌드가 보고하는 것입니다. 피처가 없는 빌드는 그 검사에
닿기 전에 빠진 cargo 피처를 알리며 항목을 거부하기 때문입니다. 중복 타깃은 다른 모든 타깃
비교와 마찬가지로 끝의 `[1m]`/`[1M]` 힌트를 제거한 뒤 비교합니다. 맵이 없는 두 항목은 원래 같은
id를 공유할 수 있지만, 라우터는 디스커버리 메타데이터가 아니라 라우팅 정책을 지정하므로
중복되면 하나의 id에 두 정책이 남습니다. 타깃 id는 라우팅이 매칭하는 방식과 동일하게 끝의
`[1m]` 또는 `[1M]` 힌트를 제거한 뒤 비교합니다. 그래서 자기 라우터를 가진 항목으로 해석되는
타깃은 두 `type`이 무엇이든 거부되며, 이것이 해석을 한 홉으로 묶어 둡니다.
[`[models.router.classifier]`](#modelsrouterclassifier-선택) 타깃도 같은 한 홉 규칙을
따르며 검사가 두 가지 더 붙습니다. `classifier.base_threshold`는
`confidence_threshold`와 똑같이 범위를 검사하고, 판정 타깃은 passthrough 라우트로
해석되면 안 됩니다 — 실효 체인에 passthrough 업스트림이 들어 있는 타깃은 판정 호출에서
호출자의 자격 증명이 제거된 뒤 실행할 것이 남지 않으므로 시작 오류입니다.
`auth = "none"`은 허용됩니다. 자격 증명을 요구하지 않는 엔드포인트와 자격 증명이 사라진
엔드포인트는 다릅니다. [호출당 한도](#호출당-한도) 여섯 개 중
하나라도 `0`이면 해당 키를 알려 주는 시작 오류입니다. 다음
네 가지는 로드를 실패시키지 않고 경고만 냅니다. 각각 운영자가 의도했을 법한 설정이기
때문입니다 — 명시적 라우트와 매칭되지 않는 타깃(매칭되지 않는 다른 id와 마찬가지로
`server.default_provider`로 해석되며, 이제 모든 라우터 `type`에 적용됩니다), 같은 id로
해석되는 `capable_target`과 `efficient_target`(두 티어를 의도적으로 한 모델로 합친 경우),
`confidence_threshold`보다 낮은 `deescalate_threshold`(비용을 우선하는 배포가 원할 수 있는,
내려가는 쪽을 더 쉽게 만든 설정), 그리고 라우터 자신의 id를 지정한 `[[routes]]` 항목(해당
id의 목적지는 라우터가 정하므로 조회되지 않습니다). id가 그저 그 접두사로 시작할 뿐인
`[[route_prefixes]]` 항목은 경고하지 **않습니다** — 그 접두사에 해당하는 다른 id는 여전히
처리하기 때문입니다. 각 경고는 로드할 때마다 한 번씩 나오며, 핫 리로드도 로드이므로 설정을
고치지 않으면 리로드할 때마다 다시 나옵니다.

### `[models.subagents]` (선택)

어떤 `[[models]]` 항목에도 붙일 수 있는 **위임된 작업** 전용 오버레이입니다 —
`[models.upstream_model]` 맵을 가진 항목, `[models.router]` 테이블을 가진 항목, 맵이 없어
`[[routes]]`로 해석되는 id 모두입니다. 그 id를 요청하는 `Task` 서브에이전트, 훅 에이전트,
워크플로 서브에이전트는 오버레이의 타깃으로 우회됩니다. 부모 세션 자신의 턴은 이 테이블을
전혀 보지 않으며, 오버레이가 없을 때와 똑같이 항목을 해석합니다. 이 테이블이 `router` 안이
아니라 항목 위에 놓이는 것은 고정 항목에는 라우터 테이블이 없고, Switchyard의 "subagents를
곁들인 passthrough"가 여기서는 바로 그 고정 항목이기 때문입니다.

```toml
[[models]]
id = "claude-opus-4-8"

[models.upstream_model]
anthropic = "claude-opus-4-8"

[models.subagents]
type = "passthrough"
target = "claude-haiku-4-5"
by_type = { Explore = "claude-haiku-4-5", fork = "claude-sonnet-4-6", teammate = "claude-sonnet-4-6" }
```

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `type` | ✅ 필수 | 이 릴리스가 구현하는 유일한 형태인 `passthrough`. 판정자가 자식의 티어를 고르는 `llm_classifier` 형태는 이후 릴리스이며, 지정하면 시작 오류입니다 |
| `target` | ✅ 필수 | `by_type`이 해당 에이전트 타입에 아무것도 지정하지 않았을 때 위임 턴이 가는 model id — 에이전트 타입 헤더가 전송되지 않으면 모든 위임 턴이 여기로 갑니다 |
| `by_type` | `{}` | 에이전트 타입 → model id. `x-claude-code-agent-type`의 값 그대로를 키로 씁니다 |

**무엇이 위임된 작업인가.** `x-claude-code-request-class`가 `subagent` 또는 `workflow`인
요청, 그리고 그 헤더가 없을 때는 비어 있지 않은 `x-claude-code-agent-id`를 실은 요청입니다 —
이 헤더는 힌트 게이트와 무관하게 Claude Code가 모든 위임 턴에 보냅니다. 클래스가 전송되면
그것이 결정권을 갖습니다. 에이전트 id가 붙은 `main`은 메인 트래픽이고, `compaction`과
`auxiliary`는 하네스 유지보수입니다 — 이 셋은 어느 것도 오버레이를 타지 않습니다. 따라서
클래스와 타입 헤더가 게이트로 꺼져 있는 기본 배포에서는 모든 `Task` 자식이 `target`으로
갑니다. `by_type`을 쓰려면 클라이언트가 `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`을 설정해야
합니다.

**`by_type` 키는** 대소문자까지 포함해 정확히 매칭됩니다. 내장 에이전트의 id는 그대로
전달됩니다 — `Explore`, `Plan`, `general-purpose`, `claude`, 그리고 클라이언트가
`CLAUDE_CODE_FORK_SUBAGENT=1`에서만 제공하는 `fork`입니다. `.claude/agents/`의 프로젝트
에이전트는 `custom`으로 도착하며 자기 이름은 전송되지 않으므로, `custom`이 그 에이전트가
매칭할 수 있는 유일한 키입니다. `teammate`는 Agent Teams 멤버를 가리키는 클라이언트의
리터럴이며 아직 와이어에서 관측된 적은 없습니다. 비어 있거나 공백이 섞인 키는 시작
오류입니다. 절대 매칭될 수 없기 때문입니다.

**타깃은** 라우터 타깃과 동일한 한 홉 규칙을 따르는 평범한 공개 model id입니다. `target`과
모든 `by_type` 값은 끝의 `[1m]`/`[1M]` 힌트를 제거한 뒤 자기 `[models.router]`나
`[models.subagents]` 테이블을 가진 항목으로 해석되어서는 안 되며, 라우터 타깃도 이 오버레이를
가진 항목으로 해석될 수 없습니다. 빈 타깃, `[1m]` 또는 `[1M]`으로 끝나는 오버레이 보유 id,
어느 한쪽이 이 테이블을 가진 중복 `[[models]]` id는 시작 오류입니다. 명시적 라우트와
매칭되지 않는 타깃은 라우터 타깃과 마찬가지로 로드 시점에 경고만 내고, 여전히
`server.default_provider`로 해석됩니다.

**상태 없음.** 타깃은 오직 설정과 요청 헤더만의 함수입니다 — 세션 핀도, 저장소도, 판정자
호출도 없습니다. 라우터가 달린 id에서는 라우터가 돌기 전에 자식이 우회되므로, 자식의 턴은
트랜스크립트를 상대로 채점되는 일이 없고 부모의 핀에도 닿지 않습니다. 우회된 턴은
`x-gateway-routed-model`(타깃)과 `x-gateway-route-source`를 실어 보내며 — `by_type`이
맞으면 `subagent_type`, `target` 폴백이면 `subagent` — `algorithm = "subagents"`로
`shunt.router.decisions`에 집계됩니다. 요청이 없는 표면은 아무것도 해석하지 않습니다.
`/v1/models` 디스커버리와 `shunt check`는 모델별 목적지 정보를 전혀 싣지 않고,
`GET /routes`는 부모 자신의 `[[routes]]`/`[models.router]` 항목이 있을 때만 그것을 보여
줍니다(`server.default_provider`에 맡겨진 id는 어느 배열에도 나오지 않습니다). 셋 중
무엇도 우회된 타깃을 해석하지 않으며, 오버레이 자체가 `routers` 배열에 실리는 일도
없습니다.

## `[sentry]` (선택)

자체 Sentry 프로젝트로의 옵트인 오류 리포팅. `dsn`을 설정하지 않으면 꺼짐이며, `[otel]`과 독립적입니다. 게이트웨이 자체 진단을 보고합니다 — 치명적인 게이트웨이 시작/서빙 오류, 패닉, `error` 레벨 로그 이벤트(`warn`/`info`는 브레드크럼, 메시지만 포함) — 여기에 더해 `dsn`이 설정되어 있으면 업스트림 제공자가 실패 응답을 반환할 때마다 무조건 오류/경고 이벤트를 보냅니다: 5xx 응답은 `error`, 429/529(레이트 리밋/과부하)는 `warning`이며, 각각 `model`, `provider`, `upstream_status`만 태그로 붙습니다. 요청/응답 본문, 헤더, 자격증명은 절대 전송되지 않습니다. 메트릭과 트레이싱은 각각 별도의 추가 옵트인입니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `dsn` | — | Sentry 프로젝트 DSN. 비우면 비활성화, 잘못된 DSN은 시작 오류. Redacting secret — 진단 출력에서 `[redacted]`로 표시됨([Secret 참조](#secret-참조) 참고). |
| `environment` | — | 보고되는 이벤트에 붙는 선택적 environment 태그 |
| `metrics` | `false` | 사용량 메트릭도 전송 — OpenTelemetry 가이드에 설명된 gateway 메트릭 계열(집계값만) |
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
| `metrics` | `true` | OpenTelemetry 가이드에 설명된 gateway 메트릭 계열 내보내기 |
| `logs` | `true` | `tracing` 로그 이벤트 내보내기(stderr 로그는 영향 없음) |
| `include_session_id` | `false` | 요청 스팬에 클라이언트 세션 id 첨부 |

## `[otel.headers]` (선택)

모든 OTLP 요청에 붙는 추가 헤더(예: 호스팅 컬렉터 토큰). 표준 `OTEL_EXPORTER_OTLP_HEADERS` 아래로 병합됩니다. 각 헤더 값은 redacting secret 타입으로 진단 출력에서 `[redacted]`로 표시됩니다([Secret 참조](#secret-참조) 참고).

| 키 | 의미 |
| :-- | :-- |
| 임의 | 헤더 이름 → 값, 예: `authorization = "Bearer <token>"` |

## 라우팅 우선순위

위임된 턴에서는 일치하는 `[models.subagents]` 오버레이 → 일치하는 `[models.router]` 항목 → 일치하는 `[models.upstream_model]` 항목 → 정확한 `[[routes]]` 일치 → `[[route_prefixes]]` 프리픽스 일치 → `server.default_provider`.

오버레이가 가장 앞에 오며, 위임된 작업에만 적용됩니다. 오버레이가 붙은 id로 온 `Task` 자식
요청은 그 항목의 라우터나 맵을 참조하기 전에 오버레이의 타깃으로 전환되고, 부모 자신의 턴과
`compaction`·`auxiliary` 턴은 그 테이블이 없는 것처럼 항목을 해석합니다. 아래 사다리는 그런
턴과 오버레이가 없는 모든 id가 해석해 내려가는 경로입니다.

라우터가 그다음에 오는 이유는 `[[models]]` 항목 자체에서 일치하기 때문입니다. 라우터가
붙은 id로 온 요청은 라우터가 응답하며, 라우터는 티어를 고른 뒤 **그 타깃**을 나머지 사다리로
해석합니다. 따라서 `[[routes]]` 항목이 지정해야 하는 것은 라우터 id가 아니라 타깃입니다.
라우터 id를 지정한 정확 일치 항목은 조회되지 않으며 로드 시점에 경고가 남습니다.
`[[route_prefixes]]` 항목은 영향을 받지 않습니다. 라우터는 그 접두사에서 자신의 id 하나만
가져갈 뿐입니다.
