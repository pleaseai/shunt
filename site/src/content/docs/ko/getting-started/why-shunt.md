---
title: 왜 shunt인가
description: shunt란 무엇이며, 다른 Claude Code 프록시와 어떻게 다르고, 언제 사용하는가.
---

`shunt`는 스펙을 준수하는 [Claude Code LLM 게이트웨이](https://code.claude.com/docs/en/llm-gateway-protocol)입니다. **매핑한 모델**에 한해 추론을 **추론 계층**에서 다른 LLM 프로바이더로 우회시키는 투명 프록시입니다. 요청의 `model` id를 기준으로 라우팅하며, 기본적으로 그 외 모든 것은 변경 없이 Anthropic으로 그대로 전달됩니다(이것이 "shunt"이며, 폴백은 `server.default_provider`로 구성할 수 있습니다).

이름 자체가 동작 방식을 나타냅니다. 전기/철도의 *shunt*는 흐름의 일부를 선택해 병렬 경로로 우회시킵니다. 여기서는 매핑된 모델의 추론이 다른 프로바이더로 우회되는 동안 Claude Code의 도구와 스킬은 그대로 유지됩니다.

## 동작 방식

Claude Code는 모든 턴을 Anthropic API로 보냅니다. `shunt`는 그 앞(`ANTHROPIC_BASE_URL`을 통해)에 위치하여, 매핑한 모델에 한해 추론을 다른 프로바이더(OpenAI, Codex/ChatGPT 등)로 우회시킵니다. 라우팅이 HTTP/추론 계층에서 일어나며 작업을 다른 CLI로 넘기는 것이 아니기 때문에, 세션은 계속 Claude Code의 하네스 안에서 실행됩니다. 동일한 도구 루프, 동일하게 프리로드된 스킬, 동일한 번들 스크립트 경로 해석이 유지됩니다. 오직 토큰 생성만 외주됩니다.

이는 서브에이전트를 다른 런타임(예: Codex CLI)으로 넘기는 방식과 대조됩니다. 그 방식은 스택의 더 위쪽을 끊어내어 페르소나와 프리로드된 스킬을 잃습니다.

## 에이전트별이 아닌 모델별 — 그리고 전역 교체가 아님

대부분의 Claude Code 프록시는 **모든** 트래픽을 하나의 대체 프로바이더로 라우팅합니다(전역 모델 교체). `shunt`의 초점은 요청의 `model` id로 결정되는 **선택적, 모델별** 우회입니다. 메인 세션은 Claude에 두고, 지정한 모델만 다른 프로바이더로 우회합니다.

선택성은 Claude Code 자체에서 결정되며, Claude Code는 이미 컨텍스트별로 모델을 선택할 수 있게 해줍니다.

- 메인 세션의 `/model` 선택기,
- 서브에이전트 정의의 `model:` 프론트매터,
- 모든 서브에이전트에 대한 `CLAUDE_CODE_SUBAGENT_MODEL`,
- 선택기에 커스텀 항목을 추가하는 `ANTHROPIC_CUSTOM_MODEL_OPTION`.

shunt는 받은 model id만 그대로 존중합니다. 취약한 에이전트별 시스템 프롬프트 지문 인식은 없습니다. 그 동일한 선택성이 shunt가 호출자가 누구인지 조사하지 않고도 개별 에이전트까지 도달합니다.

## shunt가 구현하는 것

- **`POST /v1/messages`** — 요청의 `model` id에 따라 라우팅되는 추론. 매핑되지 않은 모델은 호출자 본인의 자격 증명으로 바이트 단위 그대로 Anthropic에 전달됩니다.
- **Anthropic Messages ⇄ OpenAI Responses 변환** — 매핑된 OpenAI 계열 모델에 대해 스트리밍을 포함하여 변환합니다.
- **ChatGPT 구독 재사용** — `codex` 프로바이더는 Codex CLI의 `~/.codex/auth.json` 로그인을 재사용(및 자동 갱신)합니다.
- **`GET /v1/models`** — Claude 이름 별칭에 대한 [모델 디스커버리](/ko/guides/model-discovery/).
- **토큰 카운팅** — 변환 프로바이더에 대한 로컬 tiktoken 카운트, 패스스루에 대한 정확한 업스트림 카운트.
- **스트리밍 복원력** — Cloudflare 같은 프록시가 긴 추론 구간을 끊지 않도록 하는 [SSE keepalive ping](/ko/guides/shared-gateway/#sse-keepalive-pings).
- **선택적 인바운드 인증** — 공유 배포를 위한 [클라이언트별 토큰](/ko/guides/shared-gateway/).

사용해 볼 준비가 되셨나요? [설치](/ko/getting-started/installation/)로 이동하세요.
