---
title: 빠른 시작
description: shunt를 구성하고, 게이트웨이를 실행하고, Claude Code를 5분 만에 연결하기.
---

이 안내는 설치된 `shunt` 바이너리에서 시작하여, `gpt-*` 모델이 Claude Code 자체 하네스 안에서 실행되는 Claude Code 세션까지 이어집니다. 먼저 shunt를 설치하세요 — [설치](/ko/getting-started/installation/)를 참고하세요.

## 1. 구성

shunt는 모든 프로바이더가 사전 구성된 채로 제공되므로, 최소 구성에서는 라우팅만 선언하면 됩니다. `shunt.toml`을 생성하세요(작업 디렉터리 또는 `~/.config/shunt/shunt.toml`에):

```toml
# 정확한 모델 id -> 프로바이더
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"     # `codex login`을 통한 ChatGPT 로그인을 재사용

# 또는 모든 gpt-* id를 OpenAI API로 전송
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"    # OPENAI_API_KEY를 사용
```

검증합니다:

```bash
shunt check
# -> config ok
```

## 2. 프로바이더 자격 증명 제공

라우팅한 프로바이더를 선택하세요:

```bash
codex login                     # codex 프로바이더: ChatGPT 구독 로그인
# 또는
export OPENAI_API_KEY=sk-...    # openai 프로바이더: API 키
```

## 3. 게이트웨이 실행

```bash
shunt run
# -> shunt listening on 127.0.0.1:3001
```

## 4. Claude Code 연결

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1   # /effort가 reasoning.effort로 매핑되도록
claude
```

Claude Code 안에서 `/model`을 실행하고 `gpt-5.6-sol`을 선택하세요. 매핑되지 않은 모델(모든 `claude-*` id)은 이전과 완전히 동일하게 동작합니다. shunt가 사용자 본인의 자격 증명으로 Anthropic에 전달합니다.

## 5. 검증

Claude Code를 열기 전에(또는 열지 않고) 게이트웨이를 직접 테스트하세요:

```bash
# 매핑된 모델 -> 프로바이더로 우회 (shunt의 프로바이더 자격 증명 사용)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

`{"id":"msg_`로 시작하는 JSON 응답이 오면 성공한 것입니다. Claude Code 안에서 `/status`는 **Anthropic base URL**을 `http://127.0.0.1:3001`로 표시해야 합니다.

## 다음 단계

- [구성](/ko/guides/configuration/) — 구성 파일, 환경 변수 오버라이드, 라우팅 우선순위.
- [프로바이더](/ko/guides/providers/) — Kimi, DeepSeek, GLM, OpenRouter 및 기타 백엔드 추가.
- [Claude Code 연결](/ko/guides/connect-claude-code/) — 자격 증명 심화, 에이전트별 라우팅.
- [문제 해결](/ko/reference/troubleshooting/) — 흔한 오류와 해결 방법.
