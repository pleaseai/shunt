---
title: 프로바이더
description: 내장 프로바이더와 TOML 테이블로 Anthropic 호환 백엔드를 추가하는 방법.
---

프로바이더는 **이름 → 구성 맵**입니다. 새 업스트림은 그저 또 하나의 `[providers.<name>]` 테이블일 뿐이며 — 코드 변경은 없습니다. 세 가지 어댑터 종류가 모든 것을 커버합니다:

- **`kind = "anthropic"`** — 업스트림이 Anthropic Messages API를 사용합니다. shunt는 요청을 패스스루하며, 필요하면 다른 API 키를 주입합니다.
- **`kind = "responses"`** — 업스트림이 OpenAI Responses API를 사용합니다. shunt는 Anthropic Messages ⇄ Responses를 스트리밍 포함하여 변환합니다.
- **`kind = "cursor"`** — 네이티브 Cursor 어댑터입니다. shunt는 Cursor의 ConnectRPC/protobuf AgentService(및 그 도구 프로토콜)를 Anthropic Messages API로 스트리밍 포함하여 브리지합니다. 내장 `cursor` 프로바이더가 사용합니다.

## 내장 프로바이더

| 이름 | 종류 | 인증 | 백엔드 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` — 호출자 본인의 자격 증명을 전달 |
| `openai` | `responses` | `api_key` (`OPENAI_API_KEY`) | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` — `~/.codex/auth.json` 재사용 |
| `xai` | `responses` | `api_key` (`XAI_API_KEY`) | `api.x.ai/v1` — xAI 개발자 API |
| `grok` | `responses` | `xai_oauth` | `cli-chat-proxy.grok.com/v1` — `shunt login xai`를 통한 SuperGrok / X Premium+ 구독 |
| `cursor` | `cursor` | `cursor_oauth` | `api2.cursor.sh` — `~/.shunt/cursor-auth.json` 재사용 (`shunt login cursor`) |

### codex 프로바이더 (ChatGPT 구독)

Codex CLI로 한 번 로그인하면, shunt가 `~/.codex/auth.json`을 읽고 자동 갱신합니다:

```bash
codex login
```

파일이 없거나 만료되면, shunt는 `codex login`을 실행하라는 `authentication_error`를 반환합니다.

전체 설정 — 인증 파일 처리, 모델 선택, 노력, 컨텍스트 크기 조정 — 은 전용 [ChatGPT / Codex 가이드](/ko/guides/codex/)를 참고하세요.

:::caution[모델 슬러그]
ChatGPT 계정 Codex 백엔드는 `gpt-*-codex` 슬러그를 **거부**합니다 — 계정에 실시간으로 부여된(live-entitled) 슬러그만 받아들입니다. 권위 있는 카탈로그는 openai/codex의 [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)입니다. 현재 슬러그는 `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`(프런티어)와 `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`입니다. 오래된 계정은 이전 슬러그만 부여받았을 수 있습니다. 라우트에서 `upstream_model`을 사용하면 임의의 별칭을 부여된 슬러그로 매핑할 수 있습니다.
:::

### cursor 프로바이더 (Cursor 구독)

내장 `cursor` 프로바이더는 Cursor 자체의 ConnectRPC/protobuf AgentService(`api2.cursor.sh`)를 통해 **Cursor** 구독에 도달합니다 — `kind = "cursor"` 네이티브 어댑터가 이를 Anthropic Messages로, 그리고 그 반대로 스트리밍과 Cursor의 네이티브 도구 호출을 포함하여 변환합니다. 한 번 로그인하세요:

```bash
shunt login cursor
```

이 명령은 Cursor OAuth 플로우를 실행하고 `~/.shunt/cursor-auth.json`을 기록하며, shunt가 이를 읽고 자동 갱신합니다. 파일이 없거나 만료되면, shunt는 `shunt login cursor`를 실행하라는 `authentication_error`를 반환합니다.

`cursor:*` 모델 id를 이 프로바이더로 라우팅하세요 — 프로바이더는 기본으로 시드되므로 `[providers.cursor]` 테이블은 필요하지 않습니다:

```toml
[[routes]]
model = "cursor:gpt-5.5"
provider = "cursor"
```

**모델 id와 에이전트 모드.** 프리픽스가 Cursor의 에이전트 모드를 선택하고 접미사가 Cursor 모델 id입니다:

| 형식 | 에이전트 모드 | 예시 |
| :-- | :-- | :-- |
| `cursor:<id>` / `cursor-agent:<id>` | Agent | `cursor:gpt-5.5` |
| `cursor-plan:<id>` | Plan | `cursor-plan:gpt-5.5` |
| `cursor-ask:<id>` | Ask | `cursor-ask:gpt-5.5` |

레거시 축약 이름도 받아들입니다: `cursor`, `cursor-agent`, `cursor-composer`, `cursor-composer-fast`(Agent); `cursor-plan`, `composer-2.5`(Plan); `cursor-ask`, `composer-2.5-fast`(Ask). 그 외의 모델 id는 `invalid_request_error`로 거부됩니다.

:::note[오버라이드]
`SHUNT_CURSOR_BASE_URL`은 엔드포인트를, `SHUNT_CURSOR_AUTH_FILE`은 자격 증명 경로를, `SHUNT_CURSOR_CLIENT_VERSION`은 `x-cursor-client-version` 헤더를 오버라이드합니다(Cursor가 오래된 클라이언트 버전을 거부하기 시작하면 재빌드 없이 값을 올리세요). `cursor_oauth` 프로바이더는 HTTPS로 Cursor 호스트에 고정됩니다 — `base_url`을 오프-오리진으로 지정하는 것은 베어러 토큰이 유출되지 않도록 거부됩니다.
:::

:::caution[본인의 판단]
비공식 클라이언트에서 Cursor 구독을 재사용하는 것은 본인의 판단입니다 — Cursor의 약관이나 계정 제재에 저촉될 수 있습니다. 사용에 따른 책임은 본인에게 있습니다.
:::

### xai / grok 프로바이더 (Grok)

두 개의 내장 프로바이더가 xAI의 **Grok** 모델에 도달하며, 자격 증명으로 갈립니다: **`grok`**은
OAuth를 통해 사용자의 **SuperGrok / X Premium+** 구독을 사용하고(`shunt login xai`, 토큰당 과금 없음),
**`xai`**는 측정된 개발자 API에 대해 `XAI_API_KEY`를 사용합니다. 구독 bearer와 API 키는 서로 교체할 수
**없습니다** — 각각은 자신의 프로바이더에서만 동작합니다.

전체 설정 — 로그인, 두 프로바이더 블록, 모델 슬러그, 옵트인 노력 다이얼, 자격 관련 함정 — 은 전용
[xAI / Grok 가이드](/ko/guides/xai/)를 참고하세요.

## Anthropic 호환 백엔드 추가

대부분의 서드파티 "Claude Code를 X와 함께 쓰기" 게이트웨이는 Anthropic-Messages 호환입니다: `kind = "anthropic"`에 `auth = "api_key"`이며, `base_url`과 키 env 변수만 다릅니다. 바로 사용할 수 있는 base:

| 프로바이더 | `base_url` | 예시 모델 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax 문서](https://platform.minimax.io/docs/token-plan/claude-code) 참고 |
| Mimo (Xiaomi) | `https://api.xiaomimimo.com/anthropic` | `mimo-v2.5-pro` — [Mimo 문서](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) 참고 |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8`(`x_api_key`를 받아들임) |

예를 들어, Kimi의 모델을 shunt를 통해 라우팅하려면:

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "KIMI_API_KEY"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

그런 다음 `export KIMI_API_KEY=…`를 실행하고, [Claude Code를 shunt에 연결](/ko/guides/connect-claude-code/)한 뒤, `kimi-k2.7-code`를 선택하세요(`ANTHROPIC_CUSTOM_MODEL_OPTION` 또는 `ANTHROPIC_MODEL`을 통해). `shunt check`를 실행하여 검증하세요 — 라우트의 알 수 없는 프로바이더, 누락된 `api_key_env`, 잘못된 `base_url`을 보고합니다.

모든 프로바이더 키(`kind`, `auth`, `api_key_header`, `count_tokens` 등)는 [구성 레퍼런스](/ko/reference/configuration/)에 문서화되어 있습니다.

## 서브에이전트 플러그인

[`pleaseai/shunt` 마켓플레이스](https://github.com/pleaseai/shunt/tree/main/plugins)는 각 프로바이더의 모델에 고정된, 미리 만들어진 Claude Code 서브에이전트를 제공합니다 — 모델당 하나의 에이전트. 플러그인을 설치한 뒤, 모델을 `@`로 멘션하거나 `CLAUDE_CODE_SUBAGENT_MODEL`을 설정하세요. 각 에이전트의 `model:` 프론트매터는 그 서브에이전트만 우회시키며, 메인 세션은 Claude에 머뭅니다.

| 플러그인 | 모델 (에이전트당 하나) | 프로바이더 |
| :-- | :-- | :-- |
| `shunt-codex` | `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` | `codex` (ChatGPT 구독) |
| `shunt-xai` | `grok-build-0.1`, `grok-4.5`, `grok-4.3` | `xai` (API 키) 또는 `grok` (구독) |
| `shunt-kimi` | `kimi-k2.7-code` | `kimi` |
| `shunt-deepseek` | `deepseek-v4-pro`, `deepseek-v4-flash` | `deepseek` |
| `shunt-zai` | `glm-5.2`, `glm-4.7` | `zai` |
| `shunt-minimax` | `MiniMax-M3[1m]` | `minimax` |
| `shunt-mimo` | `mimo-v2.5-pro` | `mimo` |

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-xai@shunt
```

각 플러그인은 여전히 `shunt.toml`에서 해당 프로바이더가 라우팅되어야 하고(위 섹션 참고) 일치하는 자격 증명이 내보내져야 합니다 — 플러그인 자체의 README가 정확한 라우트와 env 변수를 나열합니다. grok 모델은 두 xAI 프로바이더 중 어느 쪽으로도 제공할 수 있습니다: `xai`(API 키, 토큰당 과금) 또는 `grok`(`shunt login xai`를 통한 SuperGrok / X Premium+ 구독; 티어로 게이트됨 — 403 시 `xai`로 폴백).
