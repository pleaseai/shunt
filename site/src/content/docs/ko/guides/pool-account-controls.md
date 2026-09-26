---
title: 풀 계정 제어
description: 개별 풀 계정을 일시 정지하고, 사용 가능한 계정을 가장 빨리 리셋되는 순서로 정렬하기 — 둘 다 관리자 대시보드에서 런타임에, 설정 변경이나 재시작 없이.
---

계정 풀 선택([Anthropic 멀티 계정](/ko/guides/anthropic-multi-account/), [Codex 멀티 계정](/ko/guides/codex-multi-account/)) 위에는 두 가지 런타임 제어가 있습니다: 단일 계정 일시 정지, 그리고 사용 가능한 계정을 번-레이트 헤드룸 대신 가장 빨리 쿼터가 리셋되는 순서로 정렬하는 것입니다. 두 기능 모두 관리자 대시보드의 "Managed pool health" 표에서, 또는 관리자 API를 직접 호출해서 조작합니다. 둘 다 메모리 전용입니다 — 재시작하면 초기화되므로 `shunt.toml`은 전혀 건드리지 않습니다.

## 계정 일시 정지

일시 정지는 설정 쪽의 `disabled = true`와 마찬가지로 계정을 선택 대상에서 제외하지만, `shunt.toml`을 편집하거나 계정을 로그아웃시키지 않습니다. 자격 증명과 쿼터 이력은 그대로 유지되며, 일시 정지된 계정은 재개될 때까지 단순히 선택 후보로 나타나지 않습니다.

`disabled`와는 다릅니다:

| | `disabled` (설정) | `paused` (런타임) |
| :-- | :-- | :-- |
| 설정 방법 | `shunt.toml`, 리로드 필요 | 관리자 대시보드 또는 `PATCH /admin/api/pool/{provider}/accounts/{account_ref}` |
| 재시작 후 유지 | 예 | 아니오 |
| 사용 사례 | 배포에서 계정을 영구적으로 제외 | 설정 왕복 없이 잠시 계정을 빼두는 임시 운영 개입 |

대시보드에서: **Manage pool accounts → Managed pool health**를 열고 해당 계정 행의 **Pause**를 클릭합니다. 상태가 `paused`로 표시되며, **Resume**을 클릭하면 다시 복귀합니다. 두 버튼 모두 write 등급 관리자 세션이 필요합니다.

API를 직접 호출하는 경우 (write 등급 자격 증명 필요):

`account_ref`는 `GET /admin/api/pool`의 각 account 객체가 반환하는 불투명 식별자입니다. 표시 이름 `name` 대신 이 값을 사용하므로 같은 이름의 서로 다른 계정도 개별적으로 제어할 수 있습니다.

```bash
curl -X PATCH "$SHUNT_URL/admin/api/pool/anthropic/accounts/$ACCOUNT_REF" \
  -H "x-shunt-admin-token: $ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"paused": true}'
```

재개하려면 `"paused": false`로 설정하세요. 전체 엔드포인트 참조는 [`PATCH /admin/api/pool/{provider}/accounts/{account_ref}`](/ko/reference/endpoints/)를 참고하세요.

## 가장 빨리 리셋되는 순서로 정렬하기

`[server.pool] sort_by_reset`(기본값 `false`)은 *available* 계층의 정렬 방식을 바꿉니다: 가장 큰 예상 번-레이트 헤드룸 대신, 계정을 알려진 쿼터 리셋 시각이 가장 이른 순서(오름차순 — 가장 먼저 회복되는 계정을 먼저 시도하고, 리셋 신호가 없는 계정은 맨 뒤로 정렬)로 정렬합니다. 아이디어는 가장 먼저 보충될 계정을 먼저 소진시켜, 리셋이 늦은 계정은 예비로 남겨두는 것입니다.

`[server.pool]`은 — 따라서 이 설정도 — 프로바이더별이 아니라 프로세스 전체에 적용되므로, 이를 토글하면 풀링된 모든 프로바이더에 동시에 영향을 줍니다.

`shunt.toml`에서 설정:

```toml
[server.pool]
sort_by_reset = true
```

또는 풀 표 위의 대시보드 체크박스("Rank available accounts by soonest quota reset instead of burn-rate headroom")로 런타임에 토글하거나, 직접 호출:

```bash
curl -X PATCH "$SHUNT_URL/admin/api/pool" \
  -H "x-shunt-admin-token: $ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"sort_by_reset": true}'
```

런타임 토글은 해제되거나 프로세스가 재시작될 때까지 설정 파일의 값을 덮어쓰며, 그 이후에는 다시 설정 파일의 값이 적용됩니다. 재시작 없이 명시적으로 오버라이드를 해제하려면 `null`을 보내세요:

```bash
curl -X PATCH "$SHUNT_URL/admin/api/pool" \
  -H "x-shunt-admin-token: $ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"sort_by_reset": null}'
```

필드를 아예 생략하면 아무 효과가 없습니다 — 현재 오버라이드(또는 그 부재)를 그대로 둡니다; 명시적인 `null`만이 해제합니다.

`GET /admin/api/pool`은 유효한 값(오버라이드 또는 설정값)을 최상위 `sort_by_reset` 불리언으로 보고합니다. `[server.pool]` 자체가 없으면 이 설정은 전혀 효과가 없습니다 — 이 설정이 바꾸려는 레거시 선택 경로 자체가 실행되지 않기 때문입니다 — 따라서 `[server.pool]`이 존재하기 전까지는 런타임 토글과 `GET /admin/api/pool`이 보고하는 값 모두 아무 의미가 없습니다.
