---
title: 인바운드 Codex 엔드포인트
description: OpenAI Codex CLI 자체를 shunt로 향하게 하고 ChatGPT/Codex OAuth 계정 풀에 걸쳐 로드 밸런싱하기.
---

이 사이트의 다른 가이드는 모두 **Claude Code**를 다른 백엔드로 라우팅합니다. shunt는 그 반대 방향으로도 동작할 수 있습니다: 옵트인 원시 OpenAI Responses 패스스루로, **Codex CLI**가 자체 `base_url`을 shunt로 향하게 하고 ChatGPT/Codex OAuth 계정 풀에 걸쳐 로드 밸런싱되도록 합니다. 옵트인이므로 `[server.codex_endpoint]`가 없으면 관련 라우트가 하나도 등록되지 않고 shunt의 기본 HTTP 화면은 그대로 유지됩니다.

이는 [Codex 멀티 계정](/ko/guides/codex-multi-account/)과 동일한 계정 풀 위에 만들어졌습니다 — 선택, 쿨다운, 갱신은 변경 없이 공유됩니다. 정확한 페일오버 표와 리로드 시맨틱을 포함한 전체 명세는 [M11 동작 명세](https://github.com/pleaseai/shunt/blob/main/docs/m11-inbound-codex-endpoint.md)를 참고하세요.

엔드포인트 활성화, Codex CLI를 shunt로 향하게 하기, 클라이언트 인증, 계정 프로비저닝, 자격이 부여된 모델 선택까지 처음부터 끝까지의 설정 안내는 [Codex CLI 연결](/ko/guides/connect-codex-cli/)을 따르세요. 이 페이지는 *엔드포인트가 무엇을 하는지*에 집중하고, 그 가이드는 *어떻게 연결하는지*의 체크리스트입니다.

## 엔드포인트 활성화

```toml
[server.codex_endpoint]   # 모든 키 선택; 기본값 표시됨
provider = "codex"        # chatgpt_oauth 프로바이더여야 함
```

```bash
shunt check
shunt run
```

시작 검증은 알 수 없는 `provider`나 `auth = "chatgpt_oauth"`를 쓰지 않는 프로바이더를 거부합니다 — 이 엔드포인트는 운영자의 Codex bearer를 주입하므로 `chatgpt_oauth` 프로바이더만 자격이 있습니다. 모든 키와 기본값은 [구성 레퍼런스](/ko/reference/configuration/#servercodex_endpoint-선택)를, 등록된 라우트는 [HTTP 엔드포인트](/ko/reference/endpoints/)를 참고하세요.

## 클라이언트 analytics sink

Codex CLI는 제품 analytics도 base URL로 전송합니다. shunt는 CLI가 만들 수 있는 두 경로를 모두 받아들입니다:

- `POST /backend-api/codex/analytics-events/events`
- `POST /codex/analytics-events/events`

이 라우트들은 Responses 라우트와 동일한 `[server.auth]` 정책을 따르지만, 텔레메트리를 업스트림으로 전달하지는 않습니다. 풀링된 계정 하나를 고르면 클라이언트 이벤트가 그 계정에 잘못 귀속되기 때문입니다. 인증 후에는 본문이 잘못됐거나, 읽을 수 없거나, 너무 크더라도 항상 `200 {}`을 반환합니다.

페이로드와 이벤트 속성은 로깅되지도 내보내지지도 않습니다. shunt는 정제된 `event_type`만 옵트인 `shunt.codex_client_events` 카운터의 `event` 속성으로 기록합니다: 이름에는 소문자 ASCII 문자, 숫자, `.`, `_`, `-`가 최대 64바이트까지 올 수 있고, 유효하지 않은 이름은 `other`로, 해석되지 않은 배치는 `unparsed`가 됩니다. Sentry나 OpenTelemetry 메트릭이 활성화되어 있지 않다면 이는 순수한 폐기 sink입니다.

## Codex CLI를 shunt로 향하게 하기

Codex CLI는 사용하는 base URL이 무엇이든 항상 그 뒤에 `/responses`를 붙이므로, 다음 두 가지 `~/.codex/config.toml` 형태 모두 동작합니다:

**ChatGPT 백엔드의 base URL을 흉내 내기:**

```toml
chatgpt_base_url = "http://127.0.0.1:3001/backend-api/codex"
```

**또는 커스텀 모델 프로바이더**(최상위 `model_provider`가 이를 선택해야 하며, 그렇지 않으면 CLI는 내장 프로바이더를 유지합니다):

```toml
model_provider = "shunt"

[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
```

커스텀 프로바이더를 쓰면(CLI에 로컬 로그인이 필요 없도록 `requires_openai_auth = false`를 추가하세요) shunt를 가리키는 순간 Codex CLI 자체의 `~/.codex/auth.json`은 무의미해집니다 — 계정은 매 요청마다 shunt의 풀에서 옵니다. 반면 `chatgpt_base_url` 형태는 CLI를 ChatGPT 로그인 모드로 유지하므로 여전히 로컬 로그인이 필요하고, **게이팅되지 않은** 엔드포인트에서만 동작합니다: CLI의 ChatGPT bearer는 구성된 shunt 토큰이 아니므로 `[server.auth]`가 이를 거부합니다.

## 클라이언트 인증

shunt에 [`[server.auth]`](/ko/guides/shared-gateway/)가 구성되어 있다면 — 루프백을 넘어서는 모든 경우에 권장됩니다 — 클라이언트 토큰을 OpenAI 스타일 Bearer 키(`OPENAI_API_KEY` 또는 커스텀 프로바이더의 `env_key`, LiteLLM/llmgateway 방식)로 제시하**거나** `x-shunt-token` 헤더로 제시하세요:

```toml
# A. Bearer — 내장 openai 프로바이더. base URL은 ~/.codex/config.toml에 설정하고
#    OPENAI_BASE_URL 환경 변수로는 설정하지 마세요: 환경 변수를 쓰면 CLI의 Responses
#    WebSocket이 wss://api.openai.com을 계속 가리켜 shunt를 우회합니다. 연결 가이드의
#    "Codex CLI를 shunt로 향하게 하기"를 참고하세요.
openai_base_url = "http://127.0.0.1:3001/v1"
```

```bash
export OPENAI_API_KEY="<shunt-token>"      # Authorization: Bearer로 전송됨
```

```toml
# B. 헤더 — 커스텀 프로바이더가 이를 실어 보냅니다(파일 밖에 두려면 env_http_headers 사용):
[model_providers.shunt]
base_url = "http://127.0.0.1:3001/v1"
wire_api = "responses"
http_headers = { "x-shunt-token" = "<token>" }
```

`[server.auth]`가 없으면 엔드포인트는 도달할 수 있는 누구에게나 열려 있습니다 — 루프백이나 개인 용도에는 받아들일 만하지만 공유 게이트웨이에는 적절하지 않습니다. 클라이언트가 제시한 자격 증명은 shunt에 인증하는 데에**만** 사용됩니다: 이 값은(그리고 CLI가 보내는 어떤 `Authorization`이든) 제거되며 업스트림으로 전달되지 않습니다. `[server.admin]` 자격 증명 헤더(기본값 `x-shunt-admin-token`, 또는 `[server.admin] header`가 지정한 이름)도 제거됩니다 — 관리 화면이 바로 그 슬롯에서 인증하며, 관리 자격 증명은 업스트림 계정을 프로비저닝할 수 있기 때문입니다. `x-api-key`도 `[server.auth]`가 설정되지 않은 경우를 포함해 무조건 제거됩니다 — 대상 프로바이더는 부팅 시점에 `chatgpt_oauth` 전용으로 검증되므로, 인바운드 `x-api-key` 값은 이 업스트림에 대해 결코 유효한 자격 증명이 될 수 없습니다. Claude Code의 `apiKeyHelper`처럼 `Authorization`과 `x-api-key`에 같은 키를 넣는 클라이언트라도 두 번째 슬롯을 통해 그 키가 새어 나가지 않습니다. 인바운드 클라이언트가 실제 Codex CLI이므로, 패스스루는 그 요청 헤더를 그대로 전달하고(`version`, `originator`, `OpenAI-Beta`, `x-codex-*`, …) 선택된 풀 계정의 `Authorization` bearer와 `chatgpt-account-id`**만** 바꿔 넣습니다. 전체 인증 안내는 [Codex CLI 연결](/ko/guides/connect-codex-cli/#3-shunt-클라이언트-토큰-제시-serverauth가-설정된-경우)을 참고하세요.

## 계정 프로비저닝

[Codex 멀티 계정](/ko/guides/codex-multi-account/#풀-구성)과 동일한 풀을 재사용합니다:

```bash
codex login
shunt login codex --name main
```

```toml
[[providers.codex.accounts]]
name = "main"
```

`[[providers.codex.accounts]]`가 구성되지 않았고 **shunt 계정 스토어도 비어 있으면**, 엔드포인트는 기본 `~/.codex/auth.json` 자격 증명 하나로 폴백합니다 — 풀링도 페일오버도 없습니다 — 따라서 `[server.codex_endpoint]`를 설정하는 즉시 Codex 로그인 하나만으로도 동작합니다. (핸들러는 먼저 계정 스토어를 스캔해 발견한 계정을 풀에 넣으므로, 가져온 스토어 계정은 여전히 풀링을 활성화합니다.)

## `/v1/messages`와 다른 점

- **변환이 없습니다.** 인바운드 Responses 본문은 바이트 단위로 그대로 업스트림에 전달되고, 업스트림 응답도 — SSE든 JSON이든, 성공이든 오류든 — 그대로 릴레이됩니다(status와 `content-type` 보존). Anthropic Messages ⇄ Responses 변환 단계는 아예 없습니다.
- **압축된 요청 본문도 그대로 통과합니다.** 현재 Codex 릴리스는 ChatGPT 백엔드와 통신할 때 요청 본문을 zstd로 압축하며, 여기에는 이 엔드포인트를 가리키는 `chatgpt_base_url` 형태도 포함됩니다. 바이트와 그 `content-encoding: zstd` 헤더는 변경 없이 전달되고, shunt는 메트릭·로그·스팬에 쓸 요청의 `model`을 읽기 위해 메모리에서 사본만 추가로 디코딩합니다. shunt가 디코딩할 수 없는 본문도 릴레이에는 문제가 없으며 — `model` 레이블만 `unknown`으로 낮아지고, 그 이유를 밝히는 경고가 남습니다.
- **모델 기반 라우팅이 없습니다.** 모든 요청은 `[server.codex_endpoint]`에 지정된 단 하나의 프로바이더로 갑니다. 본문의 `model` 필드는 그대로 전달될 뿐 프로바이더를 선택하지 않습니다.
- **소진 시에도 그대로 릴레이합니다.** 풀링된 모든 계정을 시도했고 업스트림 응답이 최소 한 번 돌아왔다면, shunt는 그것을 Anthropic 형식의 오류로 다시 만들지 않고 마지막 응답을 변경 없이 릴레이합니다. Responses 클라이언트는 실제 ChatGPT 백엔드에서 받았을 원시 형태를 기대하기 때문입니다.
- **게이트웨이 소유 오류는 OpenAI 형식입니다.** 실패가 shunt 자신의 것일 때 — 잘못됐거나 없는 클라이언트 토큰(`401`), 업스트림 응답 없이 풀을 해석할 수 없는 경우(`502`), 지나치게 큰 요청 본문, 구성되지 않은 엔드포인트 — shunt는 이를 동일한 status 코드와 함께 OpenAI Responses 오류 형태(`{"error":{"message":…,"type":…,"code":null}}`)로 반환합니다. 그러면 Codex CLI가 Anthropic의 `{"type":"error",…}` 봉투가 아니라 자체 오류 경로로 이를 해석합니다. 릴레이되는 *업스트림* 오류(백엔드의 429/4xx/5xx)는 여전히 그대로 통과합니다.
- **HTTP/SSE 전용입니다.** 대상 프로바이더에 `websocket = true`가 설정되어 있어도 이 엔드포인트는 항상 HTTP 전송을 사용합니다.

## 보안

- 루프백을 넘어서는 모든 경우에 `[server.auth]`로 이 엔드포인트를 게이팅하세요 — 프로바이더가 매 요청마다 실제 Codex bearer를 주입합니다.
- 클라이언트 자신의 자격 증명은 어떤 것도 Codex 백엔드에 닿지 않습니다. 패스스루는 Codex CLI 자체의 요청 헤더를 그대로 전달하고 선택된 풀 계정의 bearer와 `chatgpt-account-id`만 바꿔 넣습니다(shunt 클라이언트 토큰 헤더, `[server.admin]` 자격 증명 헤더, 내부용 `x-shunt-inbound-client` 라벨, 클라이언트의 `Authorization`/`chatgpt-account-id`, 그리고 `x-api-key`는 모두 제거되며 전달되지 않습니다).
- 라우트 집합은 부팅 시 한 번 결정됩니다. 런타임에 `[server.codex_endpoint]`를 켜거나 끄면 재시작이 필요하다는 경고가 로깅됩니다. 다만 리로드로 대상 프로바이더를 바꾸는 것은 가능합니다.
