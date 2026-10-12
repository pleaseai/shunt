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

model id 하나를 스스로 판단하게 만들 수도 있습니다. [스테이지 라우터](/ko/guides/stage-router/)는 강한 티어와 효율 티어를 지정하고, 대화의 최근 tool-result 메타데이터(`tool_use.name`과 `tool_result.is_error`이며 프롬프트 텍스트는 절대 아닙니다)로 턴마다 둘 중 하나를 고릅니다. 또 어떤 항목이든 [`[models.subagents]`](/ko/reference/configuration/#modelssubagents-선택) 오버레이를 실어 위임된 작업만 다른 타깃으로 보낼 수 있고, 부모 세션은 자기 목적지를 유지합니다. 둘 다 설정하지 않으면 동작은 그대로입니다.

## NVIDIA Switchyard 기반 라우팅

스테이지 라우터와 판정자 기반 라우터는 shunt가 직접 고안한 휴리스틱이 아닙니다. NVIDIA [Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) 프로젝트의 라우팅 라이브러리인 [`switchyard-libsy`](https://github.com/NVIDIA-NeMo/Switchyard/tree/3ddea9d30174ad835cf505a93617ba251c8eb8dd/crates/libsy)를 토대로 합니다. Switchyard는 "각 LLM 호출을 그 일을 해낼 수 있는 가장 저렴한 모델로" 라우팅하는 프로젝트입니다. 덕분에 얻는 이점은 다음과 같습니다.

- **공개된 결과가 있는 스코어러.** shunt는 Switchyard의 스테이지 스코어러를 수정 없이 사용합니다. 업스트림의 Terminal-Bench 2.1 실행에서 스테이지 라우팅은 Opus 4.8 베이스라인 정확도의 95.7%를 유지하면서 비용을 30.5% 줄였습니다. 비교 대상인 단일 고정 모델 네 개는 모두 56% 미만이었습니다. 이 수치는 shunt가 아니라 업스트림이 낸 것입니다. shunt는 이 벤치마크를 돌린 적이 없고 신호도 자체 방식으로 추출합니다. 따라서 설정하려는 모델 쌍에 대한 보장이 아니라 이 접근 방식이 효과가 있다는 근거로 읽어야 합니다.
- **여러 가지 알고리즘.** shunt는 스테이지 채점 외에도 Switchyard의 다른 알고리즘을 제공하며, 모두 모델 항목 단위로 켭니다. 항목마다 라우터 타입을 하나 고릅니다. `stage_router`(핸드오프 노트를 더할 수 있음), `llm_classifier`(`escalation` 모드가 에스컬레이션 라우팅), `composite`, `advisor` 같은 타입이 있습니다. 서브에이전트 라우팅은 이와 별개로 `[models.subagents]` 테이블로 설정합니다.
- **Claude Code 세션에 맞춘 조정.** Switchyard는 프로바이더 중립적입니다. shunt는 Claude Code 게이트웨이에 필요한 것을 더합니다. Claude Code의 실제 도구 이름을 그대로 매칭합니다. 세션의 티어는 비대칭 히스테리시스로 고정해 긴 세션이 턴마다 티어를 바꾸다가 이미 쌓아 둔 프롬프트 캐시를 잃지 않게 합니다. 위임된 서브에이전트의 라우팅은 부모와 따로 둡니다.
- **일반 model id로 남는 타깃.** 라우팅된 타깃은 shunt의 기본 라우팅 경로로 다시 들어가므로 자체 페일오버 체인, 계정 풀, 어댑터를 그대로 씁니다. 클라이언트가 보는 model id도 요청한 그대로입니다.
- **적은 부담.** 이 의존성이 릴리스 바이너리에 더한 크기는 약 0.3%입니다. 검토한 업스트림 리비전에 고정해 두었으므로 업그레이드도 검토를 거친 diff로 반영됩니다. `[models.router]`와 `[models.subagents]` 테이블이 모두 없는 항목은 이전과 똑같이 라우팅됩니다.
- **별도 서버 없는 라우팅.** Switchyard의 [Server Path](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/getting_started.md#server-path)는 `switchyard-server`를 독립 프록시로 따로 띄웁니다. Claude Code처럼 Anthropic Messages로 요청하는 클라이언트라면 같은 route 타입(`auto`, `stage_router`, `llm_classifier`)을 shunt의 `[models.router]`에서 그대로 쓸 수 있으므로 프록시를 하나 더 둘 필요가 없습니다. 다만 shunt는 Anthropic Messages 요청(`/v1/messages`와 그 `/v1/messages/count_tokens` 프로브)에만 라우터를 적용합니다. OpenAI Chat Completions나 Responses 클라이언트의 라우팅은 shunt가 다루지 않습니다.

shunt가 Switchyard에서 가져온 것과 두고 온 것, 벤치마크를 읽을 때 주의할 점, 알고리즘의 단계별 동작은 [Switchyard 통합](/ko/guides/switchyard/)에서 다룹니다.

## shunt가 구현하는 것

- **`POST /v1/messages`** — 요청의 `model` id에 따라 라우팅되는 추론. 매핑되지 않은 모델은 호출자 본인의 자격 증명으로 바이트 단위 그대로 Anthropic에 전달됩니다. 단, shunt가 다른 프로바이더용으로 생성한 [`thinking` signature](/ko/providers/anthropic/)만은 Anthropic이 거부하는 값이므로 제거됩니다.
- **Anthropic Messages ⇄ OpenAI Responses 변환** — 매핑된 OpenAI 계열 모델에 대해 스트리밍을 포함하여 변환합니다.
- **ChatGPT 구독 재사용** — `codex` 프로바이더는 Codex CLI의 `~/.codex/auth.json` 로그인을 재사용(및 자동 갱신)합니다.
- **`GET /v1/models`** — Claude 이름 별칭에 대한 [모델 디스커버리](/ko/guides/model-discovery/).
- **토큰 카운팅** — 변환 프로바이더에 대한 로컬 tiktoken 카운트, 패스스루에 대한 정확한 업스트림 카운트.
- **스트리밍 복원력** — Cloudflare 같은 프록시가 긴 추론 구간을 끊지 않도록 하는 [SSE keepalive ping](/ko/guides/shared-gateway/#sse-keepalive-ping).
- **선택적 인바운드 인증** — 공유 배포를 위한 [클라이언트별 토큰](/ko/guides/shared-gateway/).

사용해 볼 준비가 되셨나요? [설치](/ko/getting-started/installation/)로 이동하세요.
