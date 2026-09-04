---
title: MiniMax China
description: MINIMAX_API_KEY로 MiniMax-M3를 MiniMax 중국 Anthropic 호환 엔드포인트로 라우팅하기.
---

**MiniMax China**는 **MiniMax-M3**를 **Anthropic 호환** 엔드포인트로 제공합니다. 내장된
`minimax-cn` preset은 `kind = "anthropic"`,
`base_url = "https://api.minimax.cn/anthropic"` 및 `MINIMAX_API_KEY`의 API 키 인증을
제공합니다.

국제 엔드포인트는 [MiniMax](/ko/providers/minimax/)를 참고하세요. 두 호스트와 자격 증명은
별개입니다.

## 업스트림 구성

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 라우트가 없는 모델(예: claude-*)의 기본값으로 Anthropic을 유지

[[upstreams]]
name = "minimax-cn"
provider = "minimax-cn"

[[routes]]
model = "MiniMax-M3"
provider = "minimax-cn"
```

순서가 있는 `[[upstreams]]`는 shunt의 내장 provider를 대체하므로, 구성이 여전히 폴백하는
`anthropic` 기본값을 선언해야 합니다(`server.default_provider`의 기본값은 `anthropic`).

## 자격 증명

```bash
export MINIMAX_API_KEY='...'
```

중국 MiniMax open platform의 키를 사용하세요. 키를 구성 파일에 쓰지 마세요. `shunt check`는
구성 구조를 검증할 뿐 키 값을 읽지 않습니다. `MINIMAX_API_KEY`가 설정되지 않았다면
`minimax-cn`으로 라우팅된 첫 요청은 인증 오류를 반환합니다.

## 모델

| 모델 ID | 참고 |
| :-- | :-- |
| `MiniMax-M3` | 1M 토큰 컨텍스트. 클라이언트가 Claude Code의 `[1m]` 표시를 붙일 수 있으며 shunt는 매칭 전에 제거하므로 접미사 없는 ID로 라우팅하세요 |

Claude Code에서 `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, 또는 서브에이전트의
`model:` frontmatter로 라우팅된 ID를 선택하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"MiniMax-M3[1m]","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `minimax-cn`을 가리키는지 확인한 뒤
[Claude Code를 shunt에 연결](/ko/guides/connect-claude-code/)하세요.
