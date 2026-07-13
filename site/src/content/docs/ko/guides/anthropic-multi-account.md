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

두 로그인 모드 중 어느 쪽으로든 계정을 저장할 수 있습니다:

```bash
# 현재의 갱신 가능한 Claude Code 로그인을 가져옵니다.
shunt login claude --name primary

# 또는 1년짜리 setup token을 생성해 저장합니다.
shunt login claude --name backup --long-lived
```

그다음 이름만 있는 항목을 사용합니다:

```toml
[[providers.anthropic.accounts]]
name = "primary"

[[providers.anthropic.accounts]]
name = "backup"
```

스토어 파일은 `~/.shunt/accounts/claude/<name>.json`에 저장되며, `SHUNT_CLAUDE_ACCOUNTS_DIR`로 디렉터리를 재정의할 수 있습니다. 구성된 `accounts` 목록이 비어 있으면 shunt는 스토어를 스캔해 유효한 JSON 계정 파일 전부를 파일명 순서로 사용합니다. 스토어 파일은 비공개입니다(Unix에서 `0600`, 디렉터리는 `0700`).

원격 운영자를 위해, 옵트인 [관리자 웹 화면](/ko/guides/admin-remote-provisioning/)에서 브라우저로 1년짜리 setup token 계정을 프로비저닝하고 풀의 현재 상태를 볼 수 있습니다; 갱신 가능한 로그인 가져오기 플로우는 CLI 전용으로 남습니다.

`--long-lived`가 아닌 명령은 현재의 `~/.claude/.credentials.json` 로그인을 shunt 스토어로 복사하고, 갱신 기능을 보존하며, 현재 계정 UUID를 기록합니다. `--long-lived`는 `claude setup-token`과 동일한 1년짜리 추론 전용 PKCE 플로우를 실행합니다; 승인 후 shunt는 표시된 인가 코드를 교환하고 토큰과 발급 계정 UUID를 함께 저장하며 토큰을 출력하지 않습니다. 이렇게 하면 풀이 다른 계정을 선택할 때도 `metadata.user_id.account_uuid`가 일치된 상태로 유지됩니다. 이름을 재사용하면 해당 계정의 스토어 파일이 교체됩니다. 기존 외부 setup token에는 여전히 `token_env`와 명시적 `uuid`가 필요합니다.

## 계정 필드

| 필드 | 필수 | 의미 |
| :-- | :-- | :-- |
| `name` | 예 | 소문자, 숫자, 하이픈만 포함하는 고유 레이블. 다른 소스 필드가 없으면 이름이 일치하는 shunt 스토어 파일을 사용합니다. |
| `credentials` | 사용 가능한 소스 중 하나 | Claude Code `.credentials.json` 형태의 파일. `~/`가 확장됩니다. shunt는 만료가 가까워지면 갱신하고 갱신된 토큰을 원자적으로 다시 써 넣습니다. |
| `token_env` | 사용 가능한 소스 중 하나 | setup token이 들어 있는 환경 변수. 값은 그대로 사용되며 401 이후 갱신할 수 없습니다. |
| `uuid` | 아니요 | 기존 `metadata.user_id.account_uuid`를 다시 쓰기 위한, 선택된 계정의 Anthropic UUID. |

한 계정에 `credentials`와 `token_env`를 동시에 설정하지 마세요.

## 선택과 선제 로테이션

- `x-claude-code-session-id`가 있으면: 안정적인 해시가 스티키 계정을 고릅니다. 그 계정이 사용 가능하고 전환 임계값 아래라면 shunt는 그 계정을 첫 번째로 유지합니다.
- 헤더가 없으면: 프로바이더마다 자체 라운드 로빈 카운터를 사용합니다.
- `claude_oauth` 계정 풀이 처리하는 모든 업스트림 응답에서, shunt는 다음 헤더가 있으면 기록합니다:
  - `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-7d_oi-utilization`;
  - `anthropic-ratelimit-unified-5h-reset`, `anthropic-ratelimit-unified-7d-reset`, `anthropic-ratelimit-unified-7d_oi-reset`(Unix 초); 그리고
  - `anthropic-ratelimit-unified-status`.
- 전환 임계값은 `0.98`입니다. 통합(unified) status가 `rejected`이거나, 공유 5시간 사용률이 `0.98` 이상이거나, 적용되는 주간 사용률이 `0.98` 이상이면 그 계정은 쿼터에 근접한 것입니다.
- 5시간 버킷은 모든 모델에 적용됩니다. Fable 모델 id는 `7d_oi` 주간 버킷의 사용률이 있으면 그것을 쓰고, 없으면 공유 `7d`로 폴백합니다. 그 외 모든 모델 계열은 공유 `7d`를 사용합니다; 현재 Sonnet 전용 헤더가 없으므로 Sonnet도 `7d`를 사용합니다.
- 쿼터에 근접했거나 쿨다운된 스티키 계정은 선제적으로 로테이션됩니다. shunt는 임계값 아래의 사용 가능한 계정을, 적용되는 주간 버킷이 가장 빨리 리셋되는 순서로 선호해 쓰지 않으면 사라지는(use-or-lose) 쿼터부터 소진합니다. 주간 리셋을 알 수 없는 계정이 먼저 정렬됩니다. 그다음 사용 가능한 쿼터 근접 계정, 그다음 가장 빨리 회복되는 순서의 쿨다운 계정이 이어집니다.
- shunt는 로컬 쿼터 상태 때문에 닫힌 채로 실패(fail closed)하지 않습니다: 모든 계정이 쿼터에 근접했거나 쿨다운 중이어도 각 계정은 시도 순서에 남아 있습니다.
- 쿼터 버킷은 리셋 타임스탬프가 지나면 자동으로 비워집니다. 성공 응답은 선택된 계정의 쿨다운을 해제합니다.

풀의 선택, 쿨다운, 쿼터 상태는 프로세스가 살아 있는 동안 설정 핫 리로드를 거쳐도 유지됩니다. 선제 로테이션으로 업스트림 제한을 피하지 못하는 경우에는 반응형 페일오버가 계속 동작합니다.

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

이 시작 검사는 OAuth bearer가 다른 오리진으로, 또는 평문으로 전송되는 것을 막습니다. HTTPS와 호스트 검사는 **루프백 호스트에서는 완화**됩니다(`localhost`, `127.0.0.1`, `[::1]` 등): 루프백 `base_url`은 평문 HTTP와 임의의 호스트를 쓸 수 있어 로컬 디버깅 프록시나 목(mock)이 트래픽을 받을 수 있습니다 — bearer가 운영자의 머신을 벗어날 수는 없습니다. 루프백이 아닌 호스트에는 항상 HTTPS + `anthropic.com`이 요구됩니다. 공유 배포에서는 `claude_oauth`가 게이트웨이 소유 자격 증명을 소비하므로 [`[server.auth]`](/ko/guides/shared-gateway/)도 함께 구성하세요.

## 남은 후속 작업

- **폭주 제어(storm-control):** 새로 전환된 계정의 동시성을 서서히 올리는 것은 이후 후속 작업으로 남아 있으며 구현되지 않았습니다.

구현 동작은 [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude)와 배포된 Claude Code 바이너리를 참고했습니다. shunt는 teamclaude에 대한 런타임 의존성이 없습니다.
