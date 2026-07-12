---
title: 모델 디스커버리
description: Claude Code의 /model 선택기를 Claude 이름 별칭으로 자동 채우기.
---

디스커버리(`GET /v1/models`)는 Claude Code의 `/model` 선택기를 자동으로 채울 수 있습니다 — **하지만 Claude Code는 `claude`/`anthropic`으로 시작하지 않는 id를 무시합니다**([프로토콜 레퍼런스](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)). 그래서 `gpt-*` id는 무엇을 하든 클라이언트 측에서 버려집니다. 디스커버리는 `[[routes]]` 항목이 실제 업스트림 슬러그로 다시 쓰는 **Claude 이름 별칭**을 노출할 때만 유용합니다:

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"     # 반드시 claude/anthropic으로 시작
display_name = "GPT-5.6-Sol (via Codex)"

[[routes]]
model = "claude-gpt-5.6-sol-via-codex"  # Claude Code가 보내는 별칭
provider = "codex"
upstream_model = "gpt-5.6-sol"          # ChatGPT 백엔드로 전달되는 실제 슬러그
```

그런 다음 디스커버리를 활성화하고(Claude Code v2.1.129+) shunt와 Claude Code를 재시작하세요:

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

별칭은 `/model`에 *From gateway*로 표시됩니다. 이를 선택하면 `claude-gpt-5.6-sol-via-codex`를 보내고, shunt는 이를 `codex`로 라우팅하여 `gpt-5.6-sol`로 다시 씁니다.

별칭이 없는 `gpt-*` id에는 대신 `ANTHROPIC_CUSTOM_MODEL_OPTION`을 사용하세요 — [Claude Code 연결](/ko/guides/connect-claude-code/#4-select-a-mapped-model)을 참고하세요.

## 디스커버리에는 게이트웨이 자격 증명이 필요합니다

claude.ai OAuth *로그인*만으로는 디스커버리가 발동하지 않습니다. Claude Code는 `ANTHROPIC_AUTH_TOKEN`, API 키, 또는 `apiKeyHelper`가 설정되어 있을 때만 `/v1/models` 요청을 보냅니다. 순수 Max/Pro 구독 로그인에서는 아무것도 보내지 않으며 — 요청이 shunt에 도달하지 않고, 캐시도 기록되지 않습니다 — 플래그가 켜져 있어도 그렇습니다. [자격 증명 선택](/ko/guides/connect-claude-code/#2-choose-the-anthropic-credential)을 참고하세요; `claude setup-token`이 권장 경로입니다.

## 디버깅

디스커버리는 **조용히** 실패하고(3초 타임아웃, 모든 리다이렉트는 실패로 간주) 캐시/내장 목록으로 폴백합니다. `claude --debug`를 실행하고 `[gatewayDiscovery]` 줄을 찾아 실행되었는지 확인하세요.
