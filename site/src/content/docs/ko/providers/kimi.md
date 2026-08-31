---
title: Kimi (Moonshot)
description: MOONSHOT_API_KEY로 매핑된 모델을 Moonshot의 Anthropic 호환 Kimi 엔드포인트로 라우팅하거나, OAuth로 Kimi Code 구독을 재사용하기.
---

**Kimi**는 Moonshot AI의 모델 제품군으로 **Anthropic 호환** 엔드포인트를 통해 제공됩니다 —
shunt는 Moonshot API 키를 주입하고 Claude Code의 Messages 요청을 전달합니다. Anthropic이 아닌
업스트림 id는 지연 도구 필드가 제거됩니다(OpenRouter의 stealth 슬러그와 동일한 규칙). `kimi`
프리셋은 내장이므로, 구성은 업스트림 항목 하나에 라우트만 더하면 됩니다.

이 페이지는 서로 다른 자격 증명을 쓰는 두 개의 별도 Kimi 서비스를 다룹니다: 측정되는 Moonshot
API(`kimi` 프리셋, API 키, 아래)와 **Kimi Code** 구독(`kimi-code` 프리셋, OAuth 로그인, 이 페이지
맨 아래의 [Kimi Code (OAuth 구독)](#kimi-code-oauth-구독) 참고). 둘은 서로 다른
엔드포인트이며 교체할 수 없습니다.

## 빠른 시작

코딩 에이전트가 대신 구성하도록 하세요 — `shunt add`는 내장된 설정 블루프린트를 출력합니다
(오프라인·읽기 전용이며, 구성은 에이전트가 편집하고 이 명령은 절대 편집하지 않습니다):

```bash
shunt add upstream kimi --print | claude
```

또는 아래의 수동 단계를 따르세요.

## 업스트림 구성

`kimi` 프리셋은 `kind = "anthropic"`, `base_url = "https://api.moonshot.ai/anthropic"`,
그리고 `MOONSHOT_API_KEY`에서 오는 API 키 인증을 제공합니다:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 라우트가 없는 모델(예: claude-*)의 기본값으로 Anthropic을 유지

[[upstreams]]
name = "kimi"
provider = "kimi"

[[routes]]
model = "kimi-k3"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

순서가 있는 `[[upstreams]]`는 shunt의 내장 프로바이더를 대체하므로, `kimi`로 라우팅하는 구성은 여전히
가리키고 있는 `anthropic` 기본값도 함께 선언해야 합니다(`server.default_provider`의 기본값은
`anthropic`입니다). `anthropic` 항목을 빼려면 `default_provider`도 선언된 업스트림으로 설정해야
합니다.

레거시 `[providers.kimi]` 테이블 형식도 계속 지원됩니다(예전 예시는 `api_key_env = "KIMI_API_KEY"`를
사용했으며, 명시적으로 설정하면 여전히 동작합니다) — 다만 한 파일에서 `[[upstreams]]`와
`[providers.*]`를 섞지 마세요.

## 자격 증명

```bash
export MOONSHOT_API_KEY='...'
```

키를 구성 파일에 절대 쓰지 마세요. `shunt check`는 구성의 구조를 검증하지만 키의 값을 읽지는 않습니다 —
`MOONSHOT_API_KEY`가 설정되어 있지 않으면 `kimi`로 라우팅된 첫 요청이 인증 오류를 반환합니다.

## 모델

| 모델 id | 비고 |
| :-- | :-- |
| `kimi-k3` | 프런티어 티어이며, 클라이언트가 Claude Code의 `[1m]` 컨텍스트 마커를 덧붙일 수 있습니다(`kimi-k3[1m]`) — shunt가 매칭 전에 이를 제거하므로 접미사가 없는 id를 라우팅하세요 |
| `kimi-k2.7-code` | 코딩에 초점을 맞춘 티어 |

Claude Code에서 라우팅된 id는 `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, 또는 서브에이전트의
`model:` 프론트매터로 선택하세요. 대신 `/model` 선택기에 항목을 드러내려면, `[models.upstream_model]`
맵과 함께 `claude` 프리픽스가 붙은 별칭을 광고하세요 —
[모델 디스커버리](/ko/guides/model-discovery/)를 참고하세요.

## 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"kimi-k2.7-code","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `kimi`를 가리키는지 확인한 다음,
[Claude Code를 shunt로 지정](/ko/guides/connect-claude-code/)하세요.

## 서브에이전트 플러그인

[`shunt-kimi` 플러그인](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-kimi)은 위 모델마다
하나씩 미리 만들어진 Claude Code 서브에이전트를 제공합니다:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-kimi@shunt
```

## Kimi Code (OAuth 구독)

**Kimi Code**는 위의 측정되는 Moonshot API와는 별개의, 구독 결제 방식 서비스입니다 — 호스트가 다르고
(`api.moonshot.ai`가 아니라 `api.kimi.com`), 자격 증명도 다릅니다(`MOONSHOT_API_KEY`가 아니라 shunt가
관리하는 OAuth 토큰). 이 역시 Anthropic Messages 와이어 형태를 사용하므로 동일한 어댑터를 쓰며,
프리셋만 다릅니다: `kimi-code`.

### 빠른 시작

```bash
shunt add upstream kimi-code --print | claude
```

또는 아래의 수동 단계를 따르세요.

### 1. 로그인

```bash
shunt login kimi --name <account-name>
```

`--name`은 필수이며, 이 로그인에서는 `--mode`, `--long-lived`, `--manual`이 받아들여지지 않습니다 —
자격 증명은 언제나 갱신 가능하고 수동 붙여넣기 폴백은 없습니다. shunt는
[RFC 8628](https://www.rfc-editor.org/rfc/rfc8628) 디바이스 인가 그랜트를 실행합니다: URL과 짧은
코드를 출력하고, 사용자가 브라우저에서(이 기기에서든 다른 기기에서든) 승인하며, shunt는 승인이
완료되거나 코드가 만료될 때까지 폴링합니다. 저장된 계정은
`~/.shunt/accounts/kimi/<account-name>.json`에 놓이며(0700 디렉터리 안의 0600 파일),
`SHUNT_KIMI_ACCOUNTS_DIR`로 오버라이드할 수 있습니다.

Kimi는 갱신할 때마다 refresh 토큰을 회전시키고 액세스 토큰의 수명은 약 15분에 불과하므로, 갱신이
잦습니다. Kimi 계정 파일 하나당 shunt 프로세스 하나만 실행하세요 — 파일 하나를 공유하는 두 프로세스는
첫 갱신에서 서로를 무효화합니다. 대신 프로세스마다 별도의 계정을 프로비저닝하세요.

### 2. 업스트림 구성

`kimi-code` 프리셋은 `kind = "anthropic"`, `base_url = "https://api.kimi.com/coding"`,
그리고 `auth = "kimi_oauth"`를 제공합니다:

```toml
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# [[upstreams]]를 선언하면 내장 프로바이더 집합이 대체되므로, 맨 뒤에 anthropic
# 패스스루를 남겨 두세요 — 없으면 `shunt check`가 기본값인
# server.default_provider를 거부합니다. `shunt init`이 덧붙이는 것과 같은 항목입니다.
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

`kimi_oauth`는 `claude_oauth`, `chatgpt_oauth`와 똑같이 풀링을 지원합니다: `account` 대신
`accounts = [...]`를 사용해 이름이 있는 여러 계정을 한 업스트림 아래 풀링하거나(둘은 상호 배타적입니다),
둘 다 생략해 shunt가 관리하는 Kimi 계정 스토어 전체를 스캔하게 하세요.

### 모델

shunt는 Kimi Code 자체의 모델 목록 엔드포인트를 조회하지 않습니다 — `/v1/models`는 shunt의 내장
카탈로그에서 제공합니다. 구독이 실제로 자격을 가진 모델 id를 라우팅하세요:

```toml
[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

### 검증

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"<model-id-your-subscription-exposes>","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

응답의 `x-gateway-upstream` 헤더가 `kimi-code`를 가리키는지 확인하세요.

`"We're unable to verify your membership benefits at this time"`와 함께 오는
`402 Payment Required`는 로그인은 성공했지만 그 계정에 활성화된 Kimi Code 멤버십이 없다는 뜻입니다.
자격 증명은 문제가 없으며, 살펴봐야 할 것은 구독입니다.

### 풀링된 계정과 관리자 표면

`kimi_oauth` 풀은 Claude 및 Codex 풀과 동일한 부하 분산, 페일오버, 쿼터 인지 계정 로테이션에
참여하며, 활성화되어 있으면 그 계정들이 `GET /admin/pool`과 살균된 `GET /usage` 집계에 나타납니다.
다른 풀에는 없는 조건 하나에서 추가로 로테이션합니다: 위의 `402` 멤버십 응답입니다. 비활성 멤버십은
모든 요청에 402를 반환하므로, shunt는 이를 계정 수준의 실패로 취급합니다 — 정상적인 계정들이 놀고 있는
동안 클라이언트에 402를 건네는 대신, 그 계정을 쿨다운시키고 다음 계정을 시도합니다. 풀의 *모든* 계정이
비활성이면 Kimi 자신의 402 상태와 메시지를 그대로 돌려받으므로, 원인은 계속 드러나 있습니다.
[관리자 웹 표면](https://shunt.dev/guides/admin-remote-provisioning/)의 브라우저 기반 계정
프로비저닝은 Kimi 계정을 지원하지 않습니다 — 그 표면의 풀 뷰는 Kimi에 대해 읽기 전용이며, Kimi 계정은
CLI에서 `shunt login kimi`로 프로비저닝하세요.
