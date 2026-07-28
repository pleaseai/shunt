---
title: 모델 디스커버리
description: Claude Code의 /model 선택기를 Claude 이름 별칭으로 자동 채우기.
---

디스커버리(`GET /v1/models`)는 Claude Code의 `/model` 선택기를 자동으로 채울 수 있습니다. 기본적으로 shunt는 관리자가 선별한 `[[models]]` 항목을 먼저 반환한 뒤, 스스로 발견한 모델을 추가합니다. id가 정확히 같은 항목은 선별된 항목을 우선하여 중복을 제거합니다. 선별된 목록만 노출하려면 최상위에 `auto_include_builtin_models = false`를 설정하세요.

뒷부분은 shunt가 첫 번째 Anthropic 종류 업스트림에 실제 목록을 물어보고 해당 인증 방식에 맞는 크리덴셜을 사용합니다. `auth = "passthrough"`에서는 호출자가 전달한 크리덴셜을 사용하므로 호출자마다 해당 크리덴셜로 사용할 수 있는 모델을 보게 됩니다. `api_key`에서는 설정된 키를 사용합니다. `claude_oauth`에서는 추론 경로와 동일한 유효 계정 집합에서 가장 먼저 해석되는 비활성화되지 않은 계정을 사용합니다. 이 집합에는 계정 저장소에서 검색된 계정이 포함되며 `account_scope` 순서를 따릅니다. 디스커버리는 풀 선택, 쿨다운, 할당량 기록을 수행하지 않습니다. 따라서 게이트웨이 소유 크리덴셜을 사용하는 이 두 방식에서는 모든 호출자가 해당 크리덴셜 범위의 카탈로그를 공유합니다. Anthropic 종류 업스트림이 없거나, 사용할 크리덴셜이 없거나, 호출이 실패·타임아웃(2초 상한)하면 내장 Claude 카탈로그 스냅샷으로 폴백합니다. 캐시는 하지 않습니다.

발견된 모델은 전용 `[[routes]]` 항목이 필요하지 않습니다. 일반 라우팅 규칙으로 해석되며, `[[routes]]`나 `[[route_prefixes]]` 어느 것에도 매칭되지 않을 때 `server.default_provider`로 폴백합니다.

Claude Code는 디스커버리된 id가 `claude`/`anthropic`으로 시작하지 않으면 무시하므로([프로토콜 레퍼런스](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)), `gpt-*` 같은 비-Claude 모델에는 Claude 이름 별칭을 사용하세요.

## 디스커버리와 라우팅 통합

선별한 모델에서 라우팅과 업스트림 변환을 직접 선언할 수 있습니다. 단일 항목 맵의 키는 설정된 provider 이름이고 값은 업스트림으로 보낼 모델 id입니다:

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"
display_name = "GPT-5.6-Sol (via Codex)"

[models.upstream_model]
codex = "gpt-5.6-sol"
```

이 별칭을 선택하면 `codex`로 라우팅하고 업스트림에는 `gpt-5.6-sol`을 보냅니다. 이 맵은 별도의 `[[routes]]` 항목보다 권장되는 정확한 id 형식이며, `[[routes]]`, `[[route_prefixes]]`, `server.default_provider`보다 우선합니다. 항목마다 설정된 provider 하나만 지원하며, 잘못된 맵이나 같은 id의 `[[routes]]` 항목은 시작 오류입니다.

## 별도 라우트 사용

맵이 없는 기존 `[[models]]` 항목은 이전과 호환됩니다. 계속 `[[routes]]` 항목과 조합하여 실제 업스트림 슬러그로 다시 쓸 수 있습니다:

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

## Claude Desktop은 tier 이름 id만 인식합니다

Claude Code는 `claude`/`anthropic`으로 시작하는 디스커버리 id를 모두 받아들이지만, **Claude Desktop은 더 엄격합니다**. `claude-sonnet-*`, `claude-opus-*`, `claude-haiku-*`, `claude-fable-*` 같은 tier 이름 id만 표시합니다. 따라서 위의 `claude-<slug>-via-<provider>` 별칭은 Claude Code에는 나타나지만 `gpt`가 tier 이름이 아니므로 **Claude Desktop에서는 조용히 버려집니다**.

내장 카탈로그는 모두 tier 이름이라 Desktop에서도 그대로 보입니다. 사라지는 것은 선별한 `claude-<slug>-via-<provider>` 별칭뿐입니다. 비-Anthropic 백엔드를 Claude Desktop에 노출하려면 tier 이름 id를 재사용하고 `[[routes]]`의 `upstream_model`로 매핑하세요:

```toml
[[routes]]
model = "claude-sonnet-5"        # Claude Desktop이 인식하는 tier 이름 id
provider = "codex"
upstream_model = "gpt-5.6-sol"   # 실제 백엔드 슬러그
```

Desktop에서 이를 선택하면 의도한 업스트림으로 해석됩니다. 이 route는 해당 id에 대한 내장 카탈로그의 기본 라우팅을 덮어쓰므로, 백엔드 매핑이 사용자에게 여전히 의미 있는 tier 이름을 고르세요.

## 디스커버리에는 게이트웨이 자격 증명이 필요합니다

claude.ai OAuth *로그인*만으로는 디스커버리가 발동하지 않습니다. Claude Code는 `ANTHROPIC_AUTH_TOKEN`, API 키, 또는 `apiKeyHelper`가 설정되어 있을 때만 `/v1/models` 요청을 보냅니다. 순수 Max/Pro 구독 로그인에서는 아무것도 보내지 않으며 — 요청이 shunt에 도달하지 않고, 캐시도 기록되지 않습니다 — 플래그가 켜져 있어도 그렇습니다. [자격 증명 선택](/ko/guides/connect-claude-code/#2-choose-the-anthropic-credential)을 참고하세요; `claude setup-token`이 권장 경로입니다.

## 디버깅

디스커버리는 **조용히** 실패하고(3초 타임아웃, 모든 리다이렉트는 실패로 간주) 캐시/내장 목록으로 폴백합니다. `claude --debug`를 실행하고 `[gatewayDiscovery]` 줄을 찾아 실행되었는지 확인하세요.
