---
title: 관리자 & 원격 프로비저닝
description: shunt의 관리자 웹 화면을 활성화해 Claude 계정을 원격으로 프로비저닝하고 계정 풀 상태를 점검합니다.
---

shunt는 업스트림 Claude 계정을 프로비저닝하고 각 `claude_oauth` 계정 풀의 상태를 볼 수 있는, 관리자 인증이 걸린 웹 화면을 노출할 수 있습니다. 이는 옵트인입니다: `[server.admin]`이 없으면 `/admin*` 라우트는 하나도 등록되지 않으며 shunt의 기본 HTTP 표면(surface)은 그대로입니다.

이 기능은 [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/)의 스토어와 선택 동작 위에 구축됩니다. 브라우저 플로우는 1년짜리 추론 전용 setup token 계정을 만듭니다; 갱신 가능한 Claude Code 로그인 가져오기는 CLI 전용으로 남습니다.

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

## 브라우저에서 계정 프로비저닝

1. `/admin`을 열고 관리자 토큰으로 로그인합니다.
2. 소문자, 숫자, 하이픈만 포함하는 계정 이름을 입력하고 **Start**를 선택합니다.
3. 표시된 인가 URL을 다른 탭에서 엽니다. 대상 Claude 계정으로 로그인하고 접근을 승인합니다.
4. 결과로 나온 `<code>#<state>` 값을 관리자 페이지에 붙여 넣고 **Complete**를 선택합니다.
5. shunt가 계정을 저장합니다. `accounts` 목록이 비어 있는 프로바이더는 재시작 없이 다음 요청에서 그 계정을 집어 듭니다. 그렇지 않으면 이름만 있는 항목을 추가하고 리로드하세요:

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

시작된 플로우는 `pending_ttl_secs`(기본 10분) 동안 유효해서, 운영자가 인가 페이지를 열고 결과를 붙여 넣을 시간을 확보할 수 있습니다. 완료 응답은 계정이 저장됐는지, 그리고 현재 프로바이더 구성에서 그 계정이 실제로 사용되는지(live)를 알려 줍니다.

계정 스토어 변경은 요청마다 감지되므로, 스캔 모드 프로바이더는 계정이 추가되거나 제거된 뒤 재시작할 필요가 없습니다.

## 풀 상태 점검

대시보드는 `auth = "claude_oauth"`로 구성된 각 프로바이더의 계정 스토어 메타데이터와 현재 상태를 보여 줍니다. 업스트림 응답에서 관측된 5시간, 공유 7일, `7d_oi` 사용률과 함께 통합(unified) status, 남은 쿨다운, 쿼터 근접 상태, 그리고 계정이 현재 사용 가능한지가 포함됩니다.

계정 목록은 메타데이터만 노출합니다: 계정 이름, 자격 증명 종류(`setup_token` 또는 `imported`), 만료, UUID. 토큰 자체는 절대 반환하지 않습니다. shunt가 계정을 고를 때 쿼터 상태, 쿨다운, 모델 인지 주간 버킷을 어떻게 쓰는지는 [Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/#선택과-선제-로테이션)을 참고하세요.

계정 메타데이터, 풀 상태, 계정 제거에 API/curl로 접근하려면 구성된 헤더(기본 `x-shunt-admin-token`)로 관리자 토큰을 보내고 [HTTP 엔드포인트](/ko/reference/endpoints/)에 문서화된 JSON 라우트를 사용하세요. 헤더로 인증된 요청은 브라우저 세션을 쓰지 않으며 CSRF 검사에서 제외됩니다; setup token 프로비저닝은 위의 대시보드 플로우로 수행하세요.

## SSH와 갱신 가능 가져오기 폴백

shunt 호스트에 브라우저로 접근할 수 없거나 갱신 가능한 가져온 로그인이 필요하면 CLI를 사용하세요. SSH에서는 장기(long-lived) 플로우가 노트북에서 열 수 있는 인가 URL을 출력하고, 결과 코드를 원격 터미널에서 다시 받습니다:

```bash
shunt login claude --name backup --long-lived
```

대신 호스트의 현재 갱신 가능한 Claude Code 로그인을 가져오려면 `--long-lived`를 빼세요:

```bash
shunt login claude --name primary
```

브라우저 관리자 플로우는 의도적으로 setup token 프로비저닝만 지원합니다. 갱신 가능한 가져오기는 호스트의 Claude Code 자격 증명을 읽으므로 CLI 전용으로 유지됩니다.

## 보안

- 관리자 화면은 HTTPS 또는 WireGuard·Tailscale 같은 신뢰할 수 있는 터널 뒤에 두세요. shunt 자체는 평문 HTTP를 제공합니다; 원격에 노출할 때는 앞단에서 TLS를 종료하세요.
- 강한 관리자 토큰을 생성하고 `[server.auth]` 클라이언트 자격 증명과 분리해 두세요. 관리자 접근은 업스트림 계정을 추가·제거할 수 있습니다.
- 브라우저 로그인은 HttpOnly, SameSite=Strict 세션 쿠키를 만듭니다. 쿠키는 루프백 호스트를 제외하고 Secure이므로 로컬 HTTP 개발은 계속 동작합니다.
- 상태를 바꾸는 브라우저 요청은 세션별 `x-csrf-token`을 요구하고 동일 오리진 검사를 통과해야 합니다. API/curl 호출은 대신 관리자 헤더로 인증하며 암묵적(ambient) 쿠키 권한을 갖지 않습니다.
- 프로비저닝 완료에는 레이트 리밋이 적용됩니다. shunt는 토큰 자체를 절대 로깅하거나 반환하지 않으며, 계정 추가와 제거는 계정 이름으로 감사 로그에 남습니다.

`[server.admin]`이 없으면 그 라우트들은 존재하지 않습니다. 이는 사용하지 않는 대시보드를 인증 없이 두는 것보다 강력합니다: 명시적으로 활성화하지 않는 한 관리자 화면 자체가 없습니다.
