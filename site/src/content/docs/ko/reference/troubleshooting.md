---
title: 문제 해결
description: 흔한 shunt 오류와 해결 방법.
---

| 증상 | 원인 / 해결 |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | shunt가 `~/.codex/auth.json`을 읽을 수 없습니다. `codex login`을 실행하세요. |
| 매핑된 모델에서 `authentication_error` | 만료/부재한 프로바이더 자격 증명 — `codex login`을 다시 실행하거나 `OPENAI_API_KEY`를 export하세요. shunt는 백엔드의 실제 `detail` 메시지를 노출합니다. |
| `400 … model is not supported when using Codex with a ChatGPT account` | `-codex` 슬러그(또는 계정에 부여되지 않은 것)를 사용했습니다. [models.json](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)에서 부여된 슬러그(예: `gpt-5.6-sol`, `gpt-5.5`)를 사용하거나 `upstream_model`을 설정하세요. |
| `/model`이 모델을 나열하지 않음 | `gpt-*` id에는 `ANTHROPIC_CUSTOM_MODEL_OPTION`을 사용하세요; [디스커버리](/ko/guides/model-discovery/)는 `claude`/`anthropic` 프리픽스 id만 노출합니다. |
| 디스커버리가 절대 발동하지 않음 | 게이트웨이 자격 증명(`ANTHROPIC_AUTH_TOKEN`, API 키, 또는 `apiKeyHelper`)과 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`로 게이팅됩니다. `claude --debug` → `[gatewayDiscovery]` 줄로 디버그하세요. |
| `config check failed` | `shunt check`를 실행하여 정확한 이유(bind 주소, 라우트의 알 수 없는 프로바이더, 잘못된 어댑터/인증)를 확인하세요. |
| Claude Code가 로그인을 요청함 | shunt가 매핑되지 않은 모델에 대해 전달할 수 있는 Anthropic 자격 증명(`ANTHROPIC_AUTH_TOKEN` / 로그인)을 설정하세요. base URL만으로는 자격 증명이 아닙니다. |
| 매핑된 모델에서 effort가 `medium`에 고정됨 | `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`을 설정하세요 — [노력 & 컨텍스트](/ko/guides/effort-and-context/#reasoning-effort)를 참고하세요. |
| 매핑된 모델에서 컨텍스트 길이 오류 후 세션이 멈춤 | shunt는 업스트림 오버플로 오류를 `prompt is too long …`으로 다시 써서 Claude Code가 자동 압축하고 재시도하도록 합니다 — [컨텍스트 오버플로 복구](/ko/guides/effort-and-context/#context-overflow-recovery)를 참고하세요. 몇 턴마다 반복되면 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`를 모델의 실제 윈도우로 낮추세요. |
| Cloudflare 뒤에서 스트림이 죽음(524) | [`sse_keepalive_seconds`](/ko/guides/shared-gateway/#sse-keepalive-pings)를 `0` 대신 기본값(30)으로 유지하세요. |
| 공유 게이트웨이에서 매핑된 모델에 401 | 누락/유효하지 않은 클라이언트 토큰 — `ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <token>"`을 설정하세요; [게이트웨이 공유](/ko/guides/shared-gateway/)를 참고하세요. |

전체 게이트웨이 문제 해결 표는 [Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors)를 참고하세요.
