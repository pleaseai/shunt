---
title: 관리자 & 원격 프로비저닝
description: shunt의 관리자 웹 화면을 활성화해 Claude와 Codex 계정을 원격으로 프로비저닝하고 계정 풀 상태를 점검합니다.
---

shunt는 업스트림 Claude와 Codex/ChatGPT 계정을 프로비저닝하고 `claude_oauth` 및 `chatgpt_oauth` 계정 풀의 상태를 볼 수 있는, 관리자 인증이 걸린 웹 화면을 노출할 수 있습니다. 이는 옵트인입니다: `[server.admin]`이 없으면 `/admin*` 라우트는 하나도 등록되지 않으며 shunt의 기본 HTTP 표면(surface)은 그대로입니다.

이 기능은 [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/)의 스토어와 선택 동작 위에 구축됩니다. 브라우저 폼에서는 갱신 가능한 Full OAuth 계정 또는 1년짜리 추론 전용 setup token 계정을 만들 수 있습니다. 기존 Claude Code credential 파일 가져오기는 CLI 전용입니다.

## 관리자 화면 활성화

선택적 테이블을 추가하고, 구성된 환경 변수를 통해 관리자 자격 증명을 하나 이상 제공합니다:

```toml
[server.admin]                        # 모든 키 선택; 기본값 표시됨
header = "x-shunt-admin-token"
tokens_env = "SHUNT_ADMIN_TOKENS"
session_ttl_secs = 3600
pending_ttl_secs = 600
```

```bash
export SHUNT_ADMIN_TOKENS="ops:$(openssl rand -hex 32)"
shunt check
shunt run
```

자격 증명은 `SHUNT_CLIENT_TOKENS`와 같은 쉼표 구분 `name:token` 형식을 쓰지만, 별도의 보안 경계입니다. `[server.auth]` 클라이언트 토큰을 관리자 토큰으로 재사용하지 마세요. `[server.admin]`이 있는데 토큰 환경 변수가 설정되지 않았거나, 비어 있거나, 형식이 잘못되면 시작은 닫힌 채로 실패(fail closed)합니다.

모든 키와 기본값은 [설정 레퍼런스](/ko/reference/configuration/#serveradmin-선택)를 참고하세요. 브라우저 라우트와 JSON 라우트는 [엔드포인트 레퍼런스](/ko/reference/endpoints/)에 나열되어 있습니다.

## 브라우저에서 Claude 계정 프로비저닝

1. `/admin`을 열고 관리자 토큰으로 로그인합니다.
2. 소문자, 숫자, 하이픈만 포함하는 계정 이름을 입력합니다.
3. **Full OAuth (refreshable)**(대시보드 기본값) 또는 **Setup token (1-year, inference-only)**을 선택한 뒤 **Start**를 선택합니다.
4. 표시된 인가 URL을 다른 탭에서 엽니다. 대상 Claude 계정으로 로그인하고 접근을 승인합니다.
5. 결과로 나온 `<code>#<state>` 값을 관리자 페이지에 붙여 넣고 **Complete**를 선택합니다.
6. shunt가 계정을 저장합니다. `accounts` 목록이 비어 있는 provider는 재시작 없이 다음 요청에서 그 계정을 집어 듭니다. 그렇지 않으면 이름만 있는 항목을 추가하고 reload하세요:

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

시작된 플로우는 `pending_ttl_secs`(기본 10분) 동안 유효해서 운영자가 인가 페이지를 열고 결과를 붙여 넣을 시간을 확보할 수 있습니다. 서버는 선택한 mode를 pending attempt와 함께 기록하므로 completion 요청에서 token 종류를 바꿀 수 없습니다. Full OAuth는 access token과 refresh token을 저장하며 credential kind는 `imported`로 표시됩니다. setup-token mode는 kind가 `setup_token`인 정적 credential을 저장합니다. 완료 응답은 계정이 저장됐는지, 그리고 현재 provider 구성에서 그 계정이 실제로 사용되는지(live)를 알려 줍니다.

계정 스토어 변경은 요청마다 감지되므로, 스캔 모드 프로바이더는 계정이 추가되거나 제거된 뒤 재시작할 필요가 없습니다.

## 브라우저에서 Codex 계정 프로비저닝

1. **Add Codex account**에서 소문자 계정 이름을 입력하고 **Start Codex login**을 선택합니다.
2. 인가 URL을 열고 대상 ChatGPT 계정으로 로그인한 뒤 접근을 승인합니다.
3. 브라우저는 `http://localhost:1455/auth/callback`으로 이동합니다. 로컬 페이지가 열리지 않는 것은 정상입니다.
4. 브라우저 주소창의 **전체 URL**을 복사해 관리자 페이지에 붙여 넣고 **Complete Codex login**을 선택합니다. JSON API에서는 `<code>#<state>`도 사용할 수 있습니다.
5. shunt가 code를 교환하고 갱신 가능한 Codex 자격 증명을 비공개 계정 파일에 저장합니다.

`accounts`가 비어 있는 `chatgpt_oauth` 프로바이더(기본 `codex` 포함)는 다음 요청에서 새 계정을 발견합니다. 명시적 계정 목록을 쓰는 경우 이름만 있는 항목을 추가해야 합니다. `SHUNT_CODEX_TOKEN_URL`은 로컬 통합 테스트용 token endpoint override이며, 운영에서는 설정하지 마세요.

## 풀 상태 점검

대시보드는 `auth = "claude_oauth"` 또는 `auth = "chatgpt_oauth"`로 구성된 프로바이더의 계정 스토어 메타데이터와 현재 상태를 보여 줍니다. Claude 행에는 업스트림에서 관측한 쿼터 사용률이 상태·쿨다운 상태와 함께 표시됩니다. 업스트림이 리셋 시각을 보고한 경우 각 윈도우 셀에 리셋까지 남은 시간(예: `3d 4h`)도 함께 표시되며, 마우스를 올리면 절대 리셋 시각을 볼 수 있습니다. Codex 행에는 `x-codex-*` 응답 헤더로 보고된 5시간·7일 윈도우가 표시되며, 지원하지 않는 윈도우는 무시되고 Codex에는 대응 창이 없어 `7d_oi`는 `—`로 남습니다. Codex 사용량도 Claude 사용량과 똑같이 쿼터 인지 계정 선택에 반영됩니다(이슈 #195).

계정 목록은 메타데이터만 노출합니다: 계정 이름, 자격 증명 종류(`setup_token` 또는 `imported`), 만료, UUID. 토큰 자체는 절대 반환하지 않습니다. shunt가 계정을 고를 때 쿼터 상태, 쿨다운, 모델 인지 주간 버킷을 어떻게 쓰는지는 [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/#선택과-선제-로테이션)을 참고하세요.

계정 metadata, 풀 상태, 프로비저닝, 또는 계정 제거에 API/curl로 접근하려면 구성된 헤더(기본 `x-shunt-admin-token`)로 관리자 토큰을 보내고 [HTTP 엔드포인트](/ko/reference/endpoints/)에 문서화된 JSON route를 사용하세요. 헤더로 인증된 요청은 브라우저 세션을 쓰지 않으며 CSRF 검사에서 제외됩니다. 프로비저닝을 시작할 때 `{ "name": "backup", "mode": "oauth" }` 또는 `{ "name": "backup", "mode": "setup_token" }`을 보내세요. `mode`를 생략하면 API 하위 호환을 위해 `setup_token`이 기본값입니다.

## CLI와 SSH fallback

shunt 호스트에 브라우저로 접근할 수 없으면 CLI를 사용하세요. Full OAuth는 보통 브라우저를 열고 임시 `127.0.0.1` callback으로 완료합니다. SSH 또는 headless 환경에서는 관리자 페이지와 같은 수동 붙여넣기 redirect를 강제하세요:

```bash
shunt login claude --name backup --mode oauth --manual
```

대신 호스트의 현재 갱신 가능한 Claude Code 로그인을 가져오려면:

```bash
shunt login claude --name primary --mode import
```

1년짜리 추론 전용 credential을 만들려면:

```bash
shunt login claude --name ci --mode setup-token
```

`--long-lived`는 `--mode setup-token`의 deprecated alias입니다. 관리자 화면은 Claude Full OAuth/setup-token과 Codex ChatGPT OAuth 프로비저닝을 지원합니다. 기존 credential 파일 import만 호스트 접근이 필요해 CLI 전용입니다.

:::caution[Refresh token 회전]
갱신 가능한 계정에는 활성 owner가 하나만 있어야 합니다. OAuth 갱신은 refresh token을 교체하고 오래된 복사본을 무효화할 수 있으므로, 한 스토어 파일을 프로세스끼리 공유하거나 다른 호스트로 복사해 독립적으로 실행하지 마세요. 프로세스마다 별도로 프로비저닝하거나, 갱신 불가능한 정적 credential이 적합한 경우 setup-token mode를 선택하세요.
:::

## 보안

- 관리자 화면은 HTTPS 또는 WireGuard·Tailscale 같은 신뢰할 수 있는 터널 뒤에 두세요. shunt 자체는 평문 HTTP를 제공합니다; 원격에 노출할 때는 앞단에서 TLS를 종료하세요.
- 강한 관리자 토큰을 생성하고 `[server.auth]` 클라이언트 자격 증명과 분리해 두세요. 관리자 접근은 업스트림 계정을 추가·제거할 수 있습니다.
- 브라우저 로그인은 HttpOnly, SameSite=Strict 세션 쿠키를 만듭니다. 쿠키는 루프백 호스트를 제외하고 Secure이므로 로컬 HTTP 개발은 계속 동작합니다.
- 상태를 바꾸는 브라우저 요청은 세션별 `x-csrf-token`을 요구하고 동일 오리진 검사를 통과해야 합니다. API/curl 호출은 대신 관리자 헤더로 인증하며 암묵적(ambient) 쿠키 권한을 갖지 않습니다.
- 프로비저닝 완료에는 레이트 리밋이 적용됩니다. shunt는 토큰 자체를 절대 로깅하거나 반환하지 않으며, 계정 추가와 제거는 계정 이름으로 감사 로그에 남습니다.

`[server.admin]`이 없으면 그 라우트들은 존재하지 않습니다. 이는 사용하지 않는 대시보드를 인증 없이 두는 것보다 강력합니다: 명시적으로 활성화하지 않는 한 관리자 화면 자체가 없습니다.
