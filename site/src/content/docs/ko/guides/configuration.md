---
title: 구성
description: shunt가 구성을 로드하는 방식 — 파일, 환경 변수, 라우팅.
---

shunt는 다음 순서로(우선순위가 높아지는 순서) 구성을 로드합니다:

1. **내장 기본값** — 모든 프로바이더(`anthropic`, `openai`, `codex` 등)가 사전 구성되어 있습니다.
2. **TOML 파일**. `--config <path>`를 사용하면 정확히 그 파일이 사용됩니다(파일이 없으면 오류). 그렇지 않으면 shunt는 다음에서 처음 발견되는 파일을 사용합니다:
   - `./shunt.toml`
   - `$XDG_CONFIG_HOME/shunt/shunt.toml`(기본값 `~/.config/shunt/shunt.toml`)
   - `$HOMEBREW_PREFIX/etc/shunt.toml`(기본 프리픽스 `/opt/homebrew` 및 `/usr/local`)

   부팅 로그는 어떤 파일이 로드되었는지, 또는 기본값이 사용 중인지 보고합니다.
3. `SHUNT_` 접두사가 붙은 **환경 변수**로, 중첩 키에는 `__`를 사용합니다 — 예: `SHUNT_SERVER__BIND=0.0.0.0:3001`.

기본값이 이미 모든 프로바이더를 정의하므로, `shunt.toml`에는 변경하려는 부분만 있으면 됩니다. [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)에서 시작하세요.

## 주석이 달린 예시

```toml
[server]
bind = "127.0.0.1:3001"        # shunt가 리슨하는 주소
default_provider = "anthropic" # 라우트가 없는 모든 모델의 프로바이더 (패스스루)

# 각 프로바이더는 [providers.<name>] 테이블입니다.
[providers.anthropic]
kind = "anthropic"             # Claude Code 본인의 자격 증명을 변경 없이 전달
base_url = "https://api.anthropic.com"

[providers.openai]
kind = "responses"             # Anthropic Messages -> OpenAI Responses 변환
base_url = "https://api.openai.com/v1"
auth = "api_key"
api_key_env = "OPENAI_API_KEY" # OpenAI 키를 읽어오는 env 변수
# effort = "high"              # 이 프로바이더의 선택적 기본 추론 노력

[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"         # ~/.codex/auth.json 재사용
# effort = "high"

# --- 라우팅: 요청의 `model` id가 프로바이더를 선택하는 방식 ---

# 정확한 일치가 먼저 이깁니다. `upstream_model`과 `effort`는 선택적 오버라이드입니다.
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
# upstream_model = "gpt-5.6-sol"
# effort = "high"

# 그다음 프리픽스 일치.
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"

# 선택: 디스커버리를 통해 /model 선택기에 Claude 이름 별칭을 노출.
# id는 반드시 "claude" 또는 "anthropic"으로 시작해야 하며, 그렇지 않으면 Claude Code가 무시합니다.
# [[models]]
# id = "claude-opus-via-codex"
# display_name = "Opus (via Codex)"
```

## 라우팅 우선순위

1. 요청의 `model` id에 대한 정확한 `[[routes]]` 일치.
2. `[[route_prefixes]]` 프리픽스 일치.
3. `server.default_provider` — 기본값은 `anthropic`이므로, 일치하지 않는 모델은 변경 없이 Anthropic으로 흘러갑니다.

라우트는 전달되는 모델 id(`upstream_model`)와 추론 노력(`effort`)을 모델별로 오버라이드할 수 있습니다.

## 부분 오버라이드

구성 맵은 깊은 병합(deep-merge)되므로, 내장 프로바이더를 부분 오버라이드해도 나머지 기본값은 유지됩니다:

```toml
# codex의 기본 effort만 올립니다; 나머지는 모두 내장 값 그대로 유지됩니다.
[providers.codex]
effort = "high"
```

## 검증

```bash
shunt check
# -> "config ok"를 출력하거나, 구체적인 오류(잘못된 bind 주소, 알 수 없는 프로바이더 등)를 출력
```

모든 키는 [구성 레퍼런스](/ko/reference/configuration/)를, 새 백엔드 추가는 [프로바이더](/ko/guides/providers/)를 참고하세요.
