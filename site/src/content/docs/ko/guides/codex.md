---
title: ChatGPT / Codex
description: ~/.codex/auth.json을 재사용하여 Claude Code 추론을 ChatGPT/Codex 구독으로 라우팅하기 — 인증, 모델 슬러그, 노력, 컨텍스트 윈도우.
---

**`codex`** 프로바이더는 매핑된 모델의 추론을 API 키 대신 **ChatGPT / Codex 구독**으로 라우팅합니다. Codex CLI가 이미 `~/.codex/auth.json`에 기록한 자격 증명을 재사용하므로, 붙여넣을 것도 없고 토큰당 과금도 없습니다 — 요청은 사용자의 ChatGPT 계정으로 인증되고 `codex` CLI가 통신하는 것과 동일한 백엔드가 응답합니다.

이 페이지는 처음부터 끝까지의 설정입니다. 더 깊은 주제 페이지([노력 & 컨텍스트](/ko/guides/effort-and-context/), [모델 디스커버리](/ko/guides/model-discovery/), [프로바이더](/ko/guides/providers/))를 반복하는 대신 그쪽으로 링크합니다.

## 동작 방식

`codex`는 내장 **`kind = "responses"`** 프로바이더입니다: shunt는 Claude Code의 Anthropic Messages 요청을 OpenAI **Responses API**로 변환하여 ChatGPT 계정 Codex 백엔드로 보내고, 스트리밍된 응답을 다시 변환합니다. 이를 일반 OpenAI가 아닌 "Codex"로 만드는 세 가지 요소:

| 측면 | 값 |
| :-- | :-- |
| 엔드포인트 | `<base_url>/codex/responses` |
| 인증 | `~/.codex/auth.json`의 ChatGPT OAuth, 자동 갱신 |
| Responses 방언 | `Chatgpt` 플레이버 — codex가 절대 보내지 않는 파라미터(예: `max_output_tokens`)를 제거하고, `store: false`를 보내며, 암호화된 추론을 왕복시킴 |

방언은 프로바이더 이름이 아니라 `auth = "chatgpt_oauth"`를 기준으로 결정됩니다.

## 1. 로그인

Codex CLI로 한 번 로그인하세요. shunt는 CLI가 기록한 파일을 읽고 갱신합니다 — Codex에 대해 자체 로그인을 실행하지는 **않습니다**.

```bash
codex login
```

이는 `~/.codex/auth.json`을 생성합니다. 그 파일이 없거나, 토큰이 없거나, refresh 토큰이 사라졌으면, shunt는 `codex login`을 다시 실행하라는 `authentication_error`를 반환합니다.

:::note[다른 인증 파일 위치]
shunt는 `$CODEX_AUTH_FILE`을 먼저, 그다음 `$HOME/.codex/auth.json`, 그다음 `.codex/auth.json`을 봅니다.
CI, 샌드박스, 또는 두 번째 계정을 위해 다른 곳을 가리키게 하세요:

```bash
export CODEX_AUTH_FILE=/etc/shunt/codex-auth.json
```
:::

## 2. 프로바이더 블록 (선택)

`codex`는 내장이므로 선언할 필요가 없습니다. 다음은 전체 기본값이며, 부분 테이블은 설정한 키만 오버라이드합니다(구성 맵은 깊은 병합됨):

```toml
[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"   # shunt가 /codex/responses를 붙임
auth = "chatgpt_oauth"                          # ~/.codex/auth.json 읽기 + 자동 갱신
# effort = "high"                               # 선택적 기본 추론 노력 (§4)
# count_tokens = "tiktoken"                      # 기본값; "estimate"로 옵트아웃
```

일반적인 오버라이드: 모든 Codex 트래픽에 대한 기본 `effort` 고정, 또는 `count_tokens = "estimate"` 설정. `api_key_env` / `api_key_header`는 `chatgpt_oauth`에 적용되지 않습니다 — 자격 증명은 인증 파일에서 옵니다. 모든 키는 [구성 레퍼런스](/ko/reference/configuration/#providersname)를 참고하세요.

:::note[ApiKey 모드는 `openai` 프로바이더로 갑니다]
`~/.codex/auth.json`이 **`ApiKey`** 모드이면(ChatGPT 계정이 아니라 OpenAI API 키로 로그인한 경우), `codex` OAuth 경로는 토큰을 찾지 못하고 오류가 납니다. 그 키는 대신 `OPENAI_API_KEY`가 설정되지 않았을 때 폴백으로 **`openai`** 프로바이더가 가져갑니다. `codex`는 구체적으로 ChatGPT 구독 경로입니다.
:::

## 3. 모델을 `codex`로 라우팅

요청의 `model` id가 프로바이더를 선택합니다. 우선순위: 정확한 `[[routes]]` → `[[route_prefixes]]` → `server.default_provider`.

```toml
[[routes]]
model = "gpt-5.6-sol"        # Claude Code가 보내는 id (아래 §4 참고)
provider = "codex"
# upstream_model = "gpt-5.6-sol"   # 선택: 다른 슬러그를 업스트림으로 전달
# effort = "high"                  # 선택: 이 라우트의 effort 고정
```

`upstream_model`은 Claude Code가 보내는 id와 백엔드가 받는 슬러그가 다르도록 해줍니다 — [디스커버리 별칭](/ko/guides/model-discovery/)의 메커니즘이자, Claude Code env를 건드리지 않고 실제 슬러그를 교체하는 방법입니다.

:::caution[모델 슬러그 — `-codex` 금지]
ChatGPT 계정 백엔드는 `gpt-*-codex` 슬러그(예: `gpt-5.2-codex`)를 `400`으로 **거부**합니다. 계정에 **실시간으로 부여된** 슬러그만 받아들입니다. 권위 있는 카탈로그(및 각 슬러그가 받아들이는 추론 레벨)는 openai/codex의 [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)입니다. 현재 슬러그: `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`(프런티어)와 `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`. 오래된 계정은 이전 슬러그만 부여받았을 수 있습니다(무료 계정은 `gpt-5.5`로 해석된 바 있습니다). shunt는 백엔드 자체의 오류 `detail`을 노출하므로, 잘못된 슬러그는 실제 이유를 반환합니다.
:::

:::note[`Model not found <slug>`는 자격이 아니라 클라이언트 버전 게이팅입니다]
일부 슬러그는 `minimal_client_version`을 요구합니다(예: `gpt-5.6-luna`는 ≥ 0.144.0 필요). 요청의 클라이언트 신원이 없거나 너무 오래되면 백엔드는 `Model not found <slug>`를 반환합니다. shunt는 고정된 Codex CLI 신원 헤더(`originator: codex_cli_rs`, `user-agent`, `version`)를 **openai/codex rust-v0.144.1**에 고정하여 보내므로 이를 피합니다. [openai/codex#31967](https://github.com/openai/codex/issues/31967)을 참고하세요.
:::

## 4. Claude Code에서 모델 선택

Claude Code의 `/model` 선택기는 `claude`/`anthropic`으로 시작하는 디스커버리 id만 존중하므로, 원시 `gpt-*` id는 두 경로 중 하나가 필요합니다 — 이들은 `claude-` 프리픽스를 기준으로 갈리며 겹치지 않습니다:

| | `claude-…` 디스커버리 별칭 | 비-`claude-` id (`gpt-5.6-sol`) |
| :-- | :-- | :-- |
| 디스커버리를 통한 `/model` 선택기 | ✅ 자동 등록, 여러 모델 | ❌ Claude Code가 버림 |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ 존중되지 않음 | ✅ 선택기에 추가(id 하나) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 윈도우 | ❌ 무시됨 → 200k | ✅ 실제 윈도우 |

**기본 경로** — 슬러그를 선택기에 직접 추가:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

그 id가 바로 shunt가 라우팅하는 대상이므로, `[[routes]]`/`[[route_prefixes]]` 규칙과 일치해야 합니다. 이것이 권장 경로입니다 — 정확한 컨텍스트 윈도우까지 설정할 수 있는 유일한 경로이기도 합니다. 대신 여러 Codex 모델을 선택기에 자동 등록하려면, `claude-` 이름의 [디스커버리 별칭](/ko/guides/model-discovery/)을 사용하세요(200k 윈도우 트레이드오프를 감수).

#### 서브에이전트를 Codex 슬러그에 올리기

서브에이전트는 메인 세션이 Claude에 머무는 동안 Codex 슬러그에서 실행될 수 있습니다. `model:` 프론트매터 필드는 **임의의 문자열**을 받아들입니다(내장 별칭만 받는 Agent/Task 도구의 `model` 파라미터와 달리). **기존** 서브에이전트를 `gpt-5.6-sol`로 가리키게 하려면, `.claude/agents/<name>.md`를 편집하여 `model:`을 설정하세요:

```markdown
---
name: researcher
description: Deep research agent.
model: gpt-5.6-sol        # 이전: sonnet (또는 없음 → 상속됨)
---

<the agent's system prompt — unchanged>
```

`model` 오버라이드 **없이** 스폰하세요(도구 파라미터가 프론트매터보다 우선). 해석 순서: `CLAUDE_CODE_SUBAGENT_MODEL` > 도구 `model` > 프론트매터 > `inherit`. **모든** 서브에이전트를 하나의 슬러그로 강제하려면 `export CLAUDE_CODE_SUBAGENT_MODEL="gpt-5.6-sol"`을 설정하세요.

어느 쪽이든 슬러그에는 `[[routes]]` 항목이 필요하며, 비-`claude-`이므로 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`과 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`를 따릅니다 — 윈도우는 id를 자동으로 따라갑니다.

:::tip[미리 만들어진 에이전트]
**[`shunt-codex` 플러그인](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-codex)**은 `gpt-5.6-sol` / `-terra` / `-luna`용 서브에이전트를 제공합니다 — `/plugin marketplace add pleaseai/shunt` 후 `/plugin install shunt-codex@shunt`로 설치하세요.
:::

### 티어 별칭을 Codex로 재매핑

커스텀 id 하나를 추가하는 대신, Claude Code의 **내장 티어 별칭**을 Codex 슬러그로 다시 가리키게 하면, 세션 전체의 티어 시스템이 ChatGPT 구독으로 해석됩니다([model-config env 변수](https://code.claude.com/docs/en/model-config#environment-variables)).

| Env 변수 | 제어 대상 |
| :-- | :-- |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | `haiku` 별칭 **및 백그라운드 "small-fast" 모델** |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | `sonnet` 별칭 |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_FABLE_MODEL` | `opus` / `fable` 별칭 |

2티어 설정 — `haiku → gpt-5.6-luna`, `sonnet → gpt-5.6-sol`:

```bash
export ANTHROPIC_DEFAULT_HAIKU_MODEL="gpt-5.6-luna"
export ANTHROPIC_DEFAULT_SONNET_MODEL="gpt-5.6-sol"

# 더 나은 선택기 레이블 (_NAME/_DESCRIPTION 짝은 게이트웨이에서 동작)
export ANTHROPIC_DEFAULT_SONNET_MODEL_NAME="GPT-5.6-Sol"
export ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION="ChatGPT/Codex Sol via shunt"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME="GPT-5.6-Luna"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION="ChatGPT/Codex Luna via shunt (background tier)"
```

```toml
# shunt.toml — 해석된 두 id 모두 라우트가 필요
[[routes]]
model = "gpt-5.6-luna"
provider = "codex"

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

이제 `/model`에서 **Sonnet**을 선택하면 Codex를 통해 `gpt-5.6-sol`이 실행되고, 모든 백그라운드/haiku 작업은 `gpt-5.6-luna`가 실행합니다 — 해석된 id가 바로 shunt가 라우팅하는 대상이므로 `ANTHROPIC_CUSTOM_MODEL_OPTION`이 필요 없습니다.

:::note[제대로 설정하기]
- 해석된 id는 `claude-`로 시작하지 않으므로, effort 다이얼을 위해 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`을 설정하세요. `gpt-5.6-sol`과 `gpt-5.6-luna`는 **둘 다 372k**이므로, 하나의 전역 `CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000`이 두 티어 모두에 맞습니다.
- `_SUPPORTED_CAPABILITIES` 짝은 서드파티 프로바이더(Bedrock 등)에 대해 문서화되어 있으나 게이트웨이에 대해서는 확인되지 않았습니다 — shunt에서는 effort를 위해 `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`을 사용하세요.
- **haiku 티어는 백그라운드 "small-fast" 모델**입니다(요약, 제목, 빠른 분류). 추론 모델로 라우팅해도 괜찮지만, 그 잦은 트래픽에 ChatGPT 할당량을 소비하고 느려질 수 있습니다 — 그것이 중요하다면 거기에는 가장 저렴하게 부여된 슬러그를 고르세요.
- 재매핑은 **전역이며 세션 전체**입니다. 허용 목록(`availableModels` / `enforceAvailableModels`)이 있으면 별칭이 목록 밖으로 리다이렉트될 수 없습니다(Claude Code는 **v2.1.176**부터 티어 별칭 env 변수에 대해 이를 강제합니다).
:::

## 5. 추론 노력

Claude Code의 일반 컨트롤(`/effort`, `/model` 슬라이더, `--effort`)로 노력을 설정하세요. shunt는 이를 Responses `reasoning.effort`로 매핑하며, `max`를 지원하지 않는 슬러그에 대해서는 `max → xhigh`로 접습니다(오직 **gpt-5.6** 계열만 지원).

:::note[커스텀 id에 필수]
Claude Code가 노력 지원으로 인식하지 못하는 id(예: `gpt-5.6-sol`)에 대해서는 다음을 설정해야 합니다:

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```

그렇지 않으면 Claude Code는 effort 필드를 생략하고 shunt는 `medium`으로 폴백합니다. 구성의 `route.effort` / `[providers.codex].effort` 오버라이드가 클라이언트 값보다 우선합니다.
:::

전체 우선순위와 effort 테이블: [노력 & 컨텍스트](/ko/guides/effort-and-context/#reasoning-effort).

## 6. 컨텍스트 윈도우

Claude Code는 매핑된 id에 대해 컨텍스트 바를 고정된 **200k**로 크기 조정합니다. `gpt-5.6-sol`의 실제 윈도우는 **372k**이므로(`gpt-5.5`는 272k), 비-`claude-` id에 대해서는 올려주세요:

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

이 값은 **전역**이며(세션당 하나의 값), 실제 윈도우보다 크게 설정하면 `prompt is too long` 오버플로 churn을 유발합니다 — 매핑된 모델 중 가장 작은 실제 윈도우에 맞추세요. shunt는 그 오버플로를 다시 써서 Claude Code가 자동 압축(auto-compact)하고 재시도하도록 하지만, 각 왕복은 낭비되는 지연입니다. 자세한 내용, 실시간 검증된 경계, `count_tokens` 동작: [노력 & 컨텍스트](/ko/guides/effort-and-context/#context--usage-display-for-mapped-models).

## 전체 예시

`shunt.toml`:

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"

[providers.codex]
effort = "high"     # 선택: 모든 Codex 트래픽에 high effort 고정

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

셸(shunt와 Claude Code 모두 이 설정으로 실행):

```bash
codex login                                          # 일회성
./target/release/shunt run                           # 게이트웨이 시작

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"   # /model 선택기에 추가
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1            # effort 슬라이더가 Codex에 닿도록
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000         # gpt-5.6-sol의 실제 윈도우
```

`/model`에서 **gpt-5.6-sol**을 선택하세요. 세션의 나머지는 모두 여전히 변경 없이 Anthropic으로 흐르며, 오직 매핑된 모델의 추론만 ChatGPT/Codex 구독이 응답합니다.

## 웹 검색

Claude Code의 내장 **웹 검색**은 별도 설정 없이 Codex 경로에서 동작합니다. 웹 검색을 활성화하면 Claude
Code가 호스티드 `web_search_20250305` 도구를 보내고, shunt는 이를 Responses API의 호스티드
**`web_search`** 도구로 등록합니다. 따라서 검색이 처리되지 않은 도구 호출로 되돌아오는 대신 백엔드에서
실제로 수행됩니다.

- 도메인 필터가 그대로 전달됩니다 — Claude Code의 `allowed_domains` / `blocked_domains`가 Responses
  `web_search`의 `filters`가 됩니다.
- `codex`(ChatGPT) 및 `openai`(스톡 Responses) 프로바이더에 적용됩니다.
- **xAI / Grok 라우트는 지원하지 않습니다** — Grok의 Responses API는 함수 도구만 허용하므로 shunt가
  호스티드 웹 검색 도구를 제거합니다. 웹 검색에는 `codex` 또는 `openai` 라우트를 사용하세요.

## 도구 검색

Claude Code의 **도구 검색** — MCP / LSP 도구 스키마를 미뤄 두었다가 `ToolSearch` 도구로 필요할 때만
드러내어, 호출하지 않을 도구에 컨텍스트를 쓰지 않게 하는 기능 — 도 Codex 경로에서 동작하지만 shunt 뒤에서는
**기본적으로 꺼져 있습니다**. 활성화하려면:

```bash
export ENABLE_TOOL_SEARCH=true
```

Claude Code는 base URL이 퍼스트파티 Anthropic 호스트가 아니면 낙관적 도구 검색을 비활성화하는데, shunt는
그에 해당하지 않습니다. 따라서 이 플래그가 없으면 첫 턴부터 모든 도구의 전체 스키마가 업스트림으로 전송되어
기능이 무의미해집니다(동작은 하지만 절약되는 것이 없습니다). 클라이언트 자체 규약은 **프록시가
`tool_reference` 블록을 전달한다면** `ENABLE_TOOL_SEARCH=true`를 설정하라는 것이며, shunt는 이를
전달합니다.

활성화하면 Claude Code는 미룰 수 있는 도구를 프롬프트에 **이름**만 나열하고 스키마는 보류합니다. shunt는
아직 로드되지 않은 이 도구들을 모델이 `ToolSearch`로 로드하기 전까지 업스트림 도구 집합에서 제외하며, 그
결과 생성된 `tool_reference`가 해당 도구의 전체 스키마를 필요할 때 드러냅니다. 이로써 미뤄 둔 스키마가 첫
턴부터 차지했을 컨텍스트 윈도우를 되찾습니다 — 도구 검색의 핵심 목적입니다.

- `shunt.toml` 변경은 필요 없습니다 — 순수하게 Claude Code 환경 변수입니다.
- `codex`(ChatGPT) 및 `openai`(스톡 Responses) 프로바이더에 적용됩니다.
- 미루지 않는 도구(및 위의 호스티드 `web_search` 도구)는 항상 전달됩니다. 점진적으로 드러나는 것은 미룰 수
  있는 도구뿐입니다.

### 옵트인 네이티브 프로토콜

위의 shim은 `tool_reference`를 스키마 텍스트로 렌더링하는 방식으로 동작합니다 — 업스트림 컨텍스트에서
아무것도 회수하지 않고, 전체 스키마를 보내는 *시점*만 미룰 뿐입니다. **옵트인 대안**(issue #82)으로,
shunt는 대신 도구 검색을 OpenAI Responses API 자체의 **네이티브 클라이언트 실행 `tool_search`**
프로토콜로 매핑할 수 있습니다: Claude Code의 `ToolSearch` 도구는 `tool_search`(`execution: "client"`)
도구가 되고, 그 `tool_use`는 `tool_search_call`이 되며, `tool_reference` 결과는 로드된 도구의 전체
스키마를 구조화된 JSON으로 담는 `tool_search_output` 항목이 됩니다 — 스키마를 텍스트로 접어 넣는 대신
실제 도구 로딩 시맨틱과 캐시 동작을 보존합니다. 프로바이더별로 활성화하세요:

```toml
[providers.codex]
tool_search = true
```

요구 사항 — 지원되지 않는 조합은 오류 없이 조용히 #43 shim을 유지합니다:

- 업스트림은 스톡 OpenAI 또는 ChatGPT/Codex 계열 Responses 백엔드여야 합니다. xAI / Grok 라우트는
  항상 shim을 유지합니다.
- 라우팅되는 모델은 **gpt-5.4 이상**(`gpt-5.4`, `gpt-5.5`, 또는 `gpt-5.6` 패밀리)이어야 합니다.
  이전 슬러그(`gpt-5.2` 이하)는 `tool_search = true`를 설정해도 shim으로 폴백합니다.
- Claude Code 쪽에서는 여전히 `ENABLE_TOOL_SEARCH=true`가 필요합니다 — 이 플래그는 shunt가 그 기능을
  업스트림으로 *어떻게* 변환하는지만 바꿀 뿐, Claude Code가 애초에 도구를 지연시키는지는 바꾸지 않습니다.

`tool_search`는 기본적으로 `false`입니다: 네이티브 형태는 특정 백엔드가 이를 받아들이는지 라이브
프로브로 확인할 때까지 이 플래그 뒤에 게이팅되어 있으므로, shunt가 모든 Codex/OpenAI 라우트를 자동으로
전환하는 것이 아니라 프로바이더별 명시적 옵트인입니다.

## 문제 해결

| 증상 | 원인 / 해결 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | `~/.codex/auth.json`이 없음(또는 잘못된 `$CODEX_AUTH_FILE`). `codex login`을 실행하세요. |
| `ChatGPT auth tokens missing` | 인증 파일이 `ApiKey` 모드임 — 그것은 `openai` 프로바이더입니다. ChatGPT 계정으로 다시 `codex login`하세요. |
| `400 … not supported when using Codex with a ChatGPT account` | `gpt-*-codex` 슬러그를 사용했습니다. 부여된 비-`-codex` 슬러그를 사용하세요. |
| `Model not found <slug>` | 클라이언트 버전 게이팅 또는 부여되지 않은 슬러그 — `models.json`으로 확인하세요. |
| `gpt-*` id에서 effort 슬라이더가 무시됨 | `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`을 설정하거나, 라우트/프로바이더 `effort` 오버라이드가 이기고 있습니다. |
| 컨텍스트 바가 과다 보고 / 조기 압축 | `CLAUDE_CODE_MAX_CONTEXT_TOKENS`를 설정하세요. 디스커버리 별칭은 이를 받을 수 없습니다 — 비-`claude-` id를 사용하세요. |
| Grok 라우트에서 웹 검색 결과가 비어 있음 | xAI/Grok의 Responses API는 웹 검색을 지원하지 않아 shunt가 도구를 제거합니다. 웹 검색에는 `codex` 또는 `openai` 라우트를 사용하세요. |
| 도구 검색이 동작하지 않음 / 매 턴 모든 도구 스키마가 전송됨 | `ENABLE_TOOL_SEARCH=true`를 설정하세요 — Claude Code는 퍼스트파티가 아닌 base URL 뒤에서 도구 검색을 기본적으로 비활성화합니다. shunt는 `tool_reference` 블록을 전달하며 미뤄 둔 스키마를 필요할 때 드러냅니다. |
| 도구 검색이 지연만 할 뿐 컨텍스트를 실제로 회수하지 않음 | 네이티브 프로토콜을 위해 `[providers.codex]`에 `tool_search = true`를 설정하세요 — 스톡 OpenAI/ChatGPT-Codex 계열과 gpt-5.4 이상 모델이 필요합니다. 위의 [도구 검색 → 옵트인 네이티브 프로토콜](#옵트인-네이티브-프로토콜)을 참고하세요. |

더 많은 내용은 전체 [문제 해결](/ko/reference/troubleshooting/) 레퍼런스를 참고하세요.
