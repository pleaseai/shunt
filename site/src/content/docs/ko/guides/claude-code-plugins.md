---
title: Claude Code 플러그인
description: shunt의 Claude Code 플러그인 설치 — 풀 여유를 보여주는 /shunt:usage mod와, shunt가 우회시키는 모델을 위한 서브에이전트 번들.
---

shunt는 자체 Claude Code 플러그인 마켓플레이스를 제공합니다. 한 번만 추가하세요:

```
/plugin marketplace add pleaseai/shunt
```

여기에는 두 종류의 플러그인이 있습니다. 하나는 **`shunt` mod** — 게이트웨이 자체의 풀 사용량을 보고하는 명령을 추가합니다. 다른 하나는 **프로바이더 번들** — shunt가 다른 프로바이더로 우회시키는 모델에서 실행되는 서브에이전트를 추가합니다.

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
   tokens = ["<your client token>"]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

3. 함수 훅을 활성화한 채로 Claude Code를 실행하세요 — 이 기능은 얼리 액세스입니다:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

3단계 없이도 명령 자체는 존재하지만, 직접 답하는 대신 모델에게 도구 호출로 엔드포인트를 읽도록 요청하는 방식으로 폴백합니다.

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

### 왜 `/usage`가 아니라 `/shunt:usage`인가

`/usage`는 Claude Code 자체의 내장 명령이고, 엔진은 플러그인이 내장 명령의 이름을 가져가도록 허용하지 않습니다. 대신 플러그인의 마크다운 명령에는 플러그인 이름으로 네임스페이스가 붙으므로, 이 명령은 `shunt:usage`로 제공되며 어떤 것과도 충돌하지 않습니다.

## 프로바이더 서브에이전트 플러그인

이들은 shunt가 다른 프로바이더로 라우팅하는 모델 id에 고정된 서브에이전트를 추가합니다. 세션은 Claude Code의 하니스 안에서 계속 실행되며 — 같은 도구, 같은 스킬 — 토큰 생성만 우회됩니다.

| 플러그인 | 모델 | 설정 |
| ------ | ------ | ----- |
| `shunt-codex` | GPT-5.6 Sol · Terra · Luna | [ChatGPT / Codex](/ko/guides/codex/) |
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
