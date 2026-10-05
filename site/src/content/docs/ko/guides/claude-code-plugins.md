---
title: Claude Code 플러그인
description: shunt의 Claude Code 플러그인 설치 — 프롬프트 위에 사용량을, /shunt:usage에 풀 여유를 보여주는 mod와, shunt가 우회시키는 모델을 위한 서브에이전트 번들.
---

shunt는 자체 Claude Code 플러그인 마켓플레이스를 제공합니다. 한 번만 추가하세요:

```
/plugin marketplace add pleaseai/shunt
```

여기에는 두 종류의 플러그인이 있습니다. 하나는 **`shunt` mod** — 프롬프트 위 밴드에 사용량(shunt에서는 게이트웨이 풀, 그 밖에서는 세션 자체)을 보여주고, 명령으로 풀의 상세 내역을 보여줍니다. 다른 하나는 **프로바이더 번들** — shunt가 다른 프로바이더로 우회시키는 모델에서 실행되는 서브에이전트를 추가합니다.

## `shunt` mod: `/shunt:usage`

```
/plugin install shunt@shunt
```

`/shunt:usage`는 게이트웨이의 [`GET /usage`](/ko/reference/endpoints/) 엔드포인트에서 읽은, 공유 계정 풀의 남은 여유를 출력합니다:

```
shunt: pool — degraded   http://127.0.0.1:3001

  5h    ▓▓▓▓▓▓░░░░  62% left   resets 04:11
  7d    ▓▓▓▓▓▓▓▓░░  81% left   resets Sun 01:11
  fable ▓▓░░░░░░░░  19% left   resets Sun 01:11

  claude  ok         5h  71%  7d  84%  fable  19%
  codex   exhausted  5h   0%  7d  40%  fable    —

  headroom left, averaged over the pool's accounts; a shared figure, not a promise about your next request
```

mod가 명령에 직접 답하므로, 모델로는 아무것도 전송되지 않고 답변에 토큰이 들지 않습니다.

### 사용량 밴드

mod는 사용량을 프롬프트 위 밴드에도 계속 띄워 두므로, 따로 묻지 않아도 볼 수 있습니다. shunt 게이트웨이에서는 `shunt` 태그를 붙여 풀의 사용량을 보여줍니다:

```
shunt · 5H 5% ↻1h 41m · WK 46% ↻1d 7h · Fable 31% ↻1d 7h ⚠ anthropic degraded
```

각 수치는 해당 창을 **쓴 양**입니다. 명령이 여유로 보고하는 풀 전체 평균의 `1 - remaining`이며(아래 「수치 읽기」 참고), 그 뒤에 해당 창에서 가장 이른 리셋까지 남은 시간이 붙습니다. 남은 양이 아니라 쓴 양으로 세는 이유는, Claude Code 자체의 `/usage`나 shunt가 아닐 때의 밴드와 같은 방식으로 읽히게 하기 위해서입니다. 수치는 70% 이상이면 주황색, 90% 이상이면 빨간색입니다. 상태가 `ok`가 아닌 풀링 프로바이더는 끝에 표시하며, `degraded`는 주황색, `exhausted`와 `capped`는 빨간색입니다. 프로바이더 목록이 없으면 풀 자체의 상태를 표시합니다. 어떤 계정도 보고하지 않는 창은 생략합니다.

shunt가 아닐 때는 태그 없이 세션 자체의 Claude rate limit을 보여줍니다. 게이트웨이가 설정되지 않은 세션, `GET /usage`에 404나 풀 보고가 아닌 응답을 주는 게이트웨이, 풀링하는 프로바이더가 없는 게이트웨이가 여기에 해당합니다:

```
5H 5% ↻1h 41m · WK 46% ↻1d 7h
```

shunt에서는 세션이 시작될 때, 그 뒤로 1분마다, 그리고 턴이 끝날 때마다 `GET /usage`를 읽습니다. 이때 세션 자체의 limit은 쓰지 않습니다. 마지막으로 응답한 풀 계정 하나의 값이기 때문입니다. 토큰 거부, 게이트웨이 연결 실패, 그 밖의 오류 상태는 shunt에서 생긴 장애이므로 다른 값으로 대체하지 않고 `shunt · ⚠ credential refused`처럼 표시합니다. 게이트웨이도 rate limit도 없으면(API 키로 Anthropic에 직접 보내는 경우) 밴드는 아무것도 그리지 않습니다.

세션 동안만 숨기려면 밴드의 `[-]`(ctrl+x ctrl+a)로 접습니다. 아예 끄려면 `/config`에서 플러그인의 **Usage band** 옵션(`usageBand`)을 끄세요. 그러면 mod는 아무것도 폴링하지 않고, `/shunt:usage`는 그대로 답합니다.

### 수치 읽기

`remaining`은 풀의 총 용량 중 아직 **쓰이지 않은** 비율입니다. `62%`는 여유가 62% 남았다는 뜻이지, 62%를 썼다는 뜻이 아닙니다. 해당 창을 보고하는 비활성화되지 않은 계정들에 대한 `mean(1 - utilization)`이므로, 소진된 계정 아홉 개에 새 계정 하나면 `100%`가 아니라 `10%`로 읽힙니다.

이는 풀 전체 집계이며, **예측이 아닙니다**. 라우팅은 가용성, 모델, 세션 어피니티, 우선순위도 함께 따지므로, 수치가 건강하다고 해서 다음 요청이 수락된다는 보장은 아닙니다.

| 창  | 포함 범위 |
| ------- | -------------- |
| `5h`    | 롤링 5시간 세션 창 |
| `7d`    | 공유 주간 창 |
| `fable` | Fable 범위의 주간 창 (`7d_oi`) |

비활성화되지 않은 계정 중 어느 것도 해당 창을 보고하지 않으면 그 창은 `—`로 표시됩니다. ChatGPT/Codex 계정은 `x-codex-*` 응답 헤더로 `5h`와 `7d`를 채우며, 자체적인 Fable 범위 신호는 없습니다.

첫 번째 블록은 풀링된 모든 프로바이더에 걸친 집계이고, 그 아래 행들은 풀링된 프로바이더별 동일한 집계이므로, 한 프로바이더로 라우팅된 세션은 혼합 수치 대신 그 프로바이더의 여유를 읽을 수 있습니다. 이 엔드포인트는 계정 이름, 개수, 우선순위, 계정별 수치를 절대 담지 않습니다 — 그 세부 정보는 관리자 전용 `GET /admin/api/pool` 뒤에 남습니다.

### 사전 요구 사항

1. [Claude Code 연결](/ko/guides/connect-claude-code/)에서처럼 Claude Code를 게이트웨이로 향하게 하세요:

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
   export ANTHROPIC_AUTH_TOKEN=<your client token>
   ```

2. 엔드포인트를 활성화하세요. `GET /usage`는 옵트인이며 [`[server.auth]`](/ko/guides/shared-gateway/)를 요구하므로, [구성](/ko/reference/configuration/)에 두 테이블이 모두 있어야 합니다:

   ```toml
   [server.auth]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

   `[server.auth]`는 토큰을 TOML이 아니라 환경 변수에서 읽습니다. 기본값은 `SHUNT_CLIENT_TOKENS`이며 `name:token` 쌍 형식이고, 이 변수가 설정되지 않으면 게이트웨이는 시작에 실패합니다. 위에서 `ANTHROPIC_AUTH_TOKEN`으로 설정한 것과 같은 토큰을 사용해, 게이트웨이가 실행되는 쪽에서 내보내세요:

   ```bash
   export SHUNT_CLIENT_TOKENS="claude-code:<your client token>"
   ```

   `[server.gateway]`로 로그인을 발급하는 게이트웨이는 `[server.auth]` 대신 그 테이블로 `[server.usage]`를 켤 수 있습니다. 그러면 `shunt gateway claude`로 시작한 세션은 클라이언트 토큰이 필요 없고, mod가 그 세션의 gateway 로그인으로 인증합니다(아래 참고).

3. 함수 훅을 활성화한 채로 Claude Code를 실행하세요 — 이 기능은 얼리 액세스입니다:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

3단계 없이는 밴드가 없고, 명령 자체는 존재하지만 직접 답하는 대신 모델에게 도구 호출로 엔드포인트를 읽도록 요청하는 방식으로 폴백합니다.

### 무엇을 읽는가

mod는 환경 변수 다섯 개를 읽고 아무것도 쓰지 않습니다. 기본적으로는 세션이 이미 모든 메시지를 보내고 있는 바로 그 게이트웨이로 세션 자신의 자격 증명을 보내므로, 세션이 이미 쓰고 있지 않던 호스트에는 도달하지 않습니다. `SHUNT_BASE_URL`만이 의도된 예외로, 직접 지정한 게이트웨이를 대신 바라보게 합니다. base URL이 아예 설정되어 있지 않으면 Anthropic 자체 API를 호출하는 대신 그 사실을 알립니다.

| 변수 | 용도 |
| -------- | ------- |
| `SHUNT_BASE_URL` | 게이트웨이 base URL; `ANTHROPIC_BASE_URL`을 재정의 |
| `ANTHROPIC_BASE_URL` | 이 세션이 이미 경유하고 있는 게이트웨이 |
| `SHUNT_TOKEN` | 클라이언트 토큰; 아래 둘을 모두 재정의하며 `Authorization: Bearer`로 전송 |
| `ANTHROPIC_AUTH_TOKEN` | Claude Code가 보내는 그대로 `Authorization: Bearer`로 전송 |
| `ANTHROPIC_API_KEY` | Claude Code가 보내는 그대로 `x-api-key`로 전송 |

`SHUNT_BASE_URL`은 트래픽을 다른 게이트웨이로 라우팅하면서 어느 한 게이트웨이의 풀을 읽을 수 있게 해 주는 수단입니다.

`shunt gateway claude`로 시작한 세션은 위 변수에 credential이 없습니다. launcher가 이 변수들을 지우고, 대신 `apiKeyHelper`를 `shunt gateway token`으로 연결하기 때문입니다. 그래서 위 변수가 하나도 없으면 mod는 합쳐진 Claude Code 설정을 읽고, `apiKeyHelper`가 shunt 자체의 `shunt gateway token`(이름만 쓰든 경로로 쓰든)이면 셸 없이 직접 실행해 출력된 gateway 로그인 토큰을 `Authorization: Bearer`로 보냅니다. 이 토큰은 5분 동안 재사용하고, 재사용한 토큰을 게이트웨이가 거부하면 helper를 한 번 더 실행합니다. 그 밖의 `apiKeyHelper`는 실행하지 않으므로, 그런 세션에서는 대신 `SHUNT_TOKEN`을 export하세요. 게이트웨이는 `GET /usage`에서 `[server.auth]` 클라이언트 토큰과 함께 gateway 로그인도 받습니다.

### 왜 `/usage`가 아니라 `/shunt:usage`인가

`/usage`는 Claude Code 자체의 내장 명령이고, 엔진은 플러그인이 내장 명령의 이름을 가져가도록 허용하지 않습니다. 대신 플러그인의 마크다운 명령에는 플러그인 이름으로 네임스페이스가 붙으므로, 이 명령은 `shunt:usage`로 제공되며 어떤 것과도 충돌하지 않습니다.

## 프로바이더 서브에이전트 플러그인

이들은 shunt가 다른 프로바이더로 라우팅하는 모델 id에 고정된 서브에이전트를 추가합니다. 세션은 Claude Code의 하니스 안에서 계속 실행되며 — 같은 도구, 같은 스킬 — 토큰 생성만 우회됩니다.

| 플러그인 | 모델 | 설정 |
| ------ | ------ | ----- |
| `shunt-codex` | GPT-6 Sol · Luna, GPT-5.6 Sol · Terra · Luna | [ChatGPT / Codex](/ko/guides/codex/) |
| `shunt-xai` | Grok 4.6 · 4.5 · Build | [xAI / Grok](/ko/guides/xai/) |
| `shunt-kimi` | Kimi K2.7 Code · K3 | [Kimi](/ko/providers/kimi/) |
| `shunt-deepseek` | DeepSeek V4 Pro · Flash | [DeepSeek](/ko/providers/deepseek/) |
| `shunt-zai` | GLM 5.2 · 4.7 | [Z.ai](/ko/providers/zai/) |
| `shunt-minimax` | MiniMax-M3 | [MiniMax](/ko/providers/minimax/) |
| `shunt-mimo` | MiMo V2.5 Pro | [MiMo](/ko/providers/mimo/) |

설치 방법도 같습니다:

```
/plugin install shunt-codex@shunt
```

각 플러그인은 게이트웨이 구성에서 해당 모델 id들이 그 프로바이더로 라우팅되어 있어야 합니다. 그렇지 않으면 Claude Code가 모델 id를 Anthropic으로 곧장 보내고 요청은 실패합니다.

## 주의 사항

함수 훅은 얼리 액세스입니다. 훅 모듈은 함수 훅이 활성화된 곳에서만 로드되며, `shunt` mod가 대상으로 삼은 API는 Claude Code 릴리스 사이에 예고 없이 바뀔 수 있습니다. 프로바이더 번들은 함수 훅을 사용하지 않으므로 영향을 받지 않습니다.
