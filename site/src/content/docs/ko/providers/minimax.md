---
title: MiniMax
description: MINIMAX_API_KEY로 MiniMax-M3(1M 컨텍스트)를 MiniMax의 Anthropic 호환 엔드포인트로 라우팅하기.
---

**MiniMax**는 자신의 모델을 **Anthropic 호환** 엔드포인트를 통해 제공합니다 — shunt는 Claude Code의
Messages 요청을 그대로 전달하고 MiniMax API 키를 주입합니다. 내장 프리셋이 없으므로, 업스트림이
`kind`와 `base_url`을 명시적으로 선언합니다.

## 빠른 시작

코딩 에이전트가 대신 구성하도록 하세요 — 이름이 있는 블루프린트가 없는 프로바이더의 경우 `shunt add`가
일반 리서치 가이드에 문서 URL을 삽입합니다(오프라인·읽기 전용이며, 구성은 에이전트가 편집하고 이 명령은
절대 편집하지 않습니다):

```bash
shunt add upstream https://platform.minimax.io/docs/token-plan/claude-code --print | claude
```

또는 아래의 수동 단계를 따르세요.

## 업스트림 구성

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "minimax"
kind = "anthropic"
base_url = "https://api.minimax.io/anthropic"
auth = { mode = "api_key", env = "MINIMAX_API_KEY" }

[[routes]]
model = "MiniMax-M3"
provider = "minimax"
```

순서가 있는 `[[upstreams]]`는 shunt의 내장 프로바이더를 대체하므로, 구성은 여전히 폴백 대상인
`anthropic` 기본값을 선언해야 합니다(`server.default_provider`의 기본값은 `anthropic`입니다).

레거시 `[providers.minimax]` 테이블 형식도 계속 지원됩니다 — 다만 한 파일에서 `[[upstreams]]`와
`[providers.*]`를 섞지 마세요.

## 자격 증명

```bash
export MINIMAX_API_KEY='...'
```

키를 구성 파일에 절대 쓰지 마세요. `shunt check`는 구성의 구조를 검증하지만 키의 값을 읽지는 않습니다 —
`MINIMAX_API_KEY`가 설정되어 있지 않으면 `minimax`로 라우팅된 첫 요청이 인증 오류를 반환합니다.

## 모델

| 모델 id | 비고 |
| :-- | :-- |
| `MiniMax-M3` | 1M 토큰 컨텍스트이며, 클라이언트가 Claude Code의 `[1m]` 마커를 덧붙일 수 있습니다(`MiniMax-M3[1m]`, MiniMax 자체 [Claude Code 통합 문서](https://platform.minimax.io/docs/token-plan/claude-code)가 기술하는 슬러그) — shunt가 매칭 전에 이를 제거하므로 접미사가 없는 id를 라우팅하세요 |

Claude Code에서 라우팅된 id는 `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, 또는 서브에이전트의
`model:` 프론트매터로 선택하세요. 대신 `/model` 선택기에 항목을 드러내려면, `[models.upstream_model]`
맵과 함께 `claude` 프리픽스가 붙은 별칭을 광고하세요 —
[모델 디스커버리](/ko/guides/model-discovery/)를 참고하세요. 매핑된 id는 `[1m]`으로 끝나면 **안 됩니다** —
클라이언트가 매칭 전에 이 힌트를 제거하며, 이는 `MiniMax-M3[1m]`을 키로 하는 `[[routes]]` 항목도 도달할 수
없게 만들기 때문입니다. 그러니 언제나 접미사가 없는 `MiniMax-M3`을 라우팅하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `minimax`를 가리키는지 확인한 다음,
[Claude Code를 shunt로 지정](/ko/guides/connect-claude-code/)하세요.

## 서브에이전트 플러그인

[`shunt-minimax` 플러그인](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-minimax)은 위
모델을 위한 미리 만들어진 Claude Code 서브에이전트를 제공합니다:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-minimax@shunt
```
