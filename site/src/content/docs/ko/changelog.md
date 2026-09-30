---
title: 변경 이력
description: shunt의 모든 주요 변경 사항을 날짜와 함께, 호환성이 깨지는 변경은 표시해 정리합니다.
---

선별한 릴리스 노트이며 최신 순입니다. shunt는 1.0 이전이므로 마이너 버전 상승(`0.44` → `0.45`)에는
호환성이 깨지는 변경이 포함될 수 있습니다. 패치 버전 상승에는 대개 포함되지 않지만 예외가 있으므로,
**호환성 깨짐**으로 표시된 항목을 확인하세요. 아래의 호환성 관련 항목은 영향을 받는 대상, 해야 할 조치,
적용된 릴리스를 모두 명시합니다.

- **구독:** [GitHub 릴리스 피드](https://github.com/pleaseai/shunt/releases.atom)
- **전체 기록:** 모든 커밋에서 생성되는 [`CHANGELOG.md`](https://github.com/pleaseai/shunt/blob/main/CHANGELOG.md)
- **업그레이드:** [설치](/ko/getting-started/installation/)

이 페이지는 0.35.0부터를 다룹니다. 그 이전 릴리스는 생성된 변경 이력에만 있습니다.

## 0.45.1 — 2026-09-14

### `gpt-6-astra`에서 네이티브 도구 검색 활성화

**수정** — `gpt-6-astra`가 번역된 형태로 폴백하지 않고 업스트림 자체의 도구 검색 기능을 협상합니다.
[Effort와 컨텍스트](/ko/guides/effort-and-context/)를 참고하세요.

## 0.45.0 — 2026-09-14

### 관리자 JSON 및 변경 라우트가 `/admin/api/*`로 이동

**변경 · 호환성 깨짐, 0.45.0부터** — 관리자 API를 스크립트로 호출하는 모든 곳에 영향을 줍니다. 13개의 JSON
및 변경 라우트가 `/admin` 뒤에 `/api` 세그먼트를 하나 더 갖게 되었고, 기존 경로는 별칭으로 남기지 않고
제거했으므로 호출자를 모두 수정해야 합니다. `/admin`, `/admin/login`, `/admin/oidc/callback`은 그대로이며
전체 대조표는 [관리자 경로 마이그레이션](/ko/reference/endpoints/#관리자-경로-마이그레이션)에 있습니다.

### `GET /admin`이 리다이렉트 대신 SPA 셸을 반환

**변경 · 호환성 깨짐, 0.45.0부터** — `/admin/login`으로의 `303`을 "로그인되지 않음"으로 해석하는 스크립트에
영향을 줍니다. 인증되지 않은 `GET /admin`은 이제 내장 셸과 함께 `200`을 반환하며, 이 셸은 모든 방문자에게
동일한 정적 파일 하나로 운영자 데이터를 담지 않습니다. 리다이렉트는 사라진 것이 아니라 번들 안으로 옮겨가
`GET /admin/api/session`의 `401`을 따릅니다. 세션 여부는 그 부트스트랩 엔드포인트로 확인하세요.
[HTTP 엔드포인트](/ko/reference/endpoints/#관리자-spa-번들--features-ui)를 참고하세요.

### 읽기 키로 브라우저 세션 생성 가능

**변경 · 호환성 깨짐, 0.45.0부터** — "읽기 키는 브라우저 세션을 열 수 없다"를 폐기 속성으로 의존하던 배포에
영향을 줍니다. `POST /admin/login`은 `[server.admin] read_keys` 자격 증명에 대해 이전의 `401` 대신 읽기 등급
세션 쿠키와 함께 `303`을 반환합니다. 모든 변경 작업은 여전히 `403`이므로 쿠키가 키에 없던 권한을 주지는
않지만, 자체 수명을 가집니다. 브라우저 세션은 인메모리 저장소만으로 검증되므로 유출된 읽기 키를 회전하면
헤더 자격 증명은 다음 로드에서 멈추지만 쿠키는 `session_ttl_secs`(기본 1시간)가 지날 때까지 계속 읽습니다 —
리로드가 아니라 재시작으로 제거하세요.

### `--features ui` 뒤의 내장 관리자 SPA

**추가** — 관리자 대시보드는 이제 빌드 시점에 내장되어 `/admin`에서 제공되는 컴파일된 SPA 번들이며, 서버에서
렌더링하던 문자열 리터럴을 대체합니다. `--features ui` 없이 빌드한 바이너리는 셸 라우트에서 해당 피처를
명시하며 `404`를 반환합니다. [관리자 & 원격 프로비저닝](/ko/guides/admin-remote-provisioning/)을 참고하세요.

### 그레이스풀 셧다운 드레인의 상한과 설정

**추가** — 첫 SIGTERM/SIGINT 이후 진행 중인 HTTP/SSE/WebSocket 작업은 `shutdown_timeout_seconds`(기본 `30`,
범위 `1`–`3600`) 동안 드레인된 뒤 나머지가 취소됩니다. 변경하려면 재시작이 필요합니다.
[`[server]`](/ko/reference/configuration/#server)를 참고하세요.

### 빈 프레임 서비스 거부에 대한 `h2` 업데이트

**보안** — 연결에 빈 프레임을 쏟아부어 도달할 수 있는 서비스 거부를 해소하기 위해 `h2` 의존성을
업데이트했습니다. 0.45.0 이상으로 업그레이드하세요. 설정 변경은 필요하지 않습니다.

### 선택적 function 매개변수가 번역 과정에서 보존됨

**수정** — OpenAI 백엔드는 명시적 `strict`가 없는 function 도구를 strict 모드 쪽으로 정규화하며, 그 모드에서는
닫힌 매개변수 객체가 모든 속성을 필수처럼 다뤄 호출자가 설정하지 않은 값까지 모델이 채워 넣습니다. 이제
전달되는 function 도구에 `strict:false`를 고정해 선택적 속성을 선택적으로 유지합니다.
[문제 해결](/ko/reference/troubleshooting/)을 참고하세요.

### Codex 카탈로그 실패에 협상된 오류 형태 사용

**수정** — Codex 카탈로그에 대한 모델 디스커버리 실패는 이제 항상 Anthropic 형태가 아니라 인바운드
엔드포인트가 협상한 오류 형태를 반환하므로, OpenAI 프로토콜 클라이언트가 자체 오류 경로로 파싱합니다.
[모델 디스커버리](/ko/guides/model-discovery/)를 참고하세요.

### 관리자 로그인 및 프로비저닝 수정

**수정** — 로그인 페이지 CSP에서 `script-src`와 `connect-src`를 더 이상 보내지 않습니다. SPA 셸이 `/admin`뿐
아니라 `/admin/`에서도 제공됩니다. 대기 중인 하나의 로그인에 대한 동시 완료 요청이 직렬화됩니다. 거부된 시작이
인증 단계를 닫았음을 알립니다. 계정 추가 폼이 로그인을 잘못 보고하지 않습니다.

## 0.44.0 — 2026-09-09

### 인바운드 Responses 요청을 Anthropic 및 Chat Completions 업스트림으로 번역

**추가** — 인바운드 Codex 엔드포인트로 도착한 요청을 Anthropic 계열 또는 Chat Completions 업스트림으로 라우팅할
수 있으며, 번역은 양방향으로 처리됩니다.
[인바운드 Codex 엔드포인트](/ko/guides/inbound-codex-endpoint/)를 참고하세요.

### WebSocket 전송에서 인스트림 `codex.rate_limits` 기록

**수정** — WebSocket 전송이 인스트림 `codex.rate_limits` 이벤트를 기록하므로, 새 HTTP 턴에서만이 아니라 재사용된
연결에서도 쿼터 창이 최신으로 유지됩니다. 이는 [`GET /usage`](/ko/reference/endpoints/)에 반영됩니다.

### OpenAI 검증기가 거부하는 도구 스키마 정규식 제거

**수정** — OpenAI 백엔드는 도구 스키마의 모든 `pattern`을 Python `re`로 컴파일하므로 JavaScript 전용
정규식(`\p{Cc}`, `(?<name>…)`, `\u{…}`)을 거부합니다. 이제 전달하는 스키마에서 이런 패턴을 제거하며, strict 모드
밖에서 `pattern`은 권고 사항이라 힌트만 사라집니다. [문제 해결](/ko/reference/troubleshooting/)을 참고하세요.

## 0.43.0 — 2026-09-08

### `GET /usage`의 프로바이더별 분해

**추가** — 풀 전체 집계와 함께, `GET /usage`가 `providers` 아래에 풀링된 프로바이더별로 동일한 정제 수치를
제공하므로 한 프로바이더로 라우팅하는 클라이언트는 혼합 평균 대신 그 프로바이더의 여유를 읽습니다. 인증 방식이
풀링이 아닌 프로바이더는 생략됩니다. [HTTP 엔드포인트](/ko/reference/endpoints/)를 참고하세요.

### `GET /usage`가 최저 사용 계정이 아니라 풀 평균 여유를 보고

**변경** — 각 창은 이제 해당 창을 보고하는 비활성화되지 않은 계정들에 대한 `mean(1 - utilization)`, 즉 풀의 총
용량 중 아직 쓰이지 않은 비율을 보고하며, 최저 사용 계정 하나를 보고하지 않습니다. 기존 필드를 한 계정의
여유로 읽던 클라이언트는 이제 풀 전체 수치를 보게 됩니다.

## 0.42.0 — 2026-09-07

### 인바운드 Responses 엔드포인트의 모델 라우팅 서드파티 업스트림

**추가** — 인바운드 Responses 엔드포인트가 서드파티 업스트림으로의 모델별 라우팅을 따르므로, Codex CLI
클라이언트가 모델 id로 OpenAI 이외의 벤더에 도달할 수 있습니다.
[인바운드 Codex 엔드포인트](/ko/guides/inbound-codex-endpoint/)를 참고하세요.

## 0.41.3 — 2026-09-07

### `usagePercent`가 없는 Grok 제품이 쿼터 행을 비우지 않음

**수정** — `usagePercent` 없이 보고된 제품이 건너뛰어지지 않고 쿼터 행 전체를 비우던 문제를 고쳤습니다.
[xAI / Grok](/ko/guides/xai/)을 참고하세요.

## 0.41.2 — 2026-09-07

### 디바이스 페이지의 SSO 폼이 아이덴티티 프로바이더로 리다이렉트 가능

**수정** — 디바이스 페이지의 CSP `form-action`이 설정된 아이덴티티 프로바이더로의 리다이렉트를 거부해 해당
페이지에서 SSO를 완료할 수 없던 문제를 고쳤습니다. [게이트웨이 로그인](/ko/guides/gateway-login/)을 참고하세요.

## 0.41.1 — 2026-09-07

### `Origin`이 null이어도 디바이스 페이지 자체의 폼 POST를 수용

**수정** — `Referrer-Policy: no-referrer`에서는 같은 페이지의 폼 POST도 `Origin: null`로 도착하며 CSRF 가드가
이를 거부했습니다. 이제 가드는 `Sec-Fetch-Site: same-origin`을 먼저 판단합니다.

### 전달되지 못한 `agy` 핸드오프가 완료된 턴을 실패시키지 않음

**수정** — 이미 완료된 턴이 로컬 `agy` 서브프로세스로 전달되지 못한 핸드오프 때문에 실패하지 않습니다.
[Antigravity](/ko/providers/antigravity/)를 참고하세요.

## 0.41.0 — 2026-09-05

### Zhipu 및 MiniMax 중국 프리셋

**추가** — `zhipu`와 `minimax-cn`을 내장 Anthropic 호환 프리셋으로 제공하므로, 중국 본토 엔드포인트에 직접 작성한
프로바이더 테이블 없이 자격 증명만 있으면 됩니다. [Zhipu](/ko/providers/zhipu/)와
[MiniMax 중국](/ko/providers/minimax-cn/)을 참고하세요.

### 알 수 없는 모델에 대해 Antigravity effort 행렬 재탐색

**수정** — 캐시된 effort 행렬에 없는 모델을 지정한 턴이 실패하지 않고 재탐색을 유발합니다.
[Antigravity](/ko/providers/antigravity/)를 참고하세요.

### Cursor 컴포저 fast 모드와 내장 도구 호출

**수정** — 컴포저 fast 모드를 모델 id에 인코딩하지 않고 모델 메타데이터로 보내며, 내장 도구 호출이 포함된 턴을
버리지 않고 노출합니다. [Cursor](/ko/providers/cursor/)를 참고하세요.

## 0.40.2 — 2026-09-05

### `antigravity-cli`가 호출자 제공 도구를 무시하지 않고 거부

**변경 · 호환성 깨짐, 0.40.2부터** — `antigravity-cli` 프로바이더에 비어 있지 않은 `tools` 배열(`tool_choice`가
`none`인 경우는 제외)이나 `any`·`tool` 값의 `tool_choice`를 보내는 호출자에 영향을 줍니다. 이 프로바이더는 로컬 `agy` 바이너리를
실행하는데, `agy`는 자체적으로 도구 호출을 해결하며 `tool_use` 블록을 반환하지 않습니다. 그래서 이런 요청은
이전에는 도구를 조용히 무시한 텍스트 전용 `200`을 받았지만, 이제 `400 invalid_request_error`로 거부됩니다.
작업을 일반 프롬프트로 보내거나, 도구를 그대로 전달하는 네이티브 `antigravity` 또는 `gemini` 프로바이더로
해당 모델을 라우팅하세요. 도구 없이 `tool_choice`가 `auto`인 경우는 영향을 받지 않습니다.
[Antigravity](/ko/providers/antigravity/)를 참고하세요.

### `gpt-6-astra`를 위해 Codex 클라이언트 identity를 0.153.3으로 상향

**수정** — 백엔드가 `gpt-6-astra`를 제공하기 위해 요구하는 0.153.3으로 광고되는 Codex 클라이언트 identity를
올렸습니다. [ChatGPT / Codex](/ko/guides/codex/)를 참고하세요.

### 인스트림 `rate_limit_exceeded`를 429로 분류

**수정** — 인스트림 `rate_limit_exceeded` 이벤트를 429로 분류하고, misalignment steer를 버리지 않고 클라이언트로
전달합니다.

### Gemini 튜플 형식 배열 스키마

**수정** — Gemini가 요구하는 `items` 스키마를 백엔드가 거부하는 형태로 보내지 않고 튜플 형식 배열 정의에서
도출합니다.

## 0.40.1 — 2026-09-03

### Antigravity가 라이브 카탈로그에서 모델 id를 해석

**수정** — 모델 id를 계정의 라이브 카탈로그에 대해 해석하고, 프로덕션 호스트로 고정된 `base_url`을
리다이렉트합니다. 이 둘이 함께 프로바이더를 쓸 수 없게 만들던 가짜 429 "quota" 거부를 해소합니다. Antigravity의
카탈로그는 계정별로 다르고 예고 없이 바뀌므로 id를 더 이상 하드코딩하지 않습니다.
[Antigravity](/ko/providers/antigravity/)를 참고하세요.

## 0.40.0 — 2026-09-02

### `wham` 사용량 엔드포인트에서 Codex 계정 쿼터 수집

**추가** — `wham` 사용량 엔드포인트를 폴링해 Codex 계정 쿼터를 얻으므로, 트래픽이 `x-codex-*` 헤더를 반환할
때까지 기다리지 않고 풀 상태가 채워집니다. [Codex 멀티 계정](/ko/guides/codex-multi-account/)을 참고하세요.

### Antigravity가 에이전트 envelope로 daily 백엔드에 도달

**수정** — 요청이 daily 백엔드에 대해 전체 에이전트 envelope와 effort 접미사가 붙은 모델 id를 함께 보내며, 이는
서비스가 실제로 수용하는 조합입니다. [Antigravity](/ko/providers/antigravity/)를 참고하세요.

### 인접한 Gemini 사용자 턴 병합

**수정** — 인접한 사용자 턴을 병합해 대화 중간의 시스템 메시지를 넘어서도 도구 짝짓기가 유지됩니다.

## 0.39.2 — 2026-09-01

### 한 번도 선택되지 않은 계정도 재로그인 필요 판정 유지

**수정** — 어떤 프로바이더 테이블도 선택한 적 없는 계정이 `has_state: false`와 함께 `needs_relogin`을
보고합니다. 관리자 갱신 프로브가 저장소 이름으로 판정을 기록하기 때문입니다.
[HTTP 엔드포인트](/ko/reference/endpoints/)를 참고하세요.

## 0.39.1 — 2026-08-31

### Claude 계정 상태를 자격 증명 종류로 보고

**수정** — 계정 상태를 원시 만료 시각이 아니라 자격 증명 종류에서 도출하므로, 죽은 자격 증명이 단순 만료가
아니라 재로그인 필요로 보고됩니다. [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/)을 참고하세요.

## 0.39.0 — 2026-08-29

### 쿼터에 근접한 오래된 계정을 기회적으로 재탐침

**추가** — 쿼터에 근접해 일시 중지된 계정을 기회가 될 때 재탐침하므로, 고정된 쿨다운을 기다리지 않고 창이
리셋되는 즉시 로테이션에 복귀합니다. [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/)을 참고하세요.

### 풀 플랜을 계정 아이덴티티로 키잉

**수정** — 플랜을 계정 아이덴티티로 키잉하고 뒷받침하는 파일 읽기를 single-flight로 처리하므로, 동시 읽기가 한
계정의 플랜을 다른 계정에 귀속시키지 않습니다.

### 비 Anthropic Messages 모델에서 deferred 도구 제거

**수정** — deferred 도구 블록을 이해하지 못하는 비 Anthropic Messages 업스트림으로 전달하기 전에 제거합니다.
[모델 별칭](/ko/guides/model-aliases/)을 참고하세요.

### 리셋 없는 쿼터 표식의 수명 제한

**수정** — 리셋 타임스탬프 없이 도착한 쿼터 표식이 계정을 무기한 일시 중지하지 않고 스스로 만료합니다.

## 0.38.0 — 2026-08-25

### `kind = "antigravity"`가 네이티브 HTTP 업스트림

**변경 · 호환성 깨짐, 0.38.0부터** — `antigravity` 프로바이더가 있는 모든 설정에 영향을 줍니다. 이 이름은 이제
네이티브 HTTP 업스트림을 뜻하며, 로컬 `agy` 서브프로세스 전송은 `kind = "antigravity_cli"`(내장 프로바이더
`antigravity-cli`)로 옮겨졌습니다. 옛 의미를 그대로 담은 설정은 다른 대상으로 바뀌지 않고 이름으로 거부되며,
자격 증명이 없는 라우팅된 `antigravity` 프로바이더는 기동을 거부합니다. 서브프로세스 전송을 유지하려면 테이블
이름을 `antigravity_cli`로 바꾸고, HTTP 전송을 채택하려면 자격 증명을 추가하세요 —
[Antigravity](/ko/providers/antigravity/)를 참고하세요.

### `kind = "antigravity_cli"` 지원 중단

**지원 중단** — 로컬 `agy` 서브프로세스 전송은 네이티브 HTTP 업스트림(`kind = "antigravity"`)에 밀려 지원이
중단됩니다. 아직 동작하므로 편한 시점에 마이그레이션하세요. [Antigravity](/ko/providers/antigravity/)를
참고하세요.

### shunt 자격 증명을 담은 공유 슬롯 전체 제거

**변경 · 호환성 깨짐, 0.38.0부터** — 한 요청에서 `authorization` 또는 `x-api-key`를 두 번 이상 보내는 호출자에만
영향을 줍니다. shunt 자체 자격 증명이 진짜 업스트림 자격 증명과 슬롯을 공유하면 이제 슬롯 전체가 제거되므로,
업스트림 자격 증명도 함께 사라집니다. 업스트림 자격 증명은 별도 슬롯으로 보내세요.
[게이트웨이 공유](/ko/guides/shared-gateway/)를 참고하세요.

### 읽기/쓰기 관리자 키와 `[server.spend]`로 이동한 지출 표면

**추가** — `[server.admin]`에 `read_keys`와 `write_keys`가 추가되었습니다. 읽기 키는 모든 관리자 GET을 통과하고
모든 변경 작업에서 `403`으로 거부됩니다. 지출 한도 표면은 자체
[`[server.spend]`](/ko/reference/configuration/#serverspend-선택) 테이블로 이동했습니다.
[`[server.admin]`](/ko/reference/configuration/#serveradmin-선택)을 참고하세요.

### `shunt gateway` 로그인, 토큰 헬퍼, Claude Code 런처

**추가** — `shunt gateway login`, `shunt gateway token`, `shunt gateway claude`, `shunt gateway logout`으로
클라이언트가 디바이스 플로우를 통해 공유 게이트웨이에 인증하고 그에 대해 Claude Code를 실행할 수 있습니다.
[게이트웨이 로그인](/ko/guides/gateway-login/)과 [CLI](/ko/reference/cli/)를 참고하세요.

### 일급 구독 업스트림으로서의 Kimi Code OAuth

**추가** — Moonshot API 키뿐 아니라 Kimi Code 구독을 OAuth로 직접 사용할 수 있습니다.
[Kimi](/ko/providers/kimi/)를 참고하세요.

### `[server.gateway.session]` JWT 설정

**추가** — 게이트웨이 세션 JWT 매개변수를
[`[server.gateway.session]`](/ko/reference/configuration/#servergatewaysession-선택) 아래에서 설정할 수 있습니다.

### 설정의 `${VAR}` 및 `${file:}` 참조와 비밀 값 가리기

**추가** — 설정 값이 `${VAR}` 환경 변수와 `${file:…}` 경로 참조를 해석하며, 비밀 필드는 디버그 출력에서 가려져
덤프된 설정이 자격 증명을 흘리지 않습니다. [설정 레퍼런스](/ko/reference/configuration/)를 참고하세요.

### 지출 한도 관리자 API

**추가** — 지출 한도 관리자 API의 첫 단계가
[`[server.spend]`](/ko/reference/configuration/#serverspend-선택) 아래에 도입되었습니다.

### 풀 상태에 계정 플랜 노출

**추가** — 풀 계정 객체가 선택적 `plan` 문자열을 가질 수 있으며, 가능한 경우 프로필 조회로 더 정확한 값으로
정제됩니다. [HTTP 엔드포인트](/ko/reference/endpoints/)를 참고하세요.

### Codex 클라이언트 표면을 `openai/codex` 0.148.0에 동기화

**변경** — Codex 클라이언트 identity와 요청 표면을 업스트림 0.148.0에 맞췄습니다.
[ChatGPT / Codex](/ko/guides/codex/)를 참고하세요.

### 게이트웨이 JWT와 클라이언트 토큰이 업스트림에 도달하지 않음

**보안** — 게이트웨이 JWT를 인증에 성공할 때만이 아니라 형태로 제거하며 두 자격 증명 슬롯 어디로도 전달하지
않습니다. 정적 `[server.auth]` 토큰은 값으로 제거하고, 인바운드 `x-api-key`는 Codex 패스스루에서 제거합니다.
모든 자격 증명 슬롯 전달 지점이 하나의 공유 strip을 거치므로 수용 규칙과 제거 규칙이 어긋날 수 없습니다.
[게이트웨이 공유](/ko/guides/shared-gateway/)를 참고하세요.

### `shunt check`가 라우팅된 Antigravity 자격 증명 가드를 실행

**수정** — `shunt check`가 기동과 동일한 자격 증명 가드를 적용하므로, 자격 증명이 없는 라우팅된 `antigravity`
프로바이더가 서버 기동 전에 보고됩니다. [CLI](/ko/reference/cli/)를 참고하세요.

## 0.37.0 — 2026-08-13

### 옵트인 업스트림 Statuspage 폴링

**추가** — `[server.status]`가 설정된 프로바이더 Statuspage 소스를 폴링해 가장 최근 지표, 설명, 인시던트를
관찰용으로 노출합니다. 라우팅이나 페일오버가 참조하는 일은 없습니다.
[`[server.status]`](/ko/reference/configuration/#serverstatus-선택)를 참고하세요.

### `grok-4.6`과 새로워진 Grok 모델 표면

**추가** — `grok-4.6`을 추가하고 Grok 모델 표면을 갱신했습니다. [xAI / Grok](/ko/guides/xai/)을 참고하세요.

## 0.36.0 — 2026-08-11

### `agy`가 스트리밍과 샌드박싱을 갖춘 에이전트 모드로 실행

**추가** — Antigravity CLI 전송이 스트리밍 출력, 샌드박싱, 탐색된 effort 행렬과 함께 에이전트 모드로
실행됩니다. [Antigravity](/ko/providers/antigravity/)를 참고하세요.

## 0.35.0 — 2026-08-10

### 인바운드 본문 상한 기본값 32 MiB

**변경 · 호환성 깨짐, 0.35.0부터** — 32~64 MiB 사이의 요청 본문, 주로 큰 파일이나 이미지 요청을 보내는 경우에
영향을 줍니다. 상한 기본값이 기존의 하드코딩된 64 MiB에서 32 MiB로 바뀌었고, 그 구간의 요청은
`413 request_too_large`를 반환합니다. 기존 한도로 되돌리려면 `[server.limits]`의 `max_request_bytes`를
올리세요 — [설정 레퍼런스](/ko/reference/configuration/)를 참고하세요.

### HTTP 튜닝 설정 표면

**추가** — `[server.limits]`, `[server.timeouts]` 및 관련 테이블이 이전에는 하드코딩되어 있던 본문, 헤더, URL,
타임아웃 튜닝을 노출합니다. [설정 레퍼런스](/ko/reference/configuration/)를 참고하세요.
