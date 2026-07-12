---
title: 프로바이더
description: 내장 프로바이더와 TOML 테이블로 Anthropic 호환 백엔드를 추가하는 방법.
---

프로바이더는 **이름 → 구성 맵**입니다. 새 업스트림은 그저 또 하나의 `[providers.<name>]` 테이블일 뿐이며 — 코드 변경은 없습니다. 두 가지 어댑터 종류가 모든 것을 커버합니다:

- **`kind = "anthropic"`** — 업스트림이 Anthropic Messages API를 사용합니다. shunt는 요청을 패스스루하며, 필요하면 다른 API 키를 주입합니다.
- **`kind = "responses"`** — 업스트림이 OpenAI Responses API를 사용합니다. shunt는 Anthropic Messages ⇄ Responses를 스트리밍 포함하여 변환합니다.

## 내장 프로바이더

| 이름 | 종류 | 인증 | 백엔드 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` — 호출자 본인의 자격 증명을 전달 |
| `openai` | `responses` | `api_key` (`OPENAI_API_KEY`) | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` — `~/.codex/auth.json` 재사용 |

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

## Anthropic 호환 백엔드 추가

대부분의 서드파티 "Claude Code를 X와 함께 쓰기" 게이트웨이는 Anthropic-Messages 호환입니다: `kind = "anthropic"`에 `auth = "api_key"`이며, `base_url`과 키 env 변수만 다릅니다. 바로 사용할 수 있는 base:

| 프로바이더 | `base_url` | 예시 모델 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax 문서](https://platform.minimax.io/docs/token-plan/claude-code) 참고 |
| Mimo (Xiaomi) | `https://api-mimo.mi.com/anthropic` | [Mimo 문서](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) 참고 |
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
