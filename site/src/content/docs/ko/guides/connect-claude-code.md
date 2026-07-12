---
title: Claude Code 연결
description: Claude Code를 shunt로 향하게 하고, 올바른 Anthropic 자격 증명을 선택하고, 매핑된 모델을 고르기.
---

공식 [Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect) 가이드를 기반으로 합니다 — shunt가 *바로* 연결하는 게이트웨이입니다.

## 1. Claude Code를 shunt로 향하게 하기

실행 중인 게이트웨이(기본 bind `127.0.0.1:3001`)로 base URL을 설정하세요. 셸에서 하거나 [설정 파일](https://code.claude.com/docs/en/settings)의 `env` 블록에 영속화하세요:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
```

```json
// ~/.claude/settings.json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3001"
  }
}
```

기존 Anthropic 자격 증명을 유지하세요 — shunt는 매핑하지 않은 모든 모델에 대해 이를 **변경 없이** `api.anthropic.com`으로 전달하므로, 매핑되지 않은 모델은 이전과 완전히 동일하게 동작합니다. 매핑된 모델의 프로바이더 자격 증명은 shunt 자체가 주입하며, Claude Code는 이를 절대 보내지 않습니다.

## 2. Anthropic 자격 증명 선택

Claude Code가 shunt로 보내는 자격 증명은 두 가지 역할을 합니다: **Claude 패스스루 모델**을 인증하고, **[모델 디스커버리](/ko/guides/model-discovery/)를 게이팅**합니다 — Claude Code는 `ANTHROPIC_AUTH_TOKEN`, API 키, 또는 `apiKeyHelper`가 설정되어 있을 때만 `GET /v1/models` 요청을 보냅니다. 매핑된 모델(`gpt-*` 등)은 어느 쪽이든 영향을 받지 않습니다.

| 자격 증명 | 토큰 갱신 | 디스커버리 | Claude 패스스루 | 과금 |
| :-- | :-- | :-- | :-- | :-- |
| claude.ai OAuth **로그인**만 | 자동 | ❌ 절대 발동 안 함 | ✅ | 구독 |
| `claude setup-token`의 `ANTHROPIC_AUTH_TOKEN` — **권장** | 불필요(1년 토큰) | ✅ | ✅ | 구독 |
| `apiKeyHelper` = `shunt token` | 헬퍼가 갱신 | ✅ | ✅ | 구독 |
| `ANTHROPIC_AUTH_TOKEN=<실제 API 키>` | 불필요 | ✅ | ✅ | **API(구독 아님)** |

`sk-dummy` 같은 더미 값은 디스커버리 게이트를 만족시키지만 패스스루를 깨뜨립니다 — Anthropic으로 전달되어 401을 반환합니다.

**`claude setup-token`을 선호하세요.** 이는 **1년** OAuth 토큰을 발급하므로([인증 문서](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)), 갱신할 것이 없고 하나의 값이 두 역할을 모두 커버합니다:

```bash
claude setup-token                        # 브라우저 로그인 → sk-ant-oat… 출력
export ANTHROPIC_AUTH_TOKEN=sk-ant-oat…   # 또는 설정 `env` 블록에 영속화
```

:::caution[갱신 함정]
게이트웨이 자격 증명이 활성화되면, Claude Code는 **자체 로그인 갱신을 멈춥니다**. 따라서 `~/.claude/.credentials.json` 내부의 수명이 짧은 액세스 토큰은 몇 시간 내에 만료되고, 그 파일을 그저 *읽기만* 하는 헬퍼는 깨집니다. 직접 갱신하지도 마세요 — `platform.claude.com/v1/oauth/token`은 공격적으로 rate-limit됩니다. 실시간 구독 로그인을 재사용하려면, 안전하게 갱신하는 내장 [`shunt token`](/ko/reference/cli/#shunt-token) 헬퍼를 사용하세요.
:::

### `shunt token` 자격 증명 헬퍼

`shunt token`은 Claude 구독 OAuth 토큰을 stdout으로 출력하므로, Claude Code의 `apiKeyHelper`에 바로 연결됩니다:

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

- **정적 모드** — `SHUNT_GATEWAY_TOKEN` 또는 `CLAUDE_CODE_OAUTH_TOKEN`이 설정되어 있으면, 그 값을 변경 없이 그대로 출력합니다. `claude setup-token` 값을 가리키게 하면 아무것도 갱신되지 않습니다.
- **자동 갱신 모드** — 그렇지 않으면 `~/.claude/.credentials.json`을 읽고(`CLAUDE_CREDENTIALS`로 오버라이드), 액세스 토큰을 반환하며, 만료 5분 이내일 때만 갱신하여 `0600`으로 원자적으로 다시 씁니다.

정적 + `setup-token` 경로가 가장 단순하고 안전한 기본값으로 남아 있습니다.

:::note[왜 이것이 Claude 패스스루를 인증하는가]
Claude Code는 `apiKeyHelper` 값을 `x-api-key`와 `Authorization: Bearer` **양쪽 모두**에 보냅니다. 구독 OAuth 토큰(`sk-ant-oat…`)은 bearer로만 유효하므로, `x-api-key`의 사본은 `api.anthropic.com`이 요청을 거부하게 만듭니다. 패스스루 경로에서 shunt는 bearer가 OAuth 토큰일 때 그 중복된 `x-api-key`를 제거하여, 그것이 단독으로 서게 둡니다. 이것이 없으면 `apiKeyHelper` + OAuth 토큰은 디스커버리와 매핑된 모델만 커버하고 — 패스스루는 401이 됩니다.
:::

## 3. 매핑된 프로바이더의 자격 증명 제공

이것들은 Claude Code가 아니라 **shunt의 환경**으로 갑니다:

```bash
export OPENAI_API_KEY=sk-...   # openai 프로바이더
codex login                    # codex/ChatGPT 프로바이더 (이후 자동 갱신됨)
```

## 4. 매핑된 모델 선택

Claude Code의 모델 디스커버리는 `claude`/`anthropic`으로 시작하는 id만 존중하므로, OpenAI/Codex id(`gpt-*`)에는 `ANTHROPIC_CUSTOM_MODEL_OPTION`을 사용하세요 — 이는 id가 검증을 건너뛰는 선택기 항목을 추가합니다:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

그런 다음 Claude Code의 `/model`에서 선택하세요. 그 id가 shunt가 라우팅하는 대상이므로, 구성의 `[[routes]]`/`[[route_prefixes]]` 규칙과 일치해야 합니다.

두 가지 선택기 노출 방법은 `claude-`/`anthropic-` 프리픽스를 기준으로 깔끔하게 갈리며 — 겹치지 않습니다. 디스커버리는 *오직* `claude-`/`anthropic-` id만 존중하고, `ANTHROPIC_CUSTOM_MODEL_OPTION`과 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 윈도우 오버라이드는 *오직* 그 프리픽스로 시작하지 **않는** id에만 적용됩니다:

| 항목 | `claude-`/`anthropic-` id (디스커버리 별칭) | 비-`claude-` id (예: `gpt-5.6-sol`) |
| :-- | :-- | :-- |
| [`/v1/models` 디스커버리](/ko/guides/model-discovery/) → `/model` 선택기 | ✅ 자동 등록("From gateway"), 여러 모델 | ❌ Claude Code가 버림 |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ 존중되지 않음 | ✅ 선택기에 추가(**id 하나만**) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 윈도우 | ❌ 무시됨 → 200k 기본값 | ✅ 적용됨 → 실제 윈도우 설정 |

그래서 `claude-…-via-codex` 디스커버리 별칭은 편리하지만(자동 등록, 원탭) 그 컨텍스트 윈도우는 **200k 기본값에 고정**됩니다 — 오버라이드가 `claude-` 프리픽스 id에 닿을 수 없습니다([노력 & 컨텍스트](/ko/guides/effort-and-context/)). 여러 모델에 걸친 선택기 편의를 위해서는 **디스커버리 별칭**을(200k 분모를 감수), 정확한 윈도우를 위해서는 한 번에 한 모델씩 **`ANTHROPIC_CUSTOM_MODEL_OPTION`을 통한 비-`claude-` id**를 선택하세요.

:::tip[또는 티어 별칭 재매핑]
세 번째 옵션은 Claude Code의 내장 `haiku`/`sonnet`/`opus` 별칭을 Codex 슬러그로 다시 가리키게 하는 것입니다(예: `haiku → gpt-5.6-luna`, `sonnet → gpt-5.6-sol`). 그러면 `ANTHROPIC_CUSTOM_MODEL_OPTION` 없이도 세션 전체의 티어 시스템이 ChatGPT 구독으로 해석됩니다. [ChatGPT / Codex → 티어 별칭 재매핑](/ko/guides/codex/#remap-the-tier-aliases-to-codex)을 참고하세요.
:::

### 에이전트별 우회

컨텍스트별 선택은 Claude Code 자체 노브를 통해 동작합니다 — 메인 세션이 Claude에 머무는 동안 한 에이전트를 매핑된 모델로 우회하세요:

```yaml
# .claude/agents/researcher.md
---
name: researcher
model: gpt-5.6-sol   # 이 에이전트의 추론이 우회됨; 메인 세션은 Claude에 머무름
---
```

명명된 서브에이전트의 `model:` 프론트매터는 서브에이전트를 `gpt-*` id에 올리는 **유일한** 방법입니다: 그 필드는 임의의 문자열을 받는 반면, Agent/Task 도구의 `model` 파라미터는 내장 별칭(`opus`/`sonnet`/`haiku`/`fable`)으로 제한되어 게이트웨이 id를 받을 수 없습니다. `model` 오버라이드 **없이** 에이전트를 타입별로 스폰하세요 — 도구 파라미터가 프론트매터보다 우선하므로(`CLAUDE_CODE_SUBAGENT_MODEL` > 도구 `model` > 프론트매터 > `inherit`), 하나를 전달하면 매핑된 모델을 가립니다. `CLAUDE_CODE_SUBAGENT_MODEL`은 모든 서브에이전트를 하나의 모델로 강제합니다. 윈도우는 모델 id를 자동으로 따라가므로, 하나의 전역 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`가 매핑된 서브에이전트의 크기를 정하는 동안 Claude 메인은 자체 값을 유지합니다.

## 5. 검증

```bash
# 매핑되지 않은 모델 -> Anthropic으로 전달 (사용자의 Anthropic 자격 증명 사용)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-6","max_tokens":1,"messages":[{"role":"user","content":"."}]}'

# 매핑된 모델 -> 프로바이더로 우회 (shunt의 프로바이더 자격 증명 사용)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

그런 다음 `claude`를 시작하고, `/status`를 실행하여 **Anthropic base URL** 줄이 게이트웨이를 표시하는지 확인하세요. 추론 노력과 컨텍스트 윈도우 튜닝은 [노력 & 컨텍스트](/ko/guides/effort-and-context/)도 참고하세요.
