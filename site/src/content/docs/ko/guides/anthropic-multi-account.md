---
title: Anthropic 멀티 계정
description: 여러 Claude 구독 OAuth 계정을 하나의 풀로 묶어 세션 스티키·모델 인지 선제 로테이션과 반응형 페일오버로 운용합니다.
---

shunt는 내장 `anthropic` 프로바이더 뒤에서 여러 Claude 구독 OAuth 자격 증명을 풀로 묶을 수 있습니다. Claude Code가 `x-claude-code-session-id`를 보내면 요청은 세션에 고정(session-sticky)되고, 이 헤더가 없는 요청은 프로바이더별 라운드 로빈을 사용합니다. shunt는 각 계정의 업스트림 쿼터 헤더를 추적해 스티키 계정이 모델 관련 쿼터에 근접하면 선제적으로 로테이션하며, 쿼터 거부·인증 실패·업스트림 장애에 대해서는 반응형 페일오버가 안전판으로 유지됩니다.

:::caution[구독 약관]
구독 자격 증명은 계정 약관이 허용하는 범위에서만 사용하세요. shunt는 비공식 클라이언트이며 Anthropic의 계정·구독 정책을 바꾸지 않습니다.
:::

## 풀 구성

`auth = "claude_oauth"`를 설정하고 명시적인 계정 항목을 추가합니다:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

# 기존 Claude Code 자격 증명 파일. shunt가 갱신하고 다시 써 넣습니다.
[[providers.anthropic.accounts]]
name = "primary"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000" # 선택

# 장기(long-lived) `claude setup-token` 값. 그대로 사용되며 갱신되지 않습니다.
[[providers.anthropic.accounts]]
name = "backup"
token_env = "CLAUDE_BACKUP_OAUTH_TOKEN"
uuid = "11111111-1111-1111-1111-111111111111" # 선택
```

```bash
export CLAUDE_BACKUP_OAUTH_TOKEN='<value from claude setup-token>'
shunt check
shunt run
```

세 가지 Claude 로그인 모드 중 하나로 계정을 저장할 수 있습니다:

```bash
# 새 갱신 가능 로그인 생성(기본은 자동 localhost callback).
shunt login claude --name primary --mode oauth

# 현재의 갱신 가능한 Claude Code 로그인 가져오기.
shunt login claude --name imported --mode import

# 1년짜리 추론 전용 setup token 생성 및 저장.
shunt login claude --name backup --mode setup-token
```

TTY에서 `--mode`를 생략하면 OAuth가 기본 선택된 3-way 프롬프트가 열립니다. 비대화형 입력에서는 기존 `import` 기본값을 유지합니다. `--long-lived`는 `--mode setup-token`의 deprecated alias입니다. Full OAuth는 보통 일회성 `127.0.0.1` callback으로 완료됩니다. `<code>#<state>`를 붙여 넣으려면 `--manual`을 사용하세요. 브라우저 실행, callback bind, 또는 5분 대기가 실패해도 shunt가 수동 붙여넣기로 fallback합니다.

그다음 이름만 있는 항목을 사용합니다:

```toml
[[providers.anthropic.accounts]]
name = "primary"

[[providers.anthropic.accounts]]
name = "backup"
```

스토어 파일은 `~/.shunt/accounts/claude/<name>.json`에 저장되며, `SHUNT_CLAUDE_ACCOUNTS_DIR`로 디렉터리를 재정의할 수 있습니다. 구성된 `accounts` 목록이 비어 있으면 shunt는 스토어를 스캔해 유효한 JSON 계정 파일 전부를 파일명 순서로 사용합니다. 스토어 파일은 비공개입니다(Unix에서 `0600`, 디렉터리는 `0700`).

원격 운영자는 옵트인 [관리자 웹 화면](/ko/guides/admin-remote-provisioning/)에서 브라우저로 갱신 가능한 Full OAuth 계정 또는 1년짜리 setup token 계정을 프로비저닝하고 풀의 현재 상태를 볼 수 있습니다. 기존 credential 파일 가져오기는 CLI 전용입니다.

Full OAuth는 새로운 갱신 가능 credential을 만듭니다. import는 현재의 `~/.claude/.credentials.json` credential을 shunt 스토어로 복사합니다. 두 방식 모두 갱신 기능을 보존하며, import는 현재 계정 UUID도 기록합니다. setup-token 모드는 `claude setup-token`과 동일한 1년짜리 추론 전용 PKCE 플로우를 실행합니다. 승인 후 shunt는 표시된 인가 코드를 교환하고 토큰과 발급 계정 UUID를 함께 저장하며 토큰을 출력하지 않습니다. 이렇게 하면 풀이 다른 계정을 선택할 때도 `metadata.user_id.account_uuid`가 일치된 상태로 유지됩니다. 이름을 재사용하면 해당 계정의 스토어 파일이 교체됩니다. 기존 외부 setup token에는 여전히 `token_env`와 명시적 `uuid`가 필요합니다.

:::caution[Refresh token 회전]
성공적인 갱신은 대체 refresh token을 반환하고 이전 값을 무효화할 수 있습니다. 갱신 가능한 스토어 파일마다 활성 shunt owner를 하나만 두세요. 여러 프로세스가 같은 파일을 가리키거나, 별도 호스트에서 독립적으로 복사본을 실행하지 마세요. 프로세스마다 별도로 프로비저닝하거나, 갱신 불가능한 credential을 의도적으로 공유하는 경우 정적 setup token을 사용하세요.
:::

## 계정 필드

| 필드 | 필수 | 의미 |
| :-- | :-- | :-- |
| `name` | 예 | 소문자, 숫자, 하이픈만 포함하는 고유 레이블. 다른 소스 필드가 없으면 이름이 일치하는 shunt 스토어 파일을 사용합니다. |
| `credentials` | 사용 가능한 소스 중 하나 | Claude Code `.credentials.json` 형태의 파일. `~/`가 확장됩니다. shunt는 만료가 가까워지면 갱신하고 갱신된 토큰을 원자적으로 다시 써 넣습니다. |
| `token_env` | 사용 가능한 소스 중 하나 | setup token이 들어 있는 환경 변수. 값은 그대로 사용되며 401 이후 갱신할 수 없습니다. |
| `uuid` | 아니요 | 기존 `metadata.user_id.account_uuid`를 다시 쓰기 위한, 선택된 계정의 Anthropic UUID이자, 풀에서 별칭을 통합하는 데 쓰이는 안정적인 아이덴티티. 이름만 있는 항목(스토어 스캔으로 해석됨)은 선택이 일어나기 전에 스토어의 `shuntAccountUuid`로부터 자동으로 채워집니다. `credentials` 또는 `token_env`로 설정된 항목의 아이덴티티는 `uuid`가 설정되어 있으면 그 값, 없으면 `name`이며, 그 아이덴티티가 다른 별칭의 명시적 `uuid` 또는 이름 폴백 아이덴티티와 일치할 때마다 통합됩니다. 명확하고 의도적인 통합을 위해서는 두 항목 모두에 일치하는 비어 있지 않은 `uuid`를 설정하세요(shunt는 명시적 `uuid`가 다른 계정의 이름 폴백 아이덴티티와 우연히 일치하는 경우도 경고합니다). |
| `threshold` | 아니요 | 창(window)별 값이 없는 모든 창에 적용되는 계정별 소프트 쿼터 임계값, `[0.0, 1.0]` 범위. 낮은 값은 일찍 로테이션되는 백업 계정을 나타냅니다. |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | 아니요 | 창별 소프트 임계값; 각각 해당 창에서 `threshold`보다 우선합니다. |
| `priority` | 아니요 | 스티키 계정이 비정상일 때의 선택 우선순위; 낮을수록 우선되며 기본값은 `100`입니다. |
| `disabled` | 아니요 | `true`이면 구성과 관리자 대시보드에는 남긴 채 계정을 선택 대상에서 완전히 제외합니다. |

한 계정에 `credentials`와 `token_env`를 동시에 설정하지 마세요.

:::note[Duplicate names for one real account]
`uuid`는 풀의 안정적인 업스트림 아이덴티티이기도 합니다. 두 이름이 동일한 UUID를 가지면 shunt는 이들을 **하나의 계정**으로 취급합니다: 쿼터, 쿨다운, 사용량, 헬스, 리프레시 락을 공유하며, 페일오버는 중복된 별칭을 건너뜁니다. 스티키 해싱과 라운드 로빈은 서로 다른 아이덴티티 단위로 동작하므로, 별칭을 추가해도 세션이 이동하지 않습니다. 대표는 `priority`가 가장 낮은 활성화된 별칭이며, 그다음은 첫 번째 항목입니다; 오직 그 대표의 토큰만 시도됩니다. shunt는 중복 아이덴티티 경고를 로그에 남깁니다(설정 파일의 `[[providers.anthropic.accounts]]` 간 중복은 리로드를 포함하여 설정을 성공적으로 로드할 때마다 한 번, 스토어에서 발견된 중복은 중복 집합이 바뀔 때마다 한 번 — 둘 다 요청마다는 아닙니다). 따라서 그 대표 토큰이 유효하지 않고 다른 별칭의 토큰이 유효하더라도, shunt는 그 별칭을 시도하지 않습니다.

admin web 화면을 통해 스토어 관리 계정을 삭제하면, 동일한 아이덴티티를 공유하는 다른 스토어 별칭이 남아있지 않은 것이 확인된 경우에만 공유 인프로세스 헬스가 초기화됩니다. 스캔이 실패하면 헬스를 보존합니다. 이는 admin 스토어 삭제 시맨틱이며, TOML 설정에서 별칭을 제거하거나 credential 파일을 직접 삭제하는 것은 이 정리 과정을 거치지 않습니다.
:::

## 선택과 선제 로테이션

- `x-claude-code-session-id`가 있으면: 안정적인 해시가 스티키 계정을 고릅니다. 그 계정이 사용 가능하고 전환 임계값 아래라면 shunt는 그 계정을 첫 번째로 유지합니다.
- 헤더가 없으면: 프로바이더마다 자체 라운드 로빈 카운터를 사용합니다.
- `claude_oauth` 계정 풀이 처리하는 모든 업스트림 응답에서, shunt는 다음 헤더가 있으면 기록합니다:
  - `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-7d_oi-utilization`;
  - `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, `anthropic-ratelimit-unified-7d_oi-reset`(Unix 초); 그리고
  - `anthropic-ratelimit-unified-status`.
- 기본 전환 임계값은 `0.98`입니다. 통합(unified) status가 `rejected`이거나, 공유 5시간 사용률이 해당 임계값에 도달했거나, 적용되는 주간 사용률이 해당 임계값에 도달하면 그 계정은 쿼터에 근접한 것입니다. 임계값은 계정별로(위의 `threshold*` 필드) 또는 풀 전체로(자세한 내용은 [선택 튜닝](#선택-튜닝-serverpool) 참고) 낮출 수 있습니다.
- 5시간 버킷은 모든 모델에 적용됩니다. Fable 모델 id는 `7d_oi` 주간 버킷의 사용률이 있으면 그것을 쓰고, 없으면 공유 `7d`로 폴백합니다. 그 외 모든 모델 계열은 공유 `7d`를 사용합니다; 현재 Sonnet 전용 헤더가 없으므로 Sonnet도 `7d`를 사용합니다.
- 쿼터에 근접했거나 쿨다운됐거나 비활성화(disabled)된 스티키 계정은 선제적으로 로테이션됩니다. shunt는 임계값 아래의 사용 가능한 계정을 `priority`(낮은 값 우선) 순으로 먼저 선호하고, 그다음 적용되는 주간 버킷이 가장 빨리 리셋되는 순서로 선호해 쓰지 않으면 사라지는(use-or-lose) 쿼터부터 소진합니다. 주간 리셋을 알 수 없는 계정이 먼저 정렬됩니다. 그다음 사용 가능한 쿼터 근접 계정, 그다음 가장 빨리 회복되는 순서의 쿨다운 계정이 이어집니다. `[server.pool]`이 구성되어 있으면 번-레이트(burn-rate) 여유가 주간 리셋 타이브레이크를 대신합니다(아래 참고).
- shunt는 로컬 쿼터 상태 때문에 닫힌 채로 실패(fail closed)하지 않습니다: `disabled`가 아닌 모든 계정은 쿼터에 근접했거나 쿨다운 중이어도 시도 순서에 남아 있습니다.
- 쿼터 버킷은 리셋 타임스탬프가 지나면 자동으로 비워집니다. 성공 응답은 선택된 계정의 쿨다운을 해제합니다.

풀의 선택, 쿨다운, 쿼터 상태는 프로세스가 살아 있는 동안 설정 핫 리로드를 거쳐도 유지됩니다. 선제 로테이션으로 업스트림 제한을 피하지 못하는 경우에는 반응형 페일오버가 계속 동작합니다.

## 선택 튜닝 (`[server.pool]`)

선택적 `[server.pool]` 테이블(이슈 #135)은 위 동작 위에 창(window)별 소프트 임계값과 번-레이트(burn-rate) 인지 정렬을 추가합니다. 이 테이블이 없으면 선택은 이전과 동일하게 단일 내장 `0.98` 임계값을 사용합니다.

```toml
[server.pool]
# hard_threshold = 0.98      # (기본값) 백스톱; 이 값 이상이면 항상 마지막으로 정렬됨
default_threshold = 0.9      # 모든 창에 대한 소프트 기본값
default_threshold_5h = 0.95  # 창별 오버라이드
default_threshold_fable = 0.85
burn_rate_avoidance = true   # 리셋 전에 임계값에 도달할 것으로 예측되는 계정을 회피
usage_refresh_seconds = 300  # 갱신 가능 계정의 out-of-band 사용량 재보정
state_path = "shunt-state.json"  # 재시작 간 쿼터 유지(워밍업)
ramp_initial_concurrency = 2 # 폭주 제어: 방금 전환된 계정을 슬로 스타트

[[providers.anthropic.accounts]]
name = "primary"
priority = 1                 # 스티키 계정이 비정상일 때 우선 선택됨

[[providers.anthropic.accounts]]
name = "backup"
threshold = 0.5              # 백업: 쿼터의 절반이 소진되면 로테이션

[[providers.anthropic.accounts]]
name = "spare"
disabled = true              # 구성은 유지되지만 절대 선택되지 않음
```

- **임계값 해석(Threshold resolution).** 각 창 `X`(`5h`, `7d`, `fable`)에 대해 유효 소프트 임계값은 계정 `threshold_X` → 계정 `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold` 순으로 결정되며, `hard_threshold`로 상한이 걸립니다. 모든 값은 `[0.0, 1.0]` 범위의 사용률 비율이며, 범위를 벗어나면 `shunt check`가 실패합니다.
- **번-레이트 여유(Burn-rate headroom).** 각 창의 사용률과 리셋 시점(창 길이는 5시간과 7일로 고정)으로부터, shunt는 관측된 평균 속도로 소프트 임계값에 도달할 때까지의 시간에서 창이 리셋될 때까지의 시간을 뺀 값을 예측합니다. 여유(headroom)가 양수이면 현재 속도로도 리셋 시점까지 버틸 수 있다는 뜻입니다. `priority`가 같은 사용 가능한 계정은 여유가 큰 순서로 정렬되며, 관측되지 않은 창은 무제한 여유로 간주됩니다.
- **예측적 회피(Predictive avoidance).** `burn_rate_avoidance = true`이면, 예측된 여유가 음수인 계정은 임계값에 도달하기 *전에* 쿼터 근접 상태로 간주되어 로테이션됩니다. 기본값은 꺼짐이며 — 여유 기준 정렬 자체는 이 설정과 무관하게 항상 이루어집니다.
- **전체 근접 가드(All-near guard).** 모든 계정이 소프트 임계값을 넘겼거나(또는 소진이 예측되면), 풀이 비지 않습니다: 근접 계정은 여유가 가장 큰 순서로 서빙되고, `hard_threshold` 이상인 계정은 여전히 마지막으로 정렬되며, 그다음 쿨다운 중인 계정만 이어집니다.
- **적용 범위(Scope).** 쿼터 관련 노브는 두 풀 계열 모두에 작동합니다: 이 풀은 `anthropic-ratelimit-unified-*` 헤더로부터, [Codex 풀](/ko/guides/codex-multi-account/)은 보고된 `x-codex-*` 5h/7d 윈도우로부터 동작합니다(이슈 #195). Codex에는 Fable 범위의 `7d_oi` 창이 없어 `default_threshold_fable`은 그곳에서 무력화되며, `priority`와 `disabled`는 어디서나 적용됩니다.
- 관리자 풀 엔드포인트(`GET /admin/pool`)는 각 계정의 `priority`, `disabled` 플래그를 보고하며, `[server.pool]`이 구성되어 있으면 현재 여유(headroom) 예측치를 초 단위로 함께 보고합니다; 대시보드의 상태 열은 비활성화된 계정을 표시합니다.

## Usage-API 재보정

쿼터 헤더는 shunt를 통과한 트래픽만 반영합니다. `usage_refresh_seconds`는 `GET /api/oauth/usage`를 폴링해 권위 있는 사용률과 리셋 시각을 동일한 5시간, 공유 주간(`7d`), Fable 전용 주간(`7d_oi`) 창에 적용함으로써 그 간극을 메웁니다.

필드가 없거나 `0`이면 폴링은 꺼집니다; 60 미만의 양수 값은 60초로 올림됩니다. imported된 갱신 가능 계정만 대상이며, 장기 `claude setup-token`과 `token_env` 계정은 토큰이 엔드포인트를 호출할 수 없어 건너뜁니다. 간격은 부팅 시 고정되므로 설정 리로드는 폴러를 시작·중지·재조정하지 않습니다. 이 주기적 보정은 반응형 헤더 상태를 대체하지 않고 보완합니다.

## 쿼터 상태 영속화

풀 쿼터는 메모리에 있으므로 재시작은 cold로 시작합니다: 각 계정은 재시작 후 첫 응답 전까지 미관측 상태로 보이고, 이로 인해 burn-rate 회피가 비활성화되며 트래픽으로 풀이 다시 채워질 때까지 `GET /usage`가 빈 값을 반환합니다. `state_path`를 설정하면 각 계정의 창별 사용률과 리셋을 그 파일에 저장해, 풀이 마지막으로 관측된 상태에서 워밍업합니다.

이 파일은 권위 있는 소스가 아니라 best-effort 캐시입니다 — 쿼터는 어차피 업스트림 응답에서 재도출되므로, 파일이 없거나·오래됐거나·손상돼도 cold start만 발생할 뿐 부팅 실패로 이어지지 않습니다. 쓰기는 비공개 temp 파일(Unix에서 `0600`)을 대상 위로 원자적으로 rename하는 방식이며, 쿼터가 변경됐을 때만 15초 백그라운드 타이머로 이뤄집니다. 쓰기에 실패하면 dirty 상태를 유지하고 다음 tick에서 재시도합니다. 쿨다운은 저장되지 않고(재시작 시 소멸), 복원된 창 중 이미 리셋이 지난 것은 복원 후 첫 선택 또는 snapshot에서 lazy하게 폐기됩니다. 경로는 부팅 시 고정되며, 필드가 없으면 영속화는 꺼집니다.

## 페일오버 규칙

| 응답 | 동작 |
| :-- | :-- |
| 2xx | 릴레이하고 정상으로 표시합니다. |
| 429 + `anthropic-ratelimit-unified-5h-status`, `-7d-status`, `-7d_oi-status` 중 하나의 `rejected` 값 | 쿼터 소진: 숫자 `retry-after`로 쿨다운(기본 60초, 1–3600초로 클램프)한 뒤 로테이션합니다. |
| 일반 429 | 일시적 스로틀: 숫자 `retry-after`만큼 대기(기본 1초, 상한 300초)하고 **같은** 계정을 한 번 재시도한 뒤, 그 재시도 응답을 릴레이합니다. |
| `credentials`에서의 401 | 강제 갱신 후 같은 계정을 한 번 재시도; 여전히 401이면 5분 쿨다운 후 로테이션합니다. |
| `token_env` 또는 스토어 관리 setup token에서의 401 | 갱신 불가: 5분 쿨다운 후 로테이션합니다. |
| 5xx 또는 전송 실패 | 30초 쿨다운 후 로테이션합니다. |
| 그 외 status | 페일오버 없이 릴레이합니다. |

분류는 응답 본문이 스트리밍되기 전에 일어나므로, 스트림 중간의 실패는 절대 재전송되지 않습니다. 풀이 응답을 받은 뒤 시도를 소진하면 클라이언트는 마지막 실제 업스트림 status와 본문을 받습니다. 어떤 업스트림 응답도 받기 전에 모든 계정이 실패하면 shunt는 게이트웨이 소유의 오류를 반환합니다.

Anthropic으로 라우팅되는 `POST /v1/messages/count_tokens` 요청도 같은 풀을 사용합니다.

## 요청과 응답 변경

선택된 계정에 대해 shunt는 클라이언트 인증을 다음으로 교체합니다:

```http
Authorization: Bearer <selected OAuth token>
anthropic-beta: ...,oauth-2025-04-20
```

들어온 `authorization`과 `x-api-key`를 모두 제거하고, `oauth-2025-04-20`은 없을 때만 덧붙이며, 다른 종단 간(end-to-end) 헤더는 보존합니다.

풀링된 응답은 계정을 식별합니다:

```http
x-shunt-account: backup
```

공유 게이트웨이에서는 중립적인 계정 이름을 사용하세요. 이 헤더는 응답을 받는 모든 인가된 클라이언트에게 구성된 레이블을 노출합니다. 풀 소진 후 마지막 업스트림 응답을 릴레이할 때는 `x-shunt-account`가 생략됩니다.

### `account_uuid`

Claude Code는 문자열 값인 `metadata.user_id` 안에 계정 메타데이터를 JSON으로 인코딩할 수 있습니다. 선택된 계정에 `uuid`가 있으면 shunt는 **기존** 내부 `account_uuid`를 그 값으로 교체합니다. 메타데이터가 없거나, 형식이 잘못됐거나, `account_uuid`가 없거나, 선택된 계정에 UUID가 없으면 본문을 그대로 둡니다. 없는 메타데이터를 주입하지는 않습니다.

## 보안 제약

`claude_oauth`는 다음 조건에서만 허용됩니다:

- 프로바이더가 `kind = "anthropic"`이고;
- `base_url`이 HTTPS를 사용하며;
- 호스트가 `anthropic.com`이거나 `api.anthropic.com` 같은 그 서브도메인일 때.

이 시작 검사는 OAuth bearer가 다른 오리진으로, 또는 평문으로 전송되는 것을 막습니다. HTTPS와 호스트 검사는 **루프백 호스트에서는 완화**됩니다(`localhost`, `127.0.0.1`, `[::1]` 등): 루프백 `base_url`은 평문 HTTP와 임의의 호스트를 쓸 수 있어 로컬 디버깅 프록시나 목(mock)이 트래픽을 받을 수 있습니다 — bearer가 운영자의 머신을 벗어날 수는 없습니다. 루프백이 아닌 호스트에는 항상 HTTPS + `anthropic.com`이 요구됩니다. 공유 배포에서는 `claude_oauth`가 게이트웨이 소유 자격 증명을 소비하므로 [`[server.auth]`](/ko/guides/shared-gateway/#인바운드-클라이언트-토큰)도 함께 구성하세요. 클라이언트는 이미 보내고 있는 `ANTHROPIC_AUTH_TOKEN`으로 인증됩니다(`x-shunt-token`, `x-api-key`와 나란히 `Authorization: Bearer`로도 클라이언트 토큰을 받습니다) — 풀 전용 게이트웨이라면 `ANTHROPIC_CUSTOM_HEADERS` 줄이 필요 없습니다.

## 폭주 제어(storm control)

`[server.pool] ramp_initial_concurrency`(기본값 꺼짐)를 설정하면 계정 아이덴티티별 동시 허용(admission)을 슬로 스타트 램프로 제어하므로, 페일오버 전환이 방금 선택된 계정에 진행 중인 모든 요청을 한꺼번에 몰아넣지 못합니다. 방금 트래픽을 받기 시작한 아이덴티티는 최대 구성된 개수만큼의 동시 요청만 허용하며, 성공 응답마다 허용치가 두 배로 늘고(슬로 스타트), 페일오버는 램프를 다시 시작하며, 거부된 요청은 선택 순서상 다음 계정으로 넘어갑니다(마지막 후보는 항상 시도됩니다). [`[server.pool]`](/ko/reference/configuration/)를 참고하세요.

구현 동작은 [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude)와 배포된 Claude Code 바이너리를 참고했습니다. shunt는 teamclaude에 대한 런타임 의존성이 없습니다.
