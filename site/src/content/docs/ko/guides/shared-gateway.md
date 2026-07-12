---
title: 게이트웨이 공유
description: 공유 배포를 위한 클라이언트별 토큰, 그리고 프록시와 터널을 위한 SSE keepalive ping.
---

## 인바운드 클라이언트 토큰

기본적으로 shunt에는 인바운드 인증이 없습니다 — 루프백 전용 개인 게이트웨이에는 괜찮지만, VPN/터널을 통해 공유하는 순간, 그것에 도달할 수 있는 누구든 매핑된 모델에서 **운영자의** 계정을 쓸 수 있습니다(shunt가 그런 모델에 대해 자체 `api_key`/`chatgpt_oauth` 자격 증명을 주입하기 때문). 패스스루 모델은 문제가 되지 않습니다: 각 호출자 본인의 Anthropic 자격 증명을 전달합니다.

`[server.auth]`는 주입된 자격 증명 라우트만을 클라이언트별 토큰으로 게이팅합니다:

```toml
[server.auth]                        # 두 키 모두 선택; 기본값 표시됨
header = "x-shunt-token"
tokens_env = "SHUNT_CLIENT_TOKENS"
```

```bash
# 게이트웨이 측: name:token 쌍 (이름은 로깅용 레이블; 토큰은 비밀)
export SHUNT_CLIENT_TOKENS="minsu:$(openssl rand -hex 32),alice:$(openssl rand -hex 32)"
```

`[server.auth]`가 있는데 env 변수가 설정되지 않았거나 형식이 잘못되면 시작은 **닫힌 채로 실패(fail closed)**합니다. 유효한 토큰 없이 매핑된 모델에 대한 요청은 401 `authentication_error`를 받습니다; `GET /v1/models`, `GET /routes`, `GET|HEAD /`, `GET /health`, 그리고 패스스루 모델은 열린 채로 유지됩니다. `GET /routes`는 `GET /v1/models`와 동일한 디스커버리 엔드포인트 설계에 따라 인증되지 않습니다 — 라우팅 메타데이터(구성된 프로바이더/업스트림 모델 매핑)를 노출하며, 자격 증명은 절대 노출하지 않습니다. 자격 증명은 오직 프로바이더 구성에만 존재하며 그 핸들러가 읽는 일이 없습니다.

토큰 헤더는 전달 전에 항상 제거되고, 매칭은 상수 시간(constant-time)이며, 토큰 값은 절대 로깅되지 않습니다(클라이언트 *이름*은 요청별로 로깅됩니다).

클라이언트 측은 한 줄입니다(`ANTHROPIC_CUSTOM_HEADERS`는 한 줄당 하나의 `Name: Value`를 받습니다):

```bash
export ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <your token>"
```

:::note
이는 애플리케이션 계층 식별일 뿐입니다 — 전송 암호화는 여전히 배포(WireGuard/Tailscale 터널, 또는 앞단의 TLS 종료)에서 옵니다; shunt 자체는 평문 HTTP를 제공합니다.
:::

## SSE keepalive ping

미들박스는 조용한 스트림을 끊습니다 — Cloudflare의 프록시는 **한 바이트도 없이 100초가 지나면 524를 반환**하며(Enterprise 미만에서는 고정), 긴 추론 구간은 그만큼 조용할 수 있습니다. 그래서 shunt는 스트리밍 응답이 유휴 상태일 때마다 Anthropic 프로토콜 자체의 `ping` 이벤트를 주입합니다(`api.anthropic.com`이 직접 방출하며 모든 클라이언트가 무시하는 것):

```toml
[server]
sse_keepalive_seconds = 30   # 기본값; 0은 비활성화
```

Ping은 완전한 SSE 이벤트 사이에서만(절반만 보낸 프레임 안에서는 절대 안 됨), `text/event-stream` 응답에서만 주입되며, 업스트림 스트림과 함께 멈춥니다. 유휴 타임아웃이 없는 터널(WireGuard/Tailscale) 뒤에서는 ping이 무해합니다; 바이트 단위로 동일한 릴레이를 원한다면 `0`으로 비활성화하세요.
