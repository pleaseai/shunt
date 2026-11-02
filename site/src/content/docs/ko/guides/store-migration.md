---
title: 저장소로 마이그레이션
description: 풀 쿼터, 게이트웨이 refresh token, 지출 한도를 JSON state_path 스냅샷에서 SQLite 또는 PostgreSQL 저장소로 옮기고, 일회성 가져오기가 모든 스냅샷을 읽었는지 확인합니다.
---

[`[store]`](/ko/reference/configuration/#store-선택)가 없으면 shunt는 풀 쿼터, 게이트웨이 refresh token, 지출 한도를 하위 시스템마다 `state_path` 하나씩, 총 세 개의 JSON 스냅샷에 보관합니다. `[store]`를 추가하면 이 스냅샷이 트랜잭션 데이터베이스 하나로 대체됩니다. 별도의 마이그레이션 명령은 없습니다. `[store]`를 설정한 뒤 처음 실행하는 `shunt run`이 각 스냅샷을 한 번 가져오고, 그 후에는 해당 하위 시스템을 저장소에서 제공합니다.

가져오기는 하위 시스템마다 한 번만 실행되며 반복할 수 없습니다. 첫 부팅 전에 경로를 정확히 맞춰 두세요. 그때 찾지 못한 스냅샷은 이후에도 가져오지 않습니다.

## 가져오는 항목

| 하위 시스템 | 스냅샷 | 가져오는 조건 |
| :-- | :-- | :-- |
| 풀 쿼터 | `[server.pool] state_path` (기본값 없음) | `[store]`가 설정됨 |
| 게이트웨이 refresh token | `[server.gateway] state_path`, 기본값 `~/.shunt/gateway-sessions.json` | `[server.gateway]`가 설정됨 |
| 지출 한도와 감사 기록 | `[server.spend] state_path`, 기본값 `~/.shunt/gateway-spend.json` | `[server.spend]`가 설정됨 |

설정되지 않은 하위 시스템은 건너뛰며 기록도 남기지 않습니다. 따라서 `[server.gateway]`나 `[server.spend]`를 나중에 켜면 그 뒤 첫 부팅에서 해당 스냅샷을 가져옵니다.

일부 상태는 저장소 밖에 남습니다. provider 자격 증명 파일, 관리자 브라우저 세션, OIDC 상태, 쿨다운, 그리고 `<stem>.counters.json`에 있는 지출 미터 카운터가 해당합니다.

## 1. 스냅샷 찾기와 백업

설정된 각 하위 시스템의 `state_path`를 위의 기본값을 포함해 나열하고, shunt 프로세스가 찾을 위치에 파일이 있는지 확인하세요. 기본값은 shunt를 실행하는 사용자의 홈 디렉터리를 기준으로 해석되므로, systemd나 컨테이너에서는 현재 사용자의 홈 디렉터리와 다를 수 있습니다. shunt는 홈 디렉터리를 `HOME`에서 읽고, 없으면 `USERPROFILE`을 씁니다. 둘 다 설정되지 않으면 게이트웨이와 지출 하위 시스템에는 기본 스냅샷이 없으므로, 첫 부팅이 `NoSnapshot`을 기록하고 이후에도 가져오지 않습니다. 이 경우 각 `state_path`를 명시적으로 설정하세요.

파일을 안전한 곳에 복사해 두세요. shunt는 가져오는 동안에만 이 파일을 읽고 저장소가 연결된 동안에는 쓰지 않지만, 마이그레이션 이전 상태를 나중에 확인할 수 있는 방법은 이 복사본뿐입니다.

## 2. `[store]` 추가

`state_path` 키는 모두 그대로 두세요. `[store]`가 설정되면 이 키가 가져올 위치를 shunt에 알려 줍니다.

```toml
[store]
backend = "sqlite"
url = "/var/lib/shunt/shunt.db"
```

PostgreSQL은 `backend = "postgres"`와 `postgres://` URL을 설정합니다. 루프백이 아닌 호스트에는 `sslmode=verify-full`이 필요합니다. 모든 키는 [`[store]` 레퍼런스](/ko/reference/configuration/#store-선택)에 있습니다.

## 3. `shunt check` 실행

```bash
shunt check
```

```text
config ok
store ok: sqlite sqlite:/var/lib/shunt/shunt.db, file does not exist yet; it is created with mode 0600 at boot
```

`check`는 저장소를 읽기 전용으로 열며 마이그레이션, 가져오기, SQLite 파일 생성을 하지 않으므로 실행 중인 인스턴스를 중지하기 전에 실행해도 됩니다. 설정 오류나 연결할 수 없는 저장소는 부팅 시점이 아니라 여기서 실패합니다.

## 4. 실행 중인 인스턴스 중지

현재 shunt를 정상 종료하세요. 저장소가 없을 때 refresh token과 지출 한도는 바뀔 때마다 스냅샷에 기록되므로 이미 최신 상태입니다. 풀 쿼터는 다릅니다. 저장소가 없는 shunt는 풀 쿼터를 15초마다 저장하고 종료할 때 다시 저장하지 않으므로, 스냅샷에 마지막 최대 15초 동안의 쿼터 변경이 빠질 수 있습니다. 쿼터는 업스트림 응답으로 다시 계산되므로 영향을 받는 계정이 잠깐 콜드 스타트하는 정도의 비용입니다. 이마저 피하려면 트래픽을 멈추고 15초 기다린 뒤 shunt를 중지하세요. 저장소가 없는 프로세스가 가져오기 이후에 스냅샷에 쓴 내용은 저장소에 반영되지 않으므로, 기존 인스턴스가 종료되기 전에는 새 인스턴스를 시작하지 마세요.

## 5. 저장소와 함께 shunt 시작

```bash
shunt run
```

shunt는 요청을 처리하기 전에 데이터베이스를 만들거나 마이그레이션하고, 하위 시스템마다 하나의 트랜잭션으로 각 스냅샷을 가져온 뒤, 저장소에서 상태를 복원합니다. 시작 로그 줄 `store opened; state is served from it`에 각 하위 시스템의 결과가 표시됩니다.

| 결과 | 의미 |
| :-- | :-- |
| `Imported { rows: N }` | 스냅샷을 저장소로 복사함 |
| `NoSnapshot` | 경로가 설정되지 않았거나 파일이 없음. 기록이 남았으므로 **이 하위 시스템은 다시 가져오지 않음** |
| `AlreadyImported` | 이전 부팅에서 이미 이 하위 시스템을 처리함 |
| `Off` | 하위 시스템이 설정되지 않아 저장소를 사용하지 않음 |

가져오기가 실패하면 `importing the <subsystem> state snapshot <path> into the store failed` 오류 뒤에 콜론과 원인이 붙은 채로 부팅이 중단됩니다. 해당 하위 시스템은 아무것도 가져오지 않은 상태입니다. 다만 원인이 커밋 실패라면 가져오기가 이미 반영되었을 수 있고, 그러면 다음 부팅은 고친 스냅샷을 다시 읽지 않고 그 하위 시스템을 `AlreadyImported`로 보고합니다. 그 이전에 가져온 하위 시스템의 행은 그대로 남습니다. 원인에 따라 조치하세요. 스냅샷 형식이 잘못되었거나 지원하지 않는 버전이면 표시된 파일을 고치거나 제거하고, 파일을 읽을 수 없으면 권한이나 경로를 고치고, 저장소 트랜잭션이 실패했으면 저장소 자체를 확인하세요. 그런 다음 다시 시작합니다. 그 밖의 저장소 시작 오류는 [문제 해결](/ko/reference/troubleshooting/)에 있습니다.

## 6. 가져오기 확인

- 가져온 각 스냅샷 옆에는 보통 백엔드, 시각, 스냅샷의 SHA-256을 기록한 `<state_path>.imported` 파일이 생깁니다. shunt는 가져오기를 커밋한 뒤 이 파일을 쓰며, 쓰지 못하면(예: 스냅샷 디렉터리가 읽기 전용) `imported the snapshot but could not write its .imported sidecar` 경고만 로그로 남기고 계속 진행합니다. 파일이 없다고 가져오기가 실패한 것은 아닙니다. 판단 기준은 시작 로그의 결과와 `import_record`입니다.
- 저장소에는 저장소를 사용하는 하위 시스템마다 `import_record` 행이 하나씩 있습니다. SQLite에서는 다음과 같이 확인합니다.

  ```bash
  sqlite3 /var/lib/shunt/shunt.db \
    'SELECT subsystem, source_path, row_count FROM import_record'
  ```

  `source_path`가 `NULL`이면 해당 하위 시스템에서 스냅샷을 찾지 못했다는 뜻입니다.
- 게이트웨이 클라이언트가 다시 로그인하지 않아도 계속 동작하며, `[server.admin]` 자격 증명을 `x-shunt-admin-token`(기본 관리자 헤더)이나 `x-api-key`에 담아 보낸 `GET /v1/organizations/spend_limits`([엔드포인트](/ko/reference/endpoints/))가 이전과 같은 지출 한도를 반환합니다.

## 스냅샷을 놓친 경우

예상하지 못한 `NoSnapshot` 결과는 대개 첫 부팅에서 경로가 틀렸다는 뜻입니다. 홈 디렉터리가 달랐거나 파일이 아직 없었던 경우입니다. 나중에 파일을 놓아도 달라지는 것은 없습니다. 가져오기 여부는 테이블이 비어 있는지가 아니라 `import_record` 행으로 결정되기 때문입니다.

SQLite에서 첫 부팅 이후 게이트웨이가 토큰을 발급, 갱신, 폐기한 적이 없고 바뀐 지출 한도도 없다면, shunt를 중지하고 데이터베이스 파일과 `-wal`, `-shm` 파일을 함께 삭제한 뒤 경로를 고쳐서 다시 시작하세요. 스냅샷은 그대로이므로 모든 하위 시스템을 처음부터 다시 가져옵니다. 그 사이 저장된 풀 쿼터도 사라지지만, 업스트림 응답으로 다시 계산됩니다. 토큰이나 한도가 바뀐 뒤에는 초기화하지 마세요. 오래된 스냅샷을 다시 가져오면 그 사이 폐기된 refresh token이 되살아나고, 그 대신 발급된 토큰은 사라집니다.

## 되돌리기

가져오기는 동기화가 아니라 복사이며, shunt는 저장소 상태를 JSON으로 다시 내보내지 않습니다. `[store]`를 제거하면 각 하위 시스템은 가져오기 시점의 스냅샷에서 시작하므로, 그 사이에 폐기된 세션이 되살아나거나 변경한 지출 한도가 되돌려질 수 있습니다. 스냅샷이 가져온 뒤로 바뀌지 않았다면 저장소 없이 부팅할 때 이를 알리는 경고를 로그로 남깁니다. 자세한 내용은 [저장소 없이 되돌리기](https://github.com/pleaseai/shunt/blob/main/docs/store.md#going-back-to-no-store)(영문)를 참고하세요.
