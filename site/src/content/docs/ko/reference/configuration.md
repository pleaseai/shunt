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

## `[server.gateway]` (선택)

이 테이블은 Claude Code의 managed `forceLoginMethod: "gateway"`에서 사용하는 [OAuth device-flow gateway 로그인](/ko/guides/gateway-login/)을 활성화합니다. 테이블이 없으면 shunt는 `/.well-known/oauth-authorization-server`, `/oauth/device_authorization`, `/oauth/token`, `/device`, `/managed/settings`를 등록하지 않습니다.

| 키 | 기본값 | 의미 |
| :-- | :-- | :-- |
| `public_url` | 필수 | JWT issuer와 OAuth endpoint 기준으로 사용하는 외부 공개 HTTPS origin. `http`는 loopback에서만 허용 |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | 32 bytes 이상의 HS256 signing secret을 담는 env 변수 |
| `users_env` | `SHUNT_GATEWAY_USERS` | 쉼표로 구분된 `email:secret` approval user를 담는 env 변수 |
| `token_ttl_seconds` | `3600` | access token 수명. `expires_in`으로 반환 |
| `trust_forwarded_for` | `false` | `/device` rate-limit identity로 `X-Forwarded-For`/`X-Real-IP`를 신뢰. client 제공 값을 교체하는 trusted proxy 뒤에서만 활성화 |
| `state_path` | `~/.shunt/gateway-sessions.json` | 재시작 후에도 refresh session을 유지하는 파일. token은 SHA-256 hash로 저장하고 Unix에서는 소유자 전용 권한(`0600`)으로 원자적으로 기록. `""`로 설정하면 memory-only session 사용(home directory를 찾지 못한 경우에도 동일) |

URL이 경로 등을 포함하지 않은 HTTPS origin이 아니거나(`http`는 loopback에서만 허용), TTL이 0이거나, secret이 없거나 32 bytes 미만이거나, user list가 비었거나 잘못되면 시작은 fail closed합니다. secret에는 `:`를 포함할 수 있으며 첫 번째 colon만 email과 secret을 구분합니다. env-backed secret과 user 변경은 config reload 시 반영되지만, route tree는 boot 시 고정되므로 테이블 추가·제거에는 restart가 필요합니다.

발급된 bearer는 선택된 provider가 server-side credential을 주입할 때 `/v1/models`, `/v1/messages`, `/v1/messages/count_tokens`를 인증합니다. passthrough provider는 open 상태를 유지합니다. `[server.auth]`도 있으면 어느 credential이든 access를 허용합니다. refresh session은 기본적으로 재시작 후에도 유지됩니다. boot 시 `state_path`의 token hash를 복원하므로 사용자는 계속 silent refresh할 수 있습니다. 이 파일을 여러 shunt process가 동시에 공유하면 안 됩니다. `state_path = ""`이면 session은 memory-only이며, config reload에서는 유지되지만 shunt를 재시작하면 access JWT 만료 후 다시 로그인해야 합니다. Device grant와 rate-limit counter는 항상 memory-only이므로 로그인 도중 재시작하면 해당 시도만 손실됩니다. 만료된 grant와 idle rate-limit identity는 opportunistic하게 정리되며 각각 최대 4,096개로 제한됩니다. 사용한 refresh-token tombstone은 30일 동안 family당 최대 64개 유지되고, 30일 동안 사용하지 않은 active refresh token은 만료됩니다.

### `[[server.gateway.policies]]` (선택)

`[server.gateway]`가 있으면 인증된 `GET /managed/settings`가 등록되고, 순서가 있는 비어 있지 않은 policy 목록은 이 managed document를 제공합니다. 각 policy는 선택적 `[server.gateway.policies.match]`와 필수 open-schema `[server.gateway.policies.cli]` object를 가집니다. `match` 생략, `match = {}`, 또는 `emails` 없음은 catch-all입니다. 명시적으로 빈 `emails` 목록이나 빈 entry는 시작 오류입니다.

모든 catch-all policy를 순서대로 merge한 뒤, 첫 번째 정확한(case-sensitive) email policy를 위에 merge합니다. object는 재귀 merge하고 array는 교체하되, key에 `deny`가 포함된 array는 중복 없는 union으로 합칩니다. 알려진 key는 시작과 hot reload 때 검증합니다. `availableModels`는 string만 담은 array여야 하고, `env`는 string·number·boolean scalar value만 담은 table이어야 합니다. 알려지지 않은 key는 open-schema로 유지하지만, 모든 value는 JSON으로 표현할 수 있어야 하며 non-finite float는 거부됩니다.

`policies`가 없으면 endpoint는 `404`를 반환합니다. policy가 구성됐지만 일치하는 user-specific 또는 catch-all settings가 없으면 telemetry 활성 시 telemetry 전용 `settings.env`를, 비활성 시 `settings: {}`를 담은 `200`을 반환합니다. response에는 `uuid`, `checksum`, checksum을 담은 quoted `ETag`가 있으며, 일치하는 `If-None-Match`에는 `304`를 반환합니다.

해석된 `cli.availableModels`는 gateway JWT request의 `/v1/messages`와 `/v1/messages/count_tokens`에 적용됩니다. top-level `model` 끝의 Claude Code context-window hint(`[1m]` 또는 `[1M]`) 하나를 제거한 뒤 비교하며, 목록에 없으면 `400 invalid_request_error`로 거부합니다. static `[server.auth]` credential은 gateway policy user를 식별하지 않으므로 이 제한을 받지 않습니다.

### `[server.gateway.telemetry]` (선택)

`forward_to`는 필수 HTTP(S) `url`과 선택적 string `headers` map을 가진 destination array입니다. 비어 있지 않은 목록은 managed `settings.env`에 값 6개를 주입합니다. `CLAUDE_CODE_ENABLE_TELEMETRY=1`, `OTEL_METRICS_EXPORTER`/`OTEL_LOGS_EXPORTER`/`OTEL_TRACES_EXPORTER=otlp`, `OTEL_EXPORTER_OTLP_ENDPOINT=public_url`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`입니다. 충돌 시 policy env value가 우선합니다. 이 테이블은 M-B에서 environment push만 제어하며 inbound OTLP ingest/relay는 M-C(#189)입니다.

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
| `provider` | `codex` | inbound request를 처리할 `[providers.<name>]` 테이블 이름. `auth = "chatgpt_oauth"`를 사용해야 함 |

`POST /backend-api/codex/responses`, `POST /responses`, `POST /v1/responses`를 등록하며, 모두 지정한 provider의 account pool이 처리합니다. `[server.auth]`가 있으면 다른 server-side credential route처럼 유효한 client token을 요구합니다. `[server.auth]`가 없으면 operator의 Codex credential을 주입하면서도 접근 가능한 누구에게나 **open** 상태이므로 loopback 외 환경에서는 반드시 보호하세요. `/v1/messages`와 달리 request는 Anthropic Messages로 변환하거나 그 반대로 변환하지 않고 upstream과 verbatim relay합니다.

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
| `usage_refresh_seconds` | 비활성(`0`/미설정) | `GET /api/oauth/usage` 폴링 간격(초); 60 미만의 양수 값은 60초 하한으로 올림 |
| `state_path` | 미설정 | 풀의 계정별 쿼터 상태를 저장할 파일; 재시작 시 빈 풀 대신 마지막으로 관측된 사용률에서 워밍업. 미설정이면 영속화 비활성(기본값) |
| `ramp_initial_concurrency` | 비활성(`0`/미설정) | 폭주 제어: 방금 트래픽을 받기 시작한 계정 아이덴티티의 초기 동시 허용치. `0` 또는 미설정이면 허용 게이팅 비활성 |

각 창 `X`에 대해 유효 소프트 임계값은 다음 순서로 결정됩니다: 계정 `threshold_X` → 계정 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, 그리고 `hard_threshold`로 상한이 걸립니다. 모든 임계값은 `[0.0, 1.0]` 범위의 사용률 비율이며, 범위를 벗어나면 시작이 실패합니다. 임계값과 번-레이트 노브는 두 풀 계열 모두를 관장합니다: Anthropic 풀은 `anthropic-ratelimit-unified-*` 헤더로부터, Codex/ChatGPT 풀은 `x-codex-*` 5시간/주간 윈도우로부터 동작합니다(Codex에는 Fable 범위의 `7d_oi` 창이 없어 `default_threshold_fable`은 그곳에서 무력화됩니다). `usage_refresh_seconds`는 Anthropic 전용입니다 — Codex에는 out-of-band usage API가 없습니다.

양수 `usage_refresh_seconds`는 추가로 백그라운드 폴러를 시작해, Claude 계정 풀의 쿼터 상태를 Anthropic OAuth usage API와 대조해 재보정합니다; 미설정 또는 `0`이면 비활성(기본값)입니다. imported(갱신 가능) `claude_oauth` 계정만 폴링되며 — 장기 `claude setup-token`이나 `token_env` 계정은 usage 엔드포인트가 비갱신 토큰을 거부하므로 건너뜁니다. 폴러는 헤더 기반 5h/주간/Fable(`7d_oi`) 쿼터 상태를 shunt 외부의 동일 계정 소비까지 포함한 권위 있는 사용량과 대조합니다. 간격은 부팅 시 고정되며, 설정 리로드는 폴러를 시작·중지·재조정하지 않습니다.

`state_path`는 풀의 쿼터 상태(모든 provider 계정의 창별 사용률과 리셋)를 디스크에 저장합니다. 없으면 재시작이 빈 풀로 시작해, 각 계정이 재시작 후 첫 응답 전까지 미관측 상태로 보이면서 burn-rate 회피가 비활성화되고 `GET /usage`가 트래픽으로 풀이 다시 채워질 때까지 빈 값을 반환합니다. 이 파일은 권위 있는 소스가 아니라 best-effort 캐시입니다 — 쿼터는 어차피 업스트림 응답에서 재도출되므로, 파일이 없거나·오래됐거나·손상돼도 cold start만 발생할 뿐 부팅 실패로 이어지지 않습니다. 쓰기는 비공개 temp 파일(Unix에서 `0600`)을 대상 위로 원자적으로 rename하는 방식이며, 쿼터가 변경됐을 때만 백그라운드 타이머로 이뤄집니다. 쓰기에 실패하면 다음 tick에서 재시도합니다. 쿨다운은 저장되지 않고(재시작 시 소멸), 복원된 창 중 이미 리셋이 지난 것은 복원 후 첫 선택 또는 snapshot에서 lazy하게 폐기됩니다. 경로는 부팅 시 고정되며, 설정 리로드는 영속화를 시작·중지하거나 경로를 바꾸지 않습니다.

양수 `ramp_initial_concurrency`는 모든 계정 풀에 **폭주 제어(storm control)**를 활성화합니다: 페일오버 전환 후에는 진행 중인 동시 요청이 방금 선택된 계정에 한꺼번에 몰릴 수 있습니다. 게이트를 켜면, 방금 트래픽을 받기 시작한 아이덴티티(신규, 쿨다운에서 복귀, 또는 60초간 유휴)는 최대 구성된 개수만큼의 동시 요청만 허용합니다; 성공 응답마다 허용치가 두 배로 늘고(슬로 스타트), 페일오버에 해당하는 실패는 램프를 다시 시작하며, 거부된 요청은 선택 순서상 다음 계정으로 넘어갑니다. 마지막 남은 후보는 게이트와 무관하게 항상 시도되므로, 게이팅은 요청을 미룰 수는 있어도 게이트가 없었다면 서빙됐을 요청을 실패시키는 일은 절대 없습니다. 이는 곧 풀의 모든 계정이 하나의 업스트림 아이덴티티로 귀결되면 사실상 게이트가 없는 것과 같다는 뜻이기도 합니다: 유일한 후보가 곧 마지막 후보이므로, 이 설정은 서로 다른 계정 아이덴티티가 둘 이상일 때만 효력이 있습니다.

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

모든 OTLP 요청에 붙는 추가 헤더(예: 호스팅 컬렉터 토큰). 표준 `OTEL_EXPORTER_OTLP_HEADERS` 아래로 병합됩니다.

| 키 | 의미 |
| :-- | :-- |
| 임의 | 헤더 이름 → 값, 예: `authorization = "Bearer <token>"` |

## 라우팅 우선순위

정확한 `[[routes]]` 일치 → `[[route_prefixes]]` 프리픽스 일치 → `server.default_provider`.
