---
title: Standard Compute
description: STANDARDCOMPUTE_API_KEY로 매핑된 모델을 Standard Compute의 Messages 엔드포인트로 라우팅하기.
---

**Standard Compute**는 `https://api.stdcmpt.com/v1`에서 OpenAI 호환 API를 제공하는 LLM
게이트웨이입니다. `/v1/messages`에서 **Anthropic Messages** 엔드포인트도 제공하므로, shunt는
Claude Code의 Messages 요청을 그대로 전달하고 키를 주입합니다. 내장된 `standardcompute`
preset은 `kind = "anthropic"`, `base_url = "https://api.stdcmpt.com"` 및
`STANDARDCOMPUTE_API_KEY`의 API 키 인증을 제공합니다.

## 빠른 시작

코딩 에이전트에게 맡기세요. `shunt add`는 내장된 설정 블루프린트를 출력합니다(오프라인, 읽기 전용. 구성을 편집하는 건 에이전트고 명령은 아무것도 바꾸지 않습니다):

```bash
shunt add upstream standardcompute --print | claude
```

또는 아래 수동 단계를 따르세요.

## 업스트림 구성

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 라우트가 없는 모델(예: claude-*)의 기본값으로 Anthropic을 유지

[[upstreams]]
name = "standardcompute"
provider = "standardcompute"

[[routes]]
model = "StandardCompute"
provider = "standardcompute"
```

순서가 있는 `[[upstreams]]`는 shunt의 내장 provider를 대체하므로, 구성이 여전히 폴백하는
`anthropic` 기본값을 선언해야 합니다(`server.default_provider`의 기본값은 `anthropic`).

## 자격 증명

```bash
export STANDARDCOMPUTE_API_KEY='...'
```

[standardcompute.com](https://standardcompute.com)에서 키를 만드세요. 키를 구성 파일에 쓰지
마세요. `shunt check`는 구성 구조를 검증할 뿐 키 값을 읽지 않습니다.
`STANDARDCOMPUTE_API_KEY`가 설정되지 않았다면 `standardcompute`로 라우팅된 첫 요청은 인증
오류를 반환합니다.

## 모델

| 모델 ID | 참고 |
| :-- | :-- |
| `StandardCompute` | 라우팅 모델: 각 요청을 처리할 수 있는 가장 저렴한 활성 모델로 보내며 자동 폴백 |

Claude Code에서 `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, 또는 서브에이전트의
`model:` frontmatter로 라우팅된 ID를 선택하세요. 대신 `/model` 선택기에 노출하려면
`[models.upstream_model]` 맵과 함께 `claude` 프리픽스가 붙은 별칭을 선언하세요.
[모델 디스커버리](/ko/guides/model-discovery/)를 참고하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"StandardCompute","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `standardcompute`를 가리키는지 확인한 뒤
[Claude Code를 shunt에 연결](/ko/guides/connect-claude-code/)하세요.
