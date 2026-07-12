---
title: 노력 & 컨텍스트
description: 매핑된 모델에 대해 추론 노력, 토큰 카운팅, 컨텍스트 표시기가 동작하는 방식.
---

## 추론 노력

Claude Code의 노력 레벨(`/effort`, `/model` 슬라이더, `--effort`, 또는 `CLAUDE_CODE_EFFORT_LEVEL`)은 `output_config.effort` 요청 필드로 전송되며, shunt는 이를 매핑된 모델에 대해 Responses `reasoning.effort`로 매핑합니다:

| Claude Code 노력 | → `reasoning.effort` |
| :-- | :-- |
| `low` / `medium` / `high` / `xhigh` | 패스스루 |
| `max` | 받아들이는 모델(**gpt-5.6** 계열)에서는 패스스루, 그 외에는 `xhigh`로 접힘 |

Codex 슬러그가 어떤 추론 레벨을 받아들이는지는 openai/codex의 [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)에 모델별로(`supported_reasoning_levels`) 나열되어 있습니다.

:::note[커스텀 모델 id에는 플래그가 필요]
`gpt-5.6-sol` 같은 커스텀 게이트웨이 id에 대해서는 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`을 설정해야 합니다 — 그렇지 않으면 Claude Code는 노력 지원으로 인식하지 못하는 모델 id에 대해 `output_config.effort`를 생략하고, shunt는 `medium`으로 폴백합니다.

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```
:::

shunt에서의 우선순위: 구성의 `route.effort` / `[providers.*].effort` 오버라이드가 먼저 이깁니다; 그렇지 않으면 요청의 `output_config.effort`가 존중됩니다; 그렇지 않으면 `thinking.enabled → high`, 그다음 모델 이름 접미사(`-xhigh`/`-high`/`-medium`/`-low`, `-spark`는 `-low`로 취급), 그 외에는 `medium`입니다.

## 토큰 카운팅 (`count_tokens`)

**Anthropic으로 라우팅된** 모델에 대해 shunt는 `POST /v1/messages/count_tokens`를 업스트림으로 패스스루합니다(정확한 카운트). **`responses`로 라우팅된** 모델에는 동등한 업스트림 엔드포인트가 없으므로, 프로바이더의 `count_tokens` 설정이 결정합니다:

- **`count_tokens = "tiktoken"`(기본)** — shunt는 tiktoken의 `o200k_base` 인코더로 로컬에서 카운트를 계산하여 `{"input_tokens": N}`을 반환합니다. GPT 계열 모델의 텍스트에 대해 거의 정확하며, 프로세스 내에서(~ms) 응답됩니다 — 이는 Claude Code의 `/context`가 표시되는 항목당 하나의 `count_tokens` 호출(호출당 30–50개)을 발생시키기 때문에 중요합니다.
- **`count_tokens = "estimate"`(옵트인)** — shunt는 **404**를 반환하며, 이는 게이트웨이 프로토콜이 명시적으로 허용합니다. 그러면 메인 루프 컨텍스트 바는 로컬에서 추정하지만, `/context`는 모든 카테고리 카운트를 네트워크를 통해 Haiku에 대해 다시 실행합니다 — 느리고, Anthropic 자격 증명이 없으면 조용히 0 토큰으로 보고됩니다.

어느 쪽이든 요청은 responses 어댑터에 도달하지 않으므로, 카운트 요청이 전체 추론 호출로(그리고 그에 대한 과금으로) 바뀌는 일은 절대 없습니다.

## 매핑된 모델의 컨텍스트 / 사용량 표시

Claude Code는 어시스턴트 메시지의 토큰 `usage`를 모델의 컨텍스트 윈도우 크기로 나누어 로컬에서 컨텍스트 표시기를 계산합니다. `responses` 프로바이더로 라우팅된 모델의 경우:

- **토큰 카운트(분자)는 정확합니다.** shunt는 Responses `usage`에서 `input_tokens`(및 캐시된 토큰)를 읽어 Anthropic `message_delta`로 전달하며, 캐시된 부분을 `cache_read_input_tokens`로 분리합니다.
- **윈도우(분모)는 인식되지 않는 id에 대해 고정된 200k로 기본 설정됩니다.** 더 큰 실제 윈도우를 가진 모델(예: 372k의 `gpt-5.6-sol`)은 보수적으로 과다 보고된 백분율을 표시합니다 — 이는 자동 압축이 약간 일찍 발동하게 할 뿐입니다.

200k 기본값은 클라이언트 측에서 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`로 오버라이드할 수 있으며(Claude Code 2.1.205+), `claude-`로 시작하지 **않는** 모든 모델 id에 적용됩니다:

```bash
# 예: gpt-5.6-sol의 실제 윈도우
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

오버라이드는 `claude-`로 시작하지 **않는** id에만 적용되므로, [디스커버리 별칭](/ko/guides/model-discovery/)(*반드시* `claude-`로 시작해야 함)은 이를 받을 수 없습니다 — 그 윈도우는 200k 기본값에 고정됩니다. 선택기에서는 편리하지만, 정확한 윈도우가 필요할 때는 비-`claude-` id를 사용하세요(`ANTHROPIC_CUSTOM_MODEL_OPTION`을 통하거나 [티어 별칭 재매핑](/ko/guides/codex/#remap-the-tier-aliases-to-codex)을 통해). 두 매핑된 티어가 윈도우를 공유할 때 — `gpt-5.6-sol`과 `gpt-5.6-luna`는 둘 다 372k — 하나의 전역 값이 둘 다 커버합니다.

:::caution
이 값은 **전역**입니다 — 세션의 모든 비-`claude-` 모델에 대한 하나의 값 — 실제 업스트림 윈도우보다 크게 설정하면 요청이 실제 한계를 오버플로할 때까지 자동 압축이 지연됩니다. shunt는 [그 오버플로 오류를 다시 써서](#context-overflow-recovery) Claude Code가 자동으로 압축하고 재시도하도록 하지만, 각 오버플로 왕복은 낭비되는 지연입니다 — 매핑된 모델 중 가장 작은 실제 윈도우에 값을 맞추세요.

`gpt-5.6-sol`(실제 윈도우 372k)에 대한 실시간 검증된 경계: 365k 입력 토큰은 정상적으로 응답합니다; 372k+에서 스트리밍 요청은 자동 압축을 유발하는 `prompt is too long` 오류를 반환합니다(`gpt-5.5`는 272000). *비*스트리밍 요청은 대신 `input_tokens: 0`인 빈 `200`으로 저하되지만, Claude Code의 메인 루프는 항상 스트리밍합니다.
:::

또 다른 클라이언트 측 레버는 `[1m]` 모델 id 접미사로, 1M 윈도우를 강제합니다 — 업스트림이 실제로 그 윈도우를 가진 경우에만 사용하세요.(shunt는 라우트 일치 및 전달 전에 후행 `[1m]`을 제거하므로, 이 힌트는 순수하게 클라이언트 측으로 남고 프로바이더는 이를 절대 보지 않습니다.)

| 필드 | 매핑된(`responses`) 모델 | Claude 패스스루 |
| :-- | :-- | :-- |
| 사용된 컨텍스트 토큰 | ✅ 정확(shunt가 전달) | ✅ 정확 |
| 컨텍스트 윈도우(분모) | ⚠️ 200k 기본값; `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 설정 | ✅ 정확 |
| `count_tokens`(사전 요청) | ⚠️ 로컬 tiktoken 카운트(기본) | ✅ 정확(업스트림) |
| `rate_limits`(5시간 / 주간) | ❌ Anthropic 헤더 필요 | ✅ 표시됨 |

## 컨텍스트 오버플로 복구

대화가 업스트림 모델의 실제 윈도우를 넘어서면, 프로바이더는 자체 표현으로 요청을 거부합니다 — OpenAI의 `context_length_exceeded`, `"This model's maximum context length is N tokens…"`, 또는 프록시의 `"prompt token count of N exceeds the limit of M"`. Claude Code의 자동 압축-재시도는 Anthropic의 표현에서만 발동하므로, 다시 쓰지 않으면 이런 오류는 수동 `/compact`까지 세션을 멈춰 세웁니다([문서화된 게이트웨이 함정](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors)).

shunt는 `responses`로 라우팅된 모델의 컨텍스트 오버플로 오류를 감지하여 Claude Code가 일치시키는 Anthropic 형태로 다시 씁니다:

```json
{"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long: 372982 tokens > 272000 maximum"}}
```

업스트림 메시지가 두 토큰 카운트를 모두 담고 있으면, shunt는 이를 보존합니다(업스트림이 진술한 순서가 무엇이든) — Claude Code는 `N tokens > M maximum` 간격을 파싱하여 단일 재시도로 초과분 전체를 넘어 압축합니다. 업스트림이 카운트를 주지 않으면(예: Responses API의 단순한 *"Your input exceeds the context window of this model"*), shunt는 `prompt is too long`만 방출하며, 이것도 여전히 압축을 유발합니다. 오버플로가 아닌 오류는 원래 메시지와 함께 패스스루됩니다.

## 어트리뷰션 블록

Claude Code는 시스템 프롬프트 앞에 어트리뷰션 줄을 추가합니다. Anthropic은 처리 전에 이를 제거하지만, shunt는 이를 변경 없이 전달하므로 매핑된 프로바이더는 이를 `instructions`의 첫 줄로 받습니다. 해롭지는 않지만 Anthropic이 아닌 모델에는 무의미한 노이즈입니다. 제거하려면:

```bash
export CLAUDE_CODE_ATTRIBUTION_HEADER=0
```

이는 전역이므로 Anthropic 패스스루 트래픽(비용 추적에 사용)에서도 어트리뷰션을 제거합니다 — 다른 프로바이더로 라우팅할 때는 괜찮습니다.
