---
title: OpenCode Zen
description: OPENCODE_API_KEY로 매핑된 모델을 OpenCode Zen의 Anthropic 호환 엔드포인트로 라우팅하기.
---

**OpenCode Zen**은 OpenCode 팀이 큐레이션한 모델 카탈로그입니다 — Anthropic, OpenAI, Google, xAI, Z.ai 등의 테스트된 모델을 하나의 엔드포인트 뒤에 제공하고, Zen API 키로 종량제 과금됩니다. Zen은 **Anthropic Messages** 와이어 형식을 네이티브로 사용합니다(`stream: true` SSE 포함) — shunt는 Claude Code의 Messages 요청을 그대로 전달하고 Zen 키를 주입합니다. `opencode` 프리셋은 내장되어 있어 설정은 업스트림 항목 하나와 라우트뿐입니다.

## 빠른 시작

코딩 에이전트에게 맡기세요 — `shunt add`는 내장된 설정 블루프린트를 출력합니다(오프라인, 읽기 전용. 구성을 편집하는 건 에이전트고 명령은 아무것도 바꾸지 않습니다):

```bash
shunt add upstream opencode --print | claude
```

또는 아래 수동 단계를 따르세요.

## 업스트림 구성

`opencode` 프리셋은 `kind = "anthropic"`, `base_url = "https://opencode.ai/zen"`, `OPENCODE_API_KEY` 기반 API 키 인증, 그리고 zen 엔드포인트가 읽는 `x-api-key` 헤더를 제공합니다:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 라우트되지 않은 모델(예: claude-*)을 위해 Anthropic 기본값 유지

[[upstreams]]
name = "opencode"
provider = "opencode"

[[routes]]
model = "claude-fable-5-1-via-zen"
provider = "opencode"
```

순서형 `[[upstreams]]`는 shunt의 내장 프로바이더를 대체하므로, `opencode`로 라우팅하는 구성은 자신이 가리키는 `anthropic` 기본값도 선언해야 합니다(`server.default_provider` 기본값은 `anthropic`). `default_provider`를 선언된 업스트림으로 바꿀 때만 `anthropic` 항목을 제거하세요.

레거시 `[providers.opencode]` 테이블 형식도 계속 지원되지만, 프리셋이 이 형식을 채워 주지는 않습니다 — 레거시 테이블은 `kind`, `base_url`, `auth = "api_key"`, `api_key_env`, `api_key_header = "x_api_key"`를 직접 명시해야 합니다. `[[upstreams]]`와 `[providers.*]`를 한 파일에 섞지 마세요.

## 자격 증명

[opencode 콘솔](https://opencode.ai/console)에서 API 키를 만들거나(또는 `opencode` CLI 로그인이 `~/.local/share/opencode/auth.json`에 저장한 키를 재사용하고), shunt를 실행하는 환경에서 내보내세요:

```bash
export OPENCODE_API_KEY='...'
```

키를 구성 파일에 쓰지 마세요. `shunt check`는 구성의 구조만 검증하고 키 값은 읽지 않습니다 — `OPENCODE_API_KEY`가 설정되어 있지 않으면 `opencode`로 라우팅된 첫 요청이 인증 오류를 반환합니다.

Zen은 `x-api-key`에서만 자격 증명을 읽습니다 — `Authorization: Bearer` 헤더는 키 누락으로 거부됩니다. 프리셋이 올바른 헤더를 이미 보내므로 설정할 것이 없습니다. 명시적인 `auth = { mode = "api_key", header = "bearer" }` 맵으로 덮어쓸 수 있습니다(zen이 원하는 건 아닙니다).

## 모델

Zen은 크로스 벤더 카탈로그를 제공합니다 — Claude, GPT, Gemini, Grok, GLM 등. 라이브 목록은 `https://opencode.ai/zen/v1/models`에서 공개되어 있습니다. 예시 id:

| 모델 id | 비고 |
| :-- | :-- |
| `claude-fable-5-1` | zen의 Anthropic 프론티어 티어 |
| `claude-opus-5` | zen의 Anthropic 플래그십 티어 |
| `gpt-6-astra` | zen의 OpenAI 티어 |

모델 가용성은 카탈로그가 순환하며 바뀝니다 — 새 id를 라우팅하기 전에 라이브 목록을 확인하고, 알리기 전에 플랜에서 모델 id를 검증하세요.

Claude Code에서는 `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, 서브에이전트의 `model:` frontmatter로 라우팅 id를 선택합니다. `/model` 선택기에 표시하려면 `[models.upstream_model]` 맵으로 `claude` 접두사 별칭을 알리세요 — [모델 발견](/ko/guides/model-discovery/)을 참고하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-fable-5-1-via-zen","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `opencode`를 가리키는지 확인한 뒤 [Claude Code를 shunt로 연결](/ko/guides/connect-claude-code/)하세요.

## 참고

- Zen의 `/v1/messages/count_tokens`는 존재하지 않습니다: 호출한 클라이언트는 Anthropic 오류 형식 대신 zen의 HTML 404 페이지를 받습니다. 이는 zen 측 결함이며 shunt의 `count_tokens` 패스스루는 그대로 도달합니다. Claude Code의 `/context`는 카운트 엔드포인트가 실패하면 자체 카운팅으로 폴백하므로 실질적 영향은 `/context`가 느려지는 것이지 망가지는 게 아닙니다.
- 402 `Payment Required` 같은 과금 오류는 zen 자신의 응답입니다 — 자격 증명은 정상이며 계정 잔액이나 플랜이 확인 대상입니다.
