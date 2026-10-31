---
title: API Route
description: 자신의 API 키로 API Route에 모델을 라우팅합니다.
---

[API Route](https://api-route.com)는 LLM 게이트웨이입니다. 이 preset은 `/v1/messages`의 Anthropic Messages 엔드포인트를 사용하며 `API_ROUTE_API_KEY`로 Bearer 인증을 수행합니다. API Route 사용량은 Claude 구독과 별도로 과금됩니다.

## 설정

```bash
shunt add upstream api-route --print | claude
```

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "api-route"
provider = "api-route"

[[routes]]
model = "gpt-6.1-sol"
provider = "api-route"
```

라우팅되지 않은 모델의 기본값으로 Anthropic upstream을 유지하세요. 순서가 있는 upstream과 기존 provider 테이블을 혼용하지 마세요.

## 인증 정보

```bash
export API_ROUTE_API_KEY='...'
```

[API Route](https://api-route.com)에서 자신의 키를 만드세요. 키를 설정 파일이나 버전 관리에 저장하지 마세요. `shunt check`는 설정 구조만 검사하며 인증 정보는 검증하지 않습니다.

## 모델 및 검증

키로 인증하여 모델 목록을 조회하세요. 사용 가능한 모델은 계정에 따라 다릅니다. 다른 모델을 선택하기 전에 Messages 지원을 확인하세요.

```bash
curl -sS https://global.api-route.com/v1/models \
  -H "Authorization: Bearer $API_ROUTE_API_KEY"
```

```bash
shunt check
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-6.1-sol","max_tokens":32,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream: api-route`를 확인하세요. 클라이언트 설정과 모델 별칭은 [Claude Code 연결](/ko/guides/connect-claude-code/) 및 [모델 검색](/ko/guides/model-discovery/)을 참고하세요.
