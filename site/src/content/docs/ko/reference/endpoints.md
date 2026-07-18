---
title: HTTP 엔드포인트
description: shunt가 Claude Code LLM 게이트웨이로서 제공하는 엔드포인트.
---

| 메서드 | 경로 | 용도 |
| :-- | :-- | :-- |
| `HEAD` | `/` | Liveness 프로브 |
| `GET` | `/` | 사람이 읽을 수 있는 랜딩(버전 + 엔드포인트 목록) |
| `GET` | `/health` | 헬스체크 — `{"status":"ok","version":"x.y.z"}` |
| `GET` | `/v1/models` | [모델 디스커버리](/ko/guides/model-discovery/) — `[[models]]` 항목을 반환 |
| `GET` | `/routes` | shunt 네이티브 라우트 디스커버리 — 구성된 `[[routes]]` 테이블을 그대로 반환(model → provider/upstream_model/effort 매핑, claude 프리픽스 디스커버리 별칭 포함); 더 좁은 Anthropic 프로토콜 디스커버리 응답(`id`/`display_name`만)을 제공하는 `/v1/models`와 구별됨 |
| `POST` | `/v1/messages` | 추론 — 요청의 `model` id에 따라 라우팅 |
| `POST` | `/v1/messages/count_tokens` | [토큰 카운팅](/ko/guides/effort-and-context/#token-counting-count_tokens) |
| `GET` | `/admin` | 관리자 대시보드(HTML); 로그인하지 않았으면 `/admin/login`으로 리다이렉트 |
| `GET`, `POST` | `/admin/login` | 관리자 토큰 로그인 폼과 브라우저 세션 생성 |
| `POST` | `/admin/logout` | 브라우저 세션 삭제 |
| `GET` | `/admin/accounts` | Claude 계정 스토어 메타데이터: 이름, 종류, 만료, UUID; 토큰 자체는 절대 반환하지 않음 |
| `GET` | `/admin/accounts/codex` | Codex 계정 스토어 메타데이터: 이름, 만료, ChatGPT 계정 ID; 토큰 자체는 절대 반환하지 않음 |
| `GET` | `/admin/pool` | `claude_oauth` 및 `chatgpt_oauth` 프로바이더별 풀 상태; Codex는 쿼터 헤더가 없어 사용률 필드가 비어 있음 |
| `POST` | `/admin/accounts/claude` | `{name, mode}`로 Claude 브라우저 프로비저닝 시작. `mode`는 `oauth` 또는 `setup_token`이며, 생략하면 `setup_token`; `{authorize_url}` 반환 |
| `POST` | `/admin/accounts/claude/{name}/complete` | `<code>#<state>`가 담긴 `{code}`로 Claude 프로비저닝 완료; 계정을 저장하고 실제 사용 여부(live)를 보고 |
| `DELETE` | `/admin/accounts/claude/{name}` | 해당 이름 Claude 계정의 스토어 파일 제거 |
| `POST` | `/admin/accounts/codex` | `{name}`으로 ChatGPT OAuth 시작; `{authorize_url}` 반환 |
| `POST` | `/admin/accounts/codex/{name}/complete` | 전체 localhost redirect URL 또는 `<code>#<state>`가 담긴 `{code}`로 Codex 프로비저닝 완료 |
| `DELETE` | `/admin/accounts/codex/{name}` | 해당 이름 Codex 계정의 스토어 파일 제거 |
| `POST` | `/backend-api/codex/responses` | 인바운드 Codex CLI 패스스루 — 실제 ChatGPT 백엔드 경로 미러 |
| `POST` | `/responses` | 인바운드 Codex CLI 패스스루 — bare `base_url` 형식 |
| `POST` | `/v1/responses` | 인바운드 Codex CLI 패스스루 — `/v1` 접미 `base_url` 형식 |
| `POST` | `/backend-api/codex/analytics-events/events` | Codex CLI 분석 sink — 수락 후 폐기하고 정제된 이벤트 이름 카운터만 기록 |
| `POST` | `/codex/analytics-events/events` | Codex CLI 분석 sink — 루트형 `chatgpt_base_url` 형식 |

`/admin*` 라우트는 [`[server.admin]`](/ko/reference/configuration/#serveradmin-선택)이 구성된 경우에만 존재합니다; 그 테이블이 없으면 하나도 등록되지 않습니다.

인바운드 Codex Responses 및 분석 라우트는 [`[server.codex_endpoint]`](/ko/reference/configuration/)가 구성된 경우에만 존재합니다. Responses 라우트는 OpenAI Responses 요청과 응답을 그대로 중계합니다. 두 분석 라우트는 같은 인바운드 인증 정책을 적용하고, 클라이언트 payload를 전달하거나 보관하지 않으며, 인증 후에는 잘못된 JSON이나 초과 크기 본문에도 `200 {}`를 반환합니다. 정제된 이벤트 이름만 `shunt.codex_client_events`에 기록되며, 메트릭 sink가 없으면 순수 폐기 sink로 동작합니다.

`GET /`와 `GET /health`는 [`[server.auth]`](/ko/guides/shared-gateway/)가 활성화되어 있어도 열린 채로 유지되며(헬스체크 도구는 보통 토큰을 첨부할 수 없음) 민감한 것을 노출하지 않습니다 — 오직 상태, 버전, 그리고 이미 공개된 엔드포인트 목록만입니다.

## 게이트웨이 프로토콜

shunt는 공식 [Claude Code LLM 게이트웨이 프로토콜](https://code.claude.com/docs/en/llm-gateway-protocol)을 구현합니다: 올바른 헤더 및 바디 필드 전달, 기능 패스스루, 시스템 프롬프트 어트리뷰션 처리. 게이트웨이 소유 오류는 Anthropic 오류 형태로 반환되고, 업스트림 컨텍스트 오버플로 오류는 Anthropic의 `prompt is too long` 표현으로 다시 쓰여 Claude Code의 [압축-재시도](/ko/guides/effort-and-context/#context-overflow-recovery)가 발동하며, 스트리밍 응답은 버퍼링 없이 릴레이됩니다(선택적 [keepalive ping](/ko/guides/shared-gateway/#sse-keepalive-pings) 포함).
