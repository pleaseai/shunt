---
title: OpenRouter
description: 매핑된 모델을 OpenRouter의 Anthropic 호환 엔드포인트로 라우팅하기 — API 키 하나, 수백 개의 모델.
---

**OpenRouter**는 여러 모델 벤더를 API 키 하나 뒤에 모아 두고 **Anthropic 호환** 엔드포인트를
노출합니다 — shunt는 OpenRouter 키를 주입하고 Claude Code의 Messages 요청을 전달합니다.
Anthropic이 아닌 슬러그(`stealth/ox-alpha`, …)에 대해서는 OpenRouter의 스킨이 HTTP 400으로 거부하는
지연 도구 필드(`defer_loading`, `tool_search_tool_*`)를 제거하며, Anthropic id(`claude*`,
`anthropic/*`)는 그 필드를 유지합니다. 내장 프리셋이 없으므로, 업스트림이 `kind`와 `base_url`을
명시적으로 선언합니다.

## 빠른 시작

코딩 에이전트가 대신 구성하도록 하세요 — 이름이 있는 블루프린트가 없는 프로바이더의 경우 `shunt add`가
일반 리서치 가이드에 문서 URL을 삽입합니다(오프라인·읽기 전용이며, 구성은 에이전트가 편집하고 이 명령은
절대 편집하지 않습니다):

```bash
shunt add upstream https://openrouter.ai/docs --print | claude
```

또는 아래의 수동 단계를 따르세요.

## 업스트림 구성

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 라우트가 없는 모델(예: claude-*)의 기본값으로 Anthropic을 유지

[[upstreams]]
name = "openrouter"
kind = "anthropic"
base_url = "https://openrouter.ai/api"
auth = { mode = "api_key", env = "OPENROUTER_API_KEY" }

[[routes]]
model = "anthropic/claude-opus-4.8"
provider = "openrouter"
```

순서가 있는 `[[upstreams]]`는 shunt의 내장 프로바이더를 대체하므로, 구성은 여전히 폴백 대상인
`anthropic` 기본값을 선언해야 합니다(`server.default_provider`의 기본값은 `anthropic`입니다).

레거시 `[providers.openrouter]` 테이블 형식도 계속 지원됩니다 — 다만 한 파일에서 `[[upstreams]]`와
`[providers.*]`를 섞지 마세요.

## 자격 증명

```bash
export OPENROUTER_API_KEY='...'
```

키를 구성 파일에 절대 쓰지 마세요. `shunt check`는 구성의 구조를 검증하지만 키의 값을 읽지는 않습니다 —
`OPENROUTER_API_KEY`가 설정되어 있지 않으면 `openrouter`로 라우팅된 첫 요청이 인증 오류를 반환합니다.

## 모델

OpenRouter 모델 id는 `vendor/model` 슬러그입니다(예: `anthropic/claude-opus-4.8`) —
[OpenRouter 모델 카탈로그](https://openrouter.ai/models)를 둘러보고 도달 가능하게 하고 싶은 슬러그마다
`[[routes]]` 항목을 하나씩 추가하세요. Claude Code에서 라우팅된 id는 `ANTHROPIC_MODEL`,
`ANTHROPIC_CUSTOM_MODEL_OPTION`, 또는 서브에이전트의 `model:` 프론트매터로 선택하세요. 대신 `/model`
선택기에 항목을 드러내려면, `[models.upstream_model]` 맵과 함께 `claude` 프리픽스가 붙은 별칭을
광고하세요 — [모델 디스커버리](/ko/guides/model-discovery/)를 참고하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"anthropic/claude-opus-4.8","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `openrouter`를 가리키는지 확인한 다음,
[Claude Code를 shunt로 지정](/ko/guides/connect-claude-code/)하세요.
