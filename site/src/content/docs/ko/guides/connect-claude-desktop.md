---
title: Claude Desktop 연결
description: Claude Desktop의 서드 파티 추론을 shunt로 향하게 하고, 인증을 구성하고, 모델을 선택하기.
---

공식 [Deploy Claude Desktop with an LLM gateway](https://claude.com/docs/third-party/claude-desktop/gateway) 가이드를 기반으로 합니다 — Claude Desktop에서 연결할 게이트웨이가 바로 shunt입니다. shunt는 Anthropic [Messages API](https://docs.claude.com/en/api/messages)(스트리밍과 도구 사용을 지원하는 `POST /v1/messages`)와 선택 사항인 `GET /v1/models`를 구현하며, 이는 Claude Desktop의 서드 파티 추론이 기대하는 게이트웨이 계약과 정확히 일치합니다.

:::note[Claude Desktop의 게이트웨이 구성은 사용자 파일이 아닌 관리형 설정입니다]
아래의 모든 키는 [MDM / Bootstrap 관리형 설정](https://claude.com/docs/third-party/claude-desktop/mdm)입니다. `.mobileconfig`(macOS) 또는 `.reg`(Windows) 파일을 내보내는 앱 내 창(**Developer → Configure Third-Party Inference…**)에서 설정하거나 MDM을 통해 푸시하세요. 여기에는 `~/.claude` 형식의 사용자 파일이 없습니다.
:::

## 1. Claude Desktop을 shunt로 향하게 하기

**Developer → Configure Third-Party Inference…**에서 **Inference provider**를 **Gateway**로 설정하고, **Gateway base URL**을 실행 중인 shunt(기본 bind `127.0.0.1:3001`)로 설정하세요:

| Claude Desktop 키 | 값 |
| :-- | :-- |
| `inferenceProvider` | `gateway` |
| `inferenceGatewayBaseUrl` | `http://127.0.0.1:3001` (또는 공개 shunt URL) |

shunt는 평문 HTTP를 제공합니다. 루프백을 벗어난 배포에서는 [게이트웨이 공유](/ko/guides/shared-gateway/)와 마찬가지로 앞단에서 TLS를 종료하거나 터널을 사용하세요.

## 2. 인증 방식 선택

Claude Desktop은 세 가지 방식을 제공합니다. shunt는 정적 키(및 자격 증명 헬퍼 변형)에 자연스럽게 대응하지만, 사용자별 SSO는 게이트웨이 측 기능이며 shunt는 인바운드에서 이를 구현하지 않습니다.

| Claude Desktop 방식 | shunt 측 | 참고 |
| :-- | :-- | :-- |
| **정적 API 키** (`inferenceGatewayApiKey`) | [`[server.auth]`](/ko/guides/shared-gateway/) 클라이언트 토큰 | 권장. |
| **자격 증명 헬퍼** (`inferenceCredentialHelper`) | `[server.auth]` 클라이언트 토큰을 출력하는 실행 파일 | 이미 게이트웨이 자격 증명을 발급하는 조직용. |
| **대화형 SSO** (`inferenceGatewayOidc` + `inferenceCredentialKind: interactive`) | 인바운드에서 지원하지 않음 | shunt는 외부 IdP JWT가 아닌 *정적* 토큰을 검증합니다 — 아래를 참고하세요. |

### 정적 API 키(권장)

shunt에서 [`[server.auth]`](/ko/guides/shared-gateway/#인바운드-클라이언트-토큰)를 활성화하고 각 사용자에게 클라이언트 토큰을 발급하세요:

```toml
[server.auth]
header = "x-shunt-token"          # 기본값
tokens_env = "SHUNT_CLIENT_TOKENS"
```

그 토큰을 Claude Desktop의 `inferenceGatewayApiKey`에 입력하세요. shunt는 `Authorization: Bearer` 또는 `x-api-key`로 클라이언트 토큰을 받으므로, 어느 **Gateway auth scheme**이든 사용할 수 있습니다:

| Claude Desktop 키 | 값 |
| :-- | :-- |
| `inferenceGatewayApiKey` | shunt 클라이언트 토큰 |
| `inferenceGatewayAuthScheme` | `bearer` (기본값) 또는 `x-api-key` |

`[server.auth]`가 없으면 shunt는 인바운드 자격 증명을 요구하지 않습니다(개인 루프백 게이트웨이에는 괜찮습니다). 그래도 Claude Desktop은 이 필드가 채워져 있기를 요구하므로 아무 플레이스홀더나 입력하세요.

이 토큰은 `GET /v1/models`와 자격 증명이 주입되는(매핑된/풀) 모델을 게이팅합니다. [패스스루 모델](/ko/guides/connect-claude-code/#3-매핑된-프로바이더의-자격-증명-제공)은 열린 채로 유지되며 운영자 자체의 프로바이더 자격 증명을 전달합니다.

:::caution[shunt는 Claude Desktop의 SSO 계약을 구현하지 않습니다]
Claude Desktop의 **Interactive sign-in**(`inferenceGatewayOidc`)은 앱이 외부 IdP(Entra, Okta 등)에 인증하고 해당 IdP의 JWT를 게이트웨이로 보내게 하며, 게이트웨이는 `iss`/`aud`를 검증해야 합니다. shunt에는 인바운드 JWT 검증기가 없습니다. shunt의 [`[server.gateway]`](/ko/guides/gateway-login/) OAuth 화면은 **Claude Code용으로 구축된 디바이스 플로 로그인**이며 서로 다른 계약입니다. Claude Desktop에서 사용자별 SSO 어트리뷰션을 사용하려면 JWT 검증 프록시(LiteLLM, Kong, Envoy)를 shunt 앞에 두거나 사용자별 정적 토큰을 배포하세요.
:::

## 3. 모델 선택

shunt는 `GET /v1/models`를 제공하므로 Claude Desktop은 시작할 때 모델 선택기를 자동으로 디스커버리합니다. 무엇이 나타나는지는 두 가지 요소가 결정합니다.

**디스커버리 필터.** Claude Desktop의 자동 디스커버리는 *Claude로 인식할 수 있는* id, 즉 tier 이름 id(`claude-sonnet-*`, `claude-opus-*`, `claude-haiku-*`, `claude-fable-*`)만 표시합니다. shunt의 내장 카탈로그는 레퍼런스 Claude apps gateway를 정확히 미러링하며, 9개 id가 모두 tier 이름이므로 Claude Desktop에 전부 표시됩니다:

```json
// GET /v1/models — 내장 카탈로그(auto_include_builtin_models), 모두 tier 이름 id
// (각 항목에는 "type": "model"도 포함됨)
{ "data": [
  { "id": "claude-opus-4-6" },   { "id": "claude-sonnet-4-5-20250929" },
  { "id": "claude-haiku-4-5-20251001" }, { "id": "claude-fable-5" },
  { "id": "claude-opus-4-8" },   { "id": "claude-opus-4-7" },
  { "id": "claude-opus-4-1-20250805" },  { "id": "claude-sonnet-5" },
  { "id": "claude-sonnet-4-6" }
], "has_more": false, "first_id": null, "last_id": null }
```

선별된 `claude-<slug>-via-<provider>` 별칭(Claude Code에서 동작하는 패턴)은 **Claude Desktop에서 버려집니다**. [모델 디스커버리 → Claude Desktop은 tier 이름 id만 인식합니다](/ko/guides/model-discovery/#claude-desktop은-tier-이름-id만-인식합니다)를 참고하세요.

**비-Anthropic 백엔드 노출.** 두 가지 방법이 있습니다:

- `[[routes]]`의 `upstream_model`로 **tier 이름 id를 매핑**하면 Desktop에서 이를 선택할 때 해당 백엔드로 해석됩니다:

  ```toml
  [[routes]]
  model = "claude-sonnet-5"        # Claude Desktop이 인식하는 tier 이름 id
  provider = "codex"
  upstream_model = "gpt-5.6-sol"   # 실제 백엔드 슬러그
  ```

- shunt가 라우팅하는 정확한 id의 명시적 `inferenceModels` 목록으로 **Desktop 측 디스커버리를 오버라이드**하세요. 모든 항목이 전체 id이면 Claude Desktop은 `/v1/models` 호출을 건너뜁니다.

:::note[`anthropic_family_tier`는 아직 출력되지 않습니다]
`/v1/models` 항목에 `anthropic_family_tier` 필드(`sonnet` 같은 tier 이름)가 포함되어 있으면 Claude Desktop은 *불투명한* 별칭도 받아들입니다. 현재 shunt는 이 필드를 출력하지 않으므로([#211](https://github.com/pleaseai/shunt/issues/211)), tier 이름 id 또는 명시적인 `inferenceModels` 목록이 Desktop에 백엔드를 노출하는 현재 방법입니다.
:::

## 4. 검증

클라이언트 토큰으로 shunt가 디스커버리와 추론에 응답하는지 확인하세요:

```bash
# 디스커버리 — [server.auth]가 설정되어 있으면 토큰으로 게이팅됨
curl -s "$SHUNT_URL/v1/models" -H "Authorization: Bearer $SHUNT_CLIENT_TOKEN" | jq '.data[].id'

# 매핑한 tier 이름 id -> 백엔드로 우회
curl -s -X POST "$SHUNT_URL/v1/messages" \
  -H "Authorization: Bearer $SHUNT_CLIENT_TOKEN" \
  -H "anthropic-version: 2023-06-01" -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

그런 다음 Claude Desktop을 여세요. 모델 선택기에 tier 이름 항목이 표시되어야 합니다. 비어 있다면 디스커버리에서 필터링되었거나(tier 이름이 아닌 id) `/v1/models`에 접근할 수 없는 것입니다. 폴백으로 `inferenceModels`를 명시적으로 설정하세요.
