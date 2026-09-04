---
title: Zhipu (GLM China)
description: ZHIPUAI_API_KEY로 GLM Coding Plan 모델을 Zhipu의 Anthropic 호환 BigModel 엔드포인트로 라우팅하기.
---

**Zhipu**는 중국 BigModel 플랫폼에서 **GLM** 모델을 **Anthropic 호환** 엔드포인트로 제공합니다.
내장된 `zhipu` preset은 `kind = "anthropic"`,
`base_url = "https://open.bigmodel.cn/api/anthropic"` 및 `ZHIPUAI_API_KEY`의 API 키 인증을
제공합니다.

국제 Z.ai 엔드포인트는 [Z.ai (GLM)](/ko/providers/zai/)를 참고하세요. 두 호스트와 자격 증명은
별개입니다.

## 업스트림 구성

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 라우트가 없는 모델(예: claude-*)의 기본값으로 Anthropic을 유지

[[upstreams]]
name = "zhipu"
provider = "zhipu"

[[routes]]
model = "glm-5.3"
provider = "zhipu"

[[routes]]
model = "glm-5.3-flash"
provider = "zhipu"
```

순서가 있는 `[[upstreams]]`는 shunt의 내장 provider를 대체하므로, 구성이 여전히 폴백하는
`anthropic` 기본값을 선언해야 합니다(`server.default_provider`의 기본값은 `anthropic`).

## 자격 증명

```bash
export ZHIPUAI_API_KEY='...'
```

키를 구성 파일에 쓰지 마세요. `shunt check`는 구성 구조를 검증할 뿐 키 값을 읽지 않습니다.
`ZHIPUAI_API_KEY`가 설정되지 않았다면 `zhipu`로 라우팅된 첫 요청은 인증 오류를 반환합니다.

## 모델

| 모델 ID | 참고 |
| :-- | :-- |
| `glm-5.3` | GLM Coding Plan 플래그십 텍스트 모델 |
| `glm-5.3-flash` | 더 빠른 멀티모달 티어. 클라이언트가 `[1m]`을 붙일 수 있으며 shunt는 라우트 매칭 전에 제거합니다 |

Claude Code에서 `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, 또는 서브에이전트의
`model:` frontmatter로 라우팅된 ID를 선택하세요. 대신 `/model` 선택기에 노출하려면
`[models.upstream_model]` 맵과 함께 `claude` 프리픽스가 붙은 별칭을 선언하세요 —
[모델 디스커버리](/ko/guides/model-discovery/)를 참고하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"glm-5.3-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `zhipu`를 가리키는지 확인한 뒤
[Claude Code를 shunt에 연결](/ko/guides/connect-claude-code/)하세요.
