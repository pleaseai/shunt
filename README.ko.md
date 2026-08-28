# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

[English](README.md) · **한국어** · [日本語](README.ja.md) · [简体中文](README.zh-CN.md)

> Claude Code를 어떤 모델로든 우회(shunt)하세요.

`shunt`는 스펙을 준수하는 [Claude Code LLM 게이트웨이](https://code.claude.com/docs/en/llm-gateway-protocol)입니다. **매핑한 모델**에 한해 추론을 **추론 계층**에서 다른 LLM 프로바이더로 우회시키는 투명 프록시입니다. 요청의 `model` id를 기준으로 라우팅하며, 그 외 모든 것은 변경 없이 Anthropic으로 그대로 전달됩니다(이것이 "shunt"이며, 폴백은 `server.default_provider`로 구성할 수 있습니다).

이름 자체가 동작 방식을 나타냅니다. 전기/철도의 *shunt*는 흐름의 일부를 선택해 병렬 경로로 우회시킵니다. 여기서는 매핑된 모델의 추론이 다른 프로바이더로 우회되는 동안 Claude Code의 도구와 스킬은 그대로 유지됩니다.

**OpenAI**, **ChatGPT/Codex**(`codex login`으로 구독을 재사용), **xAI**(API 키), **Grok**(`shunt login xai`로 SuperGrok / X Premium+ 구독을 재사용), **Cursor**(`shunt login cursor`로 구독을 재사용), **Kimi Code**(`shunt login kimi`로 구독을 재사용), **Gemini / Google Code Assist**(`~/.gemini/oauth_creds.json`으로 구독을 재사용. shunt는 유효한 액세스 토큰을 그대로 사용하며, 자체 갱신에는 `SHUNT_GOOGLE_CLIENT_ID`와 `SHUNT_GOOGLE_CLIENT_SECRET`가 필요합니다), **Anthropic** 패스스루가 기본 내장되어 있으며, Anthropic Messages 호환 백엔드(Kimi, DeepSeek, GLM, MiniMax, OpenRouter, Vercel AI Gateway 등)라면 무엇이든 TOML 테이블 하나 또는 YAML 매핑 하나만 추가하면 됩니다. 코드 변경은 필요 없습니다.

> [!NOTE]
> `shunt`는 활발히 개발 중인 1.0 미만(pre-1.0) 소프트웨어입니다. [SemVer](https://semver.org/lang/ko/#spec) 관례에 따라 `0.x` 릴리스에는 설정 키, CLI, 동작에 대한 호환성이 깨지는 변경(breaking change)이 포함될 수 있으니, 업그레이드 전에 [릴리스 노트](https://github.com/pleaseai/shunt/releases)를 확인하세요.

## 설치

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo — 소스 저장소에서 직접 설치
cargo install --git https://github.com/pleaseai/shunt
```

새 버전은 Homebrew와 각 [GitHub 릴리스](https://github.com/pleaseai/shunt/releases)에 첨부된 사전 빌드 바이너리(macOS/Linux, arm64/x64)로 배포됩니다. crates.io 패키지는 마지막으로 게시된 버전에서 중단됩니다. 사전 빌드 바이너리 및 소스 빌드 안내는 [설치](https://shunt.dev/getting-started/installation/)를 참고하세요.

### 서비스로 실행하기 (macOS/Homebrew)

```bash
brew services start shunt
```

로그는 `$(brew --prefix)/var/log/shunt.log`에 남습니다. `brew services stop`은 `SIGTERM`을 보내고
shunt는 처리 중인 요청을 모두 마친 뒤 종료합니다. Unix에서는 종료가 시작될 때 Antigravity 에이전트 턴이
함께 종료되므로, 격리된 프로세스 그룹이 드레인을 붙잡아 둘 수 없습니다. 이후 설정 파일을 수정해도
재시작이 필요 없습니다 — 자동으로 [핫 리로드](docs/config-reload.md)됩니다. 자세한 내용은
[서비스로 실행하기](docs/running.md#run-as-a-background-service-homebrew)를 참고하세요.

## 빠른 시작

```toml
# shunt.toml — gpt-* id를 ChatGPT 구독으로 라우팅
# [[routes]]는 정확한 id를 위한 레거시 방식입니다. [models.upstream_model]을 권장합니다.
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"        # `codex login`을 재사용; OPENAI_API_KEY를 쓰려면 `openai` 사용
```

```bash
codex login                                        # 프로바이더 자격 증명
shunt run                                           # -> 127.0.0.1:3001 에서 리슨

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
claude                                              # /model -> gpt-5.6-sol 선택
```

매핑되지 않은 모델(모든 `claude-*` id)은 이전과 완전히 동일하게 동작합니다. shunt가 사용자 본인의 자격 증명으로 Anthropic에 전달합니다. 전체 안내: [빠른 시작](https://shunt.dev/getting-started/quickstart/).

### 시작 구성

`shunt init`은 기존 디렉터리에 주석이 포함된 `shunt.toml`을 생성합니다. 기본 passthrough starter를 그대로 사용하거나, 매핑되지 않은 모델의 fallback을 바꾸지 않고 순서가 있는 upstream preset을 scaffold할 수 있습니다.

```bash
shunt init
shunt init --upstream codex --upstream kimi
```

### 에이전트 중심 설정 blueprint

`shunt add`는 코딩 에이전트용 내장 Markdown 구현 가이드를 가져옵니다. `shunt add upstream`으로 사용 가능한 upstream blueprint를 확인하거나 에이전트에 바로 파이프하세요.

```bash
shunt add upstream kimi --print | claude
shunt add upstream https://provider.example/docs --print | claude
```

이 명령은 오프라인이고 읽기 전용입니다. 안내만 출력하며 파일을 수정하거나 설치하거나 네트워크에 접근하지 않습니다. 완전히 새로운 provider protocol 지원에 기여하려면 `shunt add provider <absolute-url>`을 사용하세요.

공유 배포에서는 `[server] max_concurrent_requests`(기본값 `1024`)로 처리 중인 인바운드 요청 수를
제한합니다. 초과 요청은 `503`과 `Retry-After: 1`로 즉시 흘려보내며, 스트리밍 요청은 응답 본문이
끝나거나 클라이언트가 연결을 끊을 때까지 자리를 유지합니다. 값을 `0`으로 설정하면 제한이 해제됩니다.
`/`와 `/health`는 liveness probe를 위해 항상 사용할 수 있습니다. 공유 게이트웨이는 `[server.access_control]`,
`[server.limits]`, `[server.timeouts]`, `[server.rate_limits]` 아래에서 CIDR 허용/거부 규칙, 요청·헤더·URL
크기 제한, 스트리밍 본문에는 상한을 두지 않는 업스트림 응답 헤더 타임아웃, 그리고 독립적인 device-flow
레이트 리밋을 구성할 수도 있습니다. Anthropic Messages와 인바운드 Codex Responses 요청 본문은 기본적으로
32 MiB로 제한되며, 더 큰 파일이나 이미지 요청에는 `max_request_bytes`를 올리세요. 그 외 게이트웨이, 어드민,
텔레메트리, 애널리틱스 라우트는 각 엔드포인트별 본문 제한을 그대로 유지합니다.
[구성 레퍼런스](https://shunt.dev/ko/reference/configuration/#server)를 참고하세요.

시크릿 성격의 값은 구성 파일에 리터럴로 둘 필요가 없습니다. 어떤 문자열이든 `${VAR}`(환경 변수, 예:
`"Bearer ${TOKEN}"`) 또는 `${file:/abs/path}`(파일의 공백을 제거한 내용 전체가 해당 필드의 값)로 작성할 수
있으며, 구성을 읽을 때마다 새로 확인됩니다 — [핫 리로드](docs/config-reload.md) 시에도 마찬가지이므로
`${file:}` 기반 시크릿은 shunt를 재시작하지 않고 회전할 수 있습니다. 다만 새 값이 실제로 적용되는지는 각
필드 자체의 리로드 동작을 따릅니다. `[sentry]`와 `[otel]`은 기동 시 한 번만 구성되므로, 이 두 섹션의
시크릿을 회전하려면 여전히 재시작이 필요합니다. [구성 시크릿 참조](docs/config-secrets.md)를 참고하세요.

## 프로바이더

프로바이더는 순서가 있는 `[[upstreams]]` 항목 또는 레거시 `[providers.<name>]` TOML 테이블입니다(YAML에서는 각각 해당 sequence 또는 mapping의 항목). 두 가지 어댑터 종류가 대부분의 업스트림을 커버합니다. `kind = "anthropic"`(업스트림이 Anthropic Messages를 사용하며, 필요하면 다른 키로 패스스루)와 `kind = "responses"`(업스트림이 OpenAI Responses API를 사용하며, shunt가 Anthropic Messages ⇄ Responses를 스트리밍 포함하여 변환)입니다. 세 번째 네이티브 종류인 `kind = "cursor"`는 Cursor의 ConnectRPC/protobuf AgentService를 브리지하여 Cursor 구독을 동일한 Anthropic-Messages 인터페이스로 사용할 수 있게 합니다.

순서가 있는 업스트림은 프로바이더 간 페일오버를 지원합니다. 선언 순서가 시도 순서이며, 모델의 `upstream_model` 맵은 참여할 항목을 선택하고 공개 id를 각 백엔드 id에 매핑합니다.

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic" # preset: kind, base_url, and default auth
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "codex-fallback"
provider = "codex" # defaults to chatgpt_oauth

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
codex-fallback = "gpt-5.6-sol"
```

이 체인은 `anthropic-primary`를 먼저 시도한 다음 `codex-fallback`을 시도합니다. `auth`는 mode 문자열 또는 맵을 받으며, `claude_oauth`와 `chatgpt_oauth` 맵은 `account = "name"` 또는 `accounts = [...]`로 자격 증명 범위를 좁힐 수 있습니다. 레거시 `[providers.<name>]`는 계속 지원되며 이름순의 암시적 업스트림이 됩니다. 구성 파일에서 두 형식을 함께 선언하지 마세요. `[[upstreams]]`와 `[providers.*]`를 혼합하면 구성 오류입니다. preset, 실패 클래스, 마이그레이션 세부 사항은 [구성 레퍼런스](https://shunt.dev/reference/configuration/)를 참고하세요.

**기본 내장:**

| 이름 | 종류 | 인증 | 백엔드 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough 또는 Claude OAuth 계정 풀 | `api.anthropic.com` — 기본적으로 호출자 본인의 자격 증명을 전달하며, `auth = "claude_oauth"`를 쓰면 풀링된 구독 자격 증명을 사용합니다 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — `~/.codex/auth.json`(`codex login`)을 재사용 |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` — 개발자 API, 토큰당 과금 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` — Grok CLI 프록시, `~/.shunt/xai-auth.json`을 재사용(SuperGrok / X Premium+ 구독으로 `shunt login xai`) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` — `~/.shunt/cursor-auth.json`(`shunt login cursor`)을 재사용 |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` — Google Code Assist 백엔드, `~/.gemini/oauth_creds.json`을 재사용 |
| `antigravity` | `antigravity` | Antigravity OAuth | `cloudcode-pa.googleapis.com` — HTTP로 통신하는 Google Antigravity 백엔드, `~/.shunt/antigravity-auth.json`(`shunt login antigravity`)을 사용 |
| `antigravity-cli` | `antigravity_cli` | 없음(로컬 CLI) | **Deprecated.** 로컬 `agy` 바이너리 — 서브프로세스로 동일한 백엔드를 사용하며, 위의 `antigravity`로 대체되었습니다 |

xAI는 구독 등급에 따라 OAuth 접근을 제한할 수 있습니다. `grok`이 403을 반환하면 `xai` API 키 프로바이더를 대신 사용하세요. 자세한 내용은 [`docs/m6-xai-provider.md`](docs/m6-xai-provider.md)를 참고하세요.

**Antigravity에는 두 가지 전송 방식이 있습니다.** `antigravity` 프로바이더는 HTTP로 Google Antigravity 백엔드와 통신하며 `shunt login antigravity`로 인증합니다. 이는 Antigravity 자체의 OAuth 클라이언트와 스코프를 사용하는 Google authorization-code 플로이므로, Gemini CLI 로그인을 재사용할 수 없습니다. `gemini` 프로바이더와 동일한 Code Assist 프로토콜을 사용하며 현재는 Gemini 계열 Antigravity 모델을 제공합니다. Antigravity가 함께 제공하는 Claude 모델은 아직 구현되지 않은 요청 재작성이 필요합니다(#368). 로그인과 스코프, 프로젝트 탐색, 모델 슬러그, thinking, 어댑터가 전달하는 내용 등 전체 설정은 [프로바이더 → Antigravity](https://shunt.dev/ko/providers/antigravity/)를 참고하세요.

**`antigravity-cli`는 deprecated이며 임의 코드 실행에 해당합니다.** 이 프로바이더는 로컬 `agy` 바이너리를 에이전트 모드로 실행합니다. CLI가 자체 도구로 작업을 수행하고 shunt는 그 진행 상황을 Anthropic SSE로 스트리밍해 돌려줍니다. 따라서 요청의 `tools` 배열은 전달되지 않으며 `tool_use` 블록도 반환되지 않습니다. 비대화형 실행에서는 권한 프롬프트에 응답할 수 없으므로 `agy`는 `--dangerously-skip-permissions`로 실행됩니다. 그러므로 **이 프로바이더는 shunt를 실행하는 사용자 권한의 임의 코드 실행으로 취급하세요**. 두 가지 설정이 이를 제한합니다. `sandbox`(기본값 `true`)는 `--sandbox`를 전달해 읽기와 쓰기를 워크스페이스 안으로 묶으며, 실제로 에이전트를 가두는 것은 이 설정입니다. `workspace_roots`는 에이전트가 *시작*할 수 있는 위치만 결정하며, 요청의 시스템 프롬프트(클라이언트가 제어하는 텍스트)에서 가져온 `Working directory:` 경로를 나열한 루트 아래로 정규화된 경로로 제한합니다. 샌드박스는 켜 두고 바인드는 루프백으로 유지하세요. 이런 설정이 전혀 필요 없는 위의 `antigravity` 프로바이더를 사용하는 편이 좋습니다. [프로바이더 가이드](https://shunt.dev/ko/guides/providers/)를 참고하세요.

**기존 `antigravity`에서 마이그레이션하기.** 예전에는 `kind = "antigravity"`가 로컬 CLI를 의미했습니다. 그 의미로 작성된 구성은 조용히 다른 대상으로 바뀌지 않고 이름을 근거로 거부되며, 자격 증명이 없는 상태로 라우팅된 `antigravity` 프로바이더는 기동을 거부합니다. 기동은 정상인데 그 아래에서 전송 방식과 자격 증명, 이그레스가 바뀌는 편이 실패하는 것보다 나쁘기 때문입니다. `shunt check`도 같은 가드를 실행하므로, CI와 배포 스크립트는 자격 증명이 저장되지 않은 `antigravity` 라우트를 기동 시점이 아니라 롤아웃 전에 잡아냅니다. 이 검사는 존재 여부만 확인합니다. 자격 증명을 열어 보지는 않으므로 비어 있거나 오래된 자격 증명도 통과한 뒤 요청 경로에서 실패합니다. `shunt login antigravity`를 실행하거나 라우트를 `antigravity-cli`로 지정하세요.

**Anthropic 멀티 계정.** `auth = "claude_oauth"`인 Anthropic 프로바이더는 Claude Code 자격 증명 파일이나 setup-token 환경 변수에서 명시적 계정을 불러오거나, `shunt login claude --name <name>`으로 만든 비공개 스토어 관리 계정을 사용할 수 있습니다. Claude 로그인에는 세 가지 모드가 있습니다. `--mode oauth`는 shunt 자체의 갱신 가능한 OAuth 플로를 실행하고(터미널 기본값), `--mode import`는 현재 Claude Code 로그인을 복사하며, `--mode setup-token`은 1년 동안 사용하는 추론 전용 토큰을 생성합니다(`--long-lived`는 계속 deprecated alias로 남아 있습니다). OAuth는 먼저 자동 `127.0.0.1` 콜백을 사용하고 숨겨진 수동 붙여넣기로 폴백합니다. 콜백을 건너뛰려면 `--manual`을 사용하세요. OAuth 범위 동작은 선언 형식에 따라 다릅니다. 레거시 `[providers.*].accounts = []`는 계정 스토어를 검색하지만, 순서가 있는 `[[upstreams]]`는 전체 스토어를 검색하려면 `account`와 `accounts`를 모두 생략해야 하며 명시적인 `accounts = []`는 거부됩니다. shunt는 정상인 `x-claude-code-session-id` 세션을 고정하고, 그 외에는 프로바이더별 라운드 로빈을 사용하며, 가능한 경우 모델을 반영한 5시간/주간 쿼터 상태에 따라 쿼터에 가까워진 고정 계정을 한도에 도달하기 전에 선제적으로 교체합니다. 선택 동작은 선택 사항인 `[server.pool]` 테이블로 조정할 수 있습니다(issue #135). 여기에는 계정별 재정의를 지원하는 윈도우별 소프트 쿼터 임계값(낮은 임계값은 백업 계정으로 표시), 번 레이트를 고려한 순서 지정, 리셋 전에 윈도우가 소진될 것으로 예측되는 계정을 선택적으로 피하는 기능, 계정별 `priority`와 `disabled` 설정이 포함됩니다. `usage_refresh_seconds`를 활성화하면 가져온(갱신 가능한) 계정의 Anthropic OAuth 사용량 API를 폴링해 외부에서 발생한 사용량을 반영할 수 있으며, 폴링은 기본적으로 꺼져 있습니다. `state_path`를 설정하면 계정별 쿼터를 디스크에 저장하므로 재시작 후 빈 풀 대신 마지막으로 관측한 사용량에서 시작합니다(최선 노력 캐시이며 쿼터는 이와 관계없이 업스트림에서 다시 계산됩니다). 리셋 시각이 없는 윈도우도 자체 마지막 관측 시각으로 수명을 제한하므로 영구히 남을 수 없으며, 영속화는 기본적으로 꺼져 있습니다. 쿼터로 거부된 429, 401, 5xx 응답의 반응형 처리는 계속해서 페일오버의 최저 안전선으로 작동합니다. 스톰 제어는 이후 후속 작업입니다. [사용 방법](https://shunt.dev/guides/anthropic-multi-account/), [구성 레퍼런스](https://shunt.dev/reference/configuration/), [M8 동작 사양](docs/m8-anthropic-multi-account.md)을 참고하세요.

**Codex 멀티 계정.** `chatgpt_oauth` 프로바이더(기본 제공 `codex` 프로바이더 또는 해당 인증 모드를 사용하는 모든 `responses` 프로바이더)도 여러 ChatGPT 계정을 풀링할 수 있습니다. 계정은 `codex login` 자격 증명을 `shunt login codex --name <name>`으로 가져오거나, [관리자 웹 화면](https://shunt.dev/guides/admin-remote-provisioning/)에서 ChatGPT OAuth를 실행하거나, 명시적인 `credentials`/`token_env` 계정 항목으로 구성할 수 있습니다. OAuth 범위 동작은 선언 형식에 따라 다릅니다. 레거시 `[providers.*].accounts = []`는 계정 스토어를 검색하지만, 순서가 있는 `[[upstreams]]`는 전체 스토어를 검색하려면 `account`와 `accounts`를 모두 생략해야 하며 명시적인 `accounts = []`는 거부됩니다. shunt는 백엔드의 `x-codex-*` 5시간/7일 사용량 윈도우를 기록하고 이를 Claude 풀과 같은 **쿼터 인식 선제 선택**에 반영합니다. 쿼터에 가까워진 고정 계정은 429를 반환하기 전에 양보하며, 업스트림 리셋 헤더가 비어 있어도 관측 시각만으로 표시의 수명을 제한하므로 `[server.pool]` 임계값과 번 레이트 순서 지정이 적용됩니다. 쿨다운 기반 반응형 페일오버(429, 401, 5xx, 자격 증명 확인 실패)는 안전선으로 유지됩니다. 선택 사항인 `[server.pool] ramp_initial_concurrency` 슬로 스타트 게이트를 추가로 활성화하면 페일오버 뒤 새로 선택된 계정에 동시 진행 중인 요청이 한꺼번에 몰리는 일을 막을 수 있습니다. [사용 방법](https://shunt.dev/guides/codex-multi-account/)과 [M10 동작 사양](docs/m10-codex-multi-account.md)을 참고하세요.

**인바운드 Codex 엔드포인트.** shunt는 반대 방향으로도 동작할 수 있습니다. 선택 기능인 `[server.codex_endpoint]` 테이블을 구성하면 raw OpenAI Responses 패스스루(`/responses`, `/v1/responses`, `/backend-api/codex/responses`)가 등록되므로, **Codex CLI 자체**가 `base_url`을 shunt로 지정해 같은 ChatGPT/Codex OAuth 계정 풀에서 부하 분산을 받을 수 있습니다. 이는 위의 Anthropic 변환 경로가 아니라 바이트 단위 그대로의 릴레이입니다. 또한 CLI의 두 애널리틱스 경로를 프라이버시를 지키는 폐기 싱크로 받아들여, 정제된 이벤트 이름 카운터만 기록하고 페이로드는 절대 업스트림으로 전달하지 않습니다. 기본적으로 꺼져 있으며, 테이블이 없으면 해당 라우트는 하나도 등록되지 않습니다. [사용 방법](https://shunt.dev/ko/guides/inbound-codex-endpoint/)과 [M11 동작 사양](docs/m11-inbound-codex-endpoint.md)을 참고하세요.

**제한된 업스트림 재시도.** 프로바이더의 단일 자격 증명 경로에서 발생한 일시적 업스트림 실패는, 클라이언트에 바이트가 도달하기 전에(스트림 도중에는 절대 아님) 지수 백오프와 무작위 지터로 재시도됩니다. 연결 수준의 전송 오류(connect reset/refused, 타임아웃)는 항상 재시도합니다. 이 오류가 확정되기 전에는 아무것도 수락되지 않았기 때문입니다. 일시적 응답 *상태*(`429`/`502`/`503`/`504`/`529`, Anthropic의 "Overloaded")는 멱등한 Cursor 경로에서만 재시도합니다. 멱등하지 않은 Anthropic Messages와 단일 자격 증명 Responses POST는 이를 즉시 노출하는데, 응답이 왔다는 것은 업스트림이 이미 과금 대상 생성을 수락했을 수 있다는 뜻이기 때문입니다(issue #126). 그 외 `4xx`는 절대 재시도하지 않습니다. 재시도는 `Retry-After`(delta-seconds와 HTTP-date 형식 모두)를 존중하고, `count_tokens`에서는 비활성이며, `[providers.<name>.retry]` 아래에서 프로바이더별로 구성할 수 있습니다(기본적으로 켜져 있고 보수적입니다. `max_retries = 0`으로 끌 수 있습니다). `claude_oauth`/`chatgpt_oauth` 계정 풀은 대신 자체 계정 순환 페일오버를 사용합니다. [구성 레퍼런스](https://shunt.dev/ko/reference/configuration/#providersnameretry)를 참고하세요.

**선택 기능인 Claude 앱 게이트웨이 로그인 및 정책.** `[server.gateway]`를 구성하면, 공유 정적 토큰 하나를 배포하는 대신 관리 대상 Claude Code 클라이언트가 OAuth device flow(`forceLoginMethod: "gateway"` + `forceLoginGatewayUrl`)로 로그인하게 할 수 있습니다. 브라우저 승인에는 환경 변수 기반 정적 사용자나 `[server.gateway.oidc]`를 통해 허용 목록에 올린 OIDC 프로바이더(예: Google)를 쓸 수 있으며, 두 방식을 함께 제공할 수도 있습니다. shunt는 OAuth discovery, 브라우저 승인, device/refresh 그랜트, HS256 액세스 JWT, 회전하는 불투명 refresh token, 그리고 `ETag` 캐싱과 텔레메트리 환경 변수 푸시, `availableModels` 강제를 갖춘 사용자별 `GET /managed/settings`를 제공합니다. 발급된 bearer는 `/v1/models`와, 선택된 프로바이더가 서버 측 자격 증명을 주입하는 추론 경로를 보호하며 passthrough 프로바이더는 그대로 열려 있습니다. `[server.auth]`와 조합할 수 있습니다. 이 기능은 기본적으로 꺼져 있습니다. refresh 세션은 기본적으로 재시작 후에도 유지됩니다(이슈 #194). `state_path`(기본값 `~/.shunt/gateway-sessions.json`)는 refresh token을 SHA-256 해시로, 원자적으로 기록되는 소유자 전용(Unix에서 0600) 파일에 저장하고 부팅 시 복원하므로, 사용자는 브라우저 플로우를 다시 거치지 않고 조용히 갱신을 이어갑니다. 메모리 전용 세션을 쓰려면 `state_path = ""`로 설정하세요. 이 경우 재시작하면 refresh 세션이 무효화되며, 이미 발급된 액세스 JWT는 만료될 때까지 유효하고 그 뒤에는 다시 로그인해야 합니다. device 그랜트는 항상 메모리 전용입니다. 클라이언트는 Claude Code 안에서가 아니라 터미널에서 로그인할 수도 있습니다. `shunt gateway login <url>`은 같은 device flow를 실행해 세션을 로컬(`~/.shunt/gateway/session.json`, 소유자 전용)에 저장하고, `shunt gateway token`은 `apiKeyHelper`로 쓸 액세스 토큰을 출력하며, `shunt gateway claude`는 그 연결 설정을 단일 프로세스에만 적용한 채 Claude Code를 실행합니다 — `~/.claude/settings.json`을 수정하지 않고, 클라이언트를 로그인된 게이트웨이 세션 상태로 만들지도 않으므로 그 게이트에 따르는 기능 트레이드오프를 떠안지 않습니다(모든 `apiKeyHelper`가 걸리는 일반적인 자격 증명 종류 게이트는 그대로 적용됩니다). `shunt login <provider>`와 `shunt token`은 변경되지 않았으며 여전히 shunt를 업스트림에 인증합니다. [설정 가이드](https://shunt.dev/guides/gateway-login/), [구성 레퍼런스](https://shunt.dev/reference/configuration/#servergateway-optional), [M-A 로그인 노트](docs/gateway-login.md), [M-B managed-settings 노트](docs/gateway-managed-settings.md), [M-C 텔레메트리 노트](docs/gateway-telemetry.md)를 참고하세요.

**선택 기능인 지출 한도 Admin API.** `[server.spend]`를 구성하면 조직 단위 및 사용자 단위 한도를 위한 인증된 CRUD 라우트가 `/v1/organizations/spend_limits` 아래에 등록됩니다. 1단계에서는 한도와 감사 기록을 저장하지만 아직 추론 트래픽에 한도를 강제하지는 않습니다. 이 라우트는 `[server.admin]` 자격 증명으로 인증합니다. `[server.spend]`는 키 자료를 담지 않는 최상위 정책 섹션이므로, 지출 한도를 활성화하는 데 `[server.gateway]` 로그인이 필요하지 않습니다. 상태는 기본적으로 원자적으로 기록되는 비공개 JSON 파일에 저장됩니다. 설정, API 동작, 보류된 기능은 [1단계 가이드](docs/gateway-spend-limits.md)를 참고하세요.

**선택 기능인 게이트웨이 텔레메트리 인제스트.** 비어 있지 않은 `[server.gateway.telemetry].forward_to` 목록은 두 가지 일을 합니다. 텔레메트리 활성화 플래그와 다섯 개의 `OTEL_*` 환경 값을 managed settings로 푸시해(관리 대상 클라이언트의 익스포터를 모두 shunt로 향하게 하고), 그 클라이언트들이 전송할 인바운드 OTLP/HTTP 라우트의 그대로 릴레이를 켭니다 — `POST /v1/metrics`, `POST /v1/logs`, `POST /v1/traces`이며, `[server.gateway]` 표면의 나머지와 함께 등록되고 `GET /managed/settings`와 동일한 게이트웨이 bearer로 보호됩니다. 페이로드는 **그대로** 릴레이됩니다 — 요청 바이트 그대로, 인바운드 `content-type`과 `content-encoding`을 유지하며, 클라이언트의 `Authorization` 헤더는 절대 전달하지 않습니다. 그래서 `application/x-protobuf`와 `application/json` 익스포터가 모두 동작하고 Claude Code의 클라이언트 측 어트리뷰션 속성도 보존됩니다. 각 목적지는 시그널별로 옵트인합니다. `metrics`는 기본으로 켜져 있고, `logs`와 `traces`는 Claude Code의 로그 레코드와 스팬이 명령줄, 프롬프트, 파일 경로를 담을 수 있어 꺼져 있습니다. 옵트인한 목적지가 없는 시그널은 수락 후 폐기되며, 릴레이는 분리되어 실행되므로 컬렉터가 느리거나 다운되어도 클라이언트는 항상 즉시 `200`을 받습니다. 기본적으로 꺼져 있으며, 목적지가 없으면 라우트는 수락 후 폐기합니다. [Claude Code 모니터링](https://code.claude.com/docs/en/monitoring-usage), [구성 레퍼런스](https://shunt.dev/ko/reference/configuration/#servergatewaytelemetry-optional), [M-C 텔레메트리 노트](docs/gateway-telemetry.md)를 참고하세요.

**선택 기능인 관리자 웹 화면.** `[server.admin]`을 구성하면 관리자 인증이 필요한 **계정 및 사용량** 화면이 추가되어, 지원되는 호스트 로그인을 자동으로 관측하고 알아볼 수 있게 마스킹된 신원과 프로바이더 고유의 쿼터 윈도우를 보여줍니다. 대상은 Claude Code(자격 증명 파일 또는 macOS 키체인), Codex CLI(응답에서 도출한 `x-codex-*` 윈도우), Gemini CLI(모든 Code Assist 모델 버킷), Kimi Code(주간 및 5시간 한도), Grok CLI(크레딧/제품 사용량), Cursor.app(빌링 사이클, Auto + Composer, 이름이 지정된 모델 사용량)입니다. 관측은 읽기 전용입니다. shunt는 그 원본 자격 증명을 절대 갱신하거나 복사하거나 기록하지 않으며, Cursor.app 상태는 읽기 전용으로 열어 메모리 상의 퍼스트파티 웹 세션을 도출하는 데만 사용합니다. Claude 사용량은 60초 동안 캐시되고, 나머지 프로바이더 리더는 대시보드 데이터가 요청될 때 실행됩니다. 관리 대상 풀 프로비저닝은 Claude와 Codex 계정에 대해 접힌 고급 섹션에서 계속 제공됩니다. 별도로 저장되는 이 계정들이 shunt가 로드 밸런싱을 위해 소유하고 갱신하는 자격 증명입니다. 관리 대상 풀의 `/admin/pool` 화면은 계정별 선택 항목인 `plan`(구독 등급)도 함께 표시합니다. 가능한 경우 파일에서 도출하며, Claude 계정에 대해서는 제한적이고 캐시된 라이브 백필로 보강합니다 — 이는 누락된 plan을 채울 수도 있고, 배수 정보가 없는 파일 기반 값을 더 정확한 값으로 다듬을 수도 있습니다. plan을 확정할 수 없으면 그 키는 그냥 없습니다. 이 화면은 기본적으로 꺼져 있으며(테이블이 없으면 `/admin*` 라우트가 하나도 등록되지 않습니다), `[server.auth]`와는 별개의 자격 증명을 사용합니다. 한 번에 활성화하려면 `shunt dashboard setup`을 실행하세요. 이 명령은 `~/.shunt/admin-token`(소유자 전용)에 관리자 토큰을 생성하고, `[server.admin].tokens_file`로 연결해 실행 환경 변수에 시크릿이 남지 않게 하며, `[server.oauth_usage]`를 활성화하고 대시보드 URL을 출력합니다 — 그 뒤 shunt를 재시작하세요. 관리자 토큰은 `SHUNT_ADMIN_TOKENS`에서 가져올 수도 있습니다. 두 가지 접근 등급이 있습니다. `[[server.admin.write_keys]]` 항목과 `tokens_env`/`tokens_file`의 `name:token` 쌍은 전체 접근 권한을 가지며, `[[server.admin.read_keys]]` 항목은 관리자 화면과 지출 한도 API를 통틀어 모든 `GET`을 통과하고 모든 변경 요청에서는 거부되며 브라우저 로그인 폼도 마찬가지입니다. 배열 형태의 키는 `${VAR}` / `${file:...}` 또는 `SHUNT_*` 오버라이드로 제공해야 하며, 구성 파일에 리터럴로 적으면 로드 시점에 거부됩니다. [사용 방법](https://shunt.dev/ko/guides/admin-remote-provisioning/)과 [M9 설계 노트](docs/m9-admin-surface.md)를 참고하세요.

**선택 기능인 클라이언트 사용량 엔드포인트.** `[server.usage]`를 구성하면 읽기 전용 `GET /usage`가 등록되어 공유 계정 풀 쿼터의 **정제·집계된** 뷰를 반환합니다 — 윈도우별 남은 여유분, 리셋 시각, 그리고 대략적인 `ok`/`degraded`/`exhausted` 상태이며, 관리자 화면 없이도 비관리자 클라이언트가 스로틀링을 예측할 수 있습니다. 인증은 `/v1/messages`와 동일한 `[server.auth]` 클라이언트 토큰을 사용하며(해당 테이블이 필요합니다), 계정 이름, 개수, 우선순위, `disabled` 플래그, 임계값은 절대 노출하지 않습니다. 계정별 전체 세부 정보는 관리자 전용 `/admin/pool`에만 남습니다. Codex 윈도우는 `null`을 보고합니다(쿼터 헤더가 없습니다). 기본적으로 꺼져 있으며, 테이블이 없으면 라우트가 등록되지 않습니다. [구성 레퍼런스](https://shunt.dev/ko/reference/configuration/#serverusage-optional)와 [M12 설계 노트](docs/m12-client-usage-endpoint.md)를 참고하세요.

**선택 기능인 Claude Code CLI 네이티브 사용량 막대.** `[server.oauth_usage]`를 구성하면 `GET /api/oauth/usage`가 등록됩니다. 이는 Claude Code CLI 자체의 `Current session`/`Current week` 사용량 막대가 가져오는 바로 그 경로이므로, CLI를 `ANTHROPIC_BASE_URL`로 shunt에 연결했을 때 수정되지 않은 UI가 404로 빈 막대를 띄우는 대신 Claude만을 대상으로 한, 우선순위 등급을 반영한 최악 기준의 실제 풀 수치를 렌더링할 수 있습니다. **전제 조건, 일부만 검증됨:** `claude setup-token`이나 공유 게이트웨이 클라이언트 토큰으로 `ANTHROPIC_AUTH_TOKEN`을 설정한 경우 — 문서화된 나머지 두 가지 shunt 자격 증명 구성입니다 — 에는 이 경로를 가져오지 **않는다는 점**이 확인되었습니다. 완전한 대화형 `claude login`(구독) 세션이 이를 실제로 가져온다는 것은 CLI 바이너리 정적 분석과 정황상의 UI 근거에서 추정한 것이며 직접 관측하지는 못했습니다(정찰 환경에서 실제 구독 로그인을 안전하게 스크립트로 실행할 수 없었습니다). 따라서 이는 모든 구성에서 "설치 즉시 동작"하는 기능이 아닙니다. 전제 조건에 대한 전체 근거와 검증되지 않은 한 갈래는 [M14 설계 노트](docs/m14-oauth-usage-endpoint.md)를 참고하세요. 인증은 바인드 토폴로지에 따라 결정됩니다(루프백에서는 인증 없음. 루프백이 아닌 바인드에서는 유효한 클라이언트 토큰 또는 게이트웨이 JWT가 필요하며 — 헤더가 있는지만 보는 것이 아니라 `/v1/messages`와 정확히 동일하게 검사합니다 — 이 경우 `[server.auth]`나 `[server.gateway]` 구성도 함께 필요합니다). 기본적으로 꺼져 있으며, 테이블이 없으면 라우트가 등록되지 않습니다. [구성 레퍼런스](https://shunt.dev/ko/reference/configuration/#serveroauth_usage-optional)를 참고하세요.

**선택 기능인 업스트림 상태 폴링.** `[server.status]`에 하나 이상의 Statuspage `summary.json` 소스를 구성하면 각 프로바이더의 공개 상태 피드를 일정 주기(기본 5분)로 폴링해, 마지막으로 관측한 지표를 관리자 대시보드의 "Upstream status" 스트립과 `shunt.upstream.status` 게이지 메트릭으로 노출합니다. 이는 엄격하게 관측 전용입니다. 여기서 읽은 내용은 라우팅, 페일오버, 풀/쿨다운 결정에 절대 반영되지 않습니다. 가져오기에 실패하거나 2xx가 아닌 응답을 반환하거나 shunt가 인식하지 못하는 지표를 보고하는 소스는 조용히 정상으로 간주되지 않고 `unknown`으로 저장·보고됩니다. 기본적으로 꺼져 있으며, 테이블이 없으면 백그라운드 폴링이 시작되지 않습니다. [구성 레퍼런스](https://shunt.dev/ko/reference/configuration/#serverstatus-optional)와 [설계 노트](docs/upstream-status.md)를 참고하세요.

OpenAI의 Thibault Sottiaux는 다른 코딩 하네스를 통해 Codex를 실행하는 것을 공개적으로 환영했습니다.

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([출처](https://x.com/thsottiaux/status/2075830097488249060))

그는 [후속 글](https://x.com/thsottiaux/status/2076119366647894371)에서 Claude Code("당신의 주황색 게")를 GPT-5.6 Sol에 직접 연결하는 과정을 설명했습니다. `shunt`가 수행하는 추론 계층 교체와 정확히 같으며, 별도의 앱이 필요 없습니다.

다만, 비공식 클라이언트에서 ChatGPT/Codex나 SuperGrok 구독(또는 Kimi, Cursor 등 다른 백엔드)을 재사용하는 것은 본인의 판단입니다. 공개적인 환영이 향후 정책이나 계정 제재가 없음을 보장하지는 않습니다. 사용에 따른 책임은 본인에게 있습니다.

**Cursor**도 같은 방식으로 동작합니다. 한 번 로그인한 뒤 `cursor:*` 모델 id를 라우팅하세요:

```bash
shunt login cursor                                  # OAuth -> ~/.shunt/cursor-auth.json
```

```toml
# shunt.toml — cursor:<id>를 Cursor 구독으로 라우팅
[[routes]]
model = "cursor:default"                             # "default"는 Auto의 wire id입니다. 유료 플랜은 이름이 지정된 id를 쓸 수 있습니다
provider = "cursor"
```

`cursor:` / `cursor-agent:` / `cursor-plan:` / `cursor-ask:` 프리픽스는 Cursor의 에이전트 모드(Agent / Plan / Ask)를 선택하며, 접미사는 Cursor의 **wire** 모델 id입니다(Auto는 `auto`가 아니라 `default`입니다). 어댑터는 어시스턴트 텍스트와 추론을 스트리밍하고, 클라이언트의 도구를 네이티브 Cursor MCP 도구 호출로 브리지하며, 인라인 이미지를 전달합니다(issue #170). 자세한 내용은 [프로바이더 → Cursor](https://shunt.dev/ko/providers/cursor/)를 참고하세요.

**Anthropic 호환 백엔드**라면 무엇이든 테이블 하나만 추가하면 됩니다. 코드 변경은 없습니다.

| 프로바이더 | `base_url` | 예시 모델 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| Kimi Code (구독, OAuth) | `https://api.kimi.com/coding` | 구독에서 제공하는 ID 사용 |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax 문서](https://platform.minimax.io/docs/token-plan/claude-code) 참고 |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

위 표의 행은 대부분 `auth = "api_key"`를 사용합니다. 예외는 **Kimi Code**로, 바로 위 행의 종량제 Moonshot API와는 별개의 구독형 Kimi 서비스입니다 — 호스트가 다르고, API 키 대신 OAuth를 사용합니다. `kind = "anthropic"`, `base_url = "https://api.kimi.com/coding"`, `auth = "kimi_oauth"`를 제공하는 전용 내장 `kimi-code` 프리셋이 있어, `provider = "kimi-code"`와 로그인한 계정만 있으면 별도의 `[providers.*]`/`[[upstreams]]` 테이블 작성이 필요 없습니다:

```bash
shunt login kimi --name <account-name>                # RFC 8628 디바이스 플로우 -> ~/.shunt/accounts/kimi/<account-name>.json
```

```toml
# shunt.toml — Kimi Code 구독으로 라우팅
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# [[upstreams]]를 선언하면 내장 provider 세트가 대체되므로 마지막에 anthropic
# passthrough를 남겨 둡니다. 없으면 `shunt check`가 기본 server.default_provider를
# 찾지 못해 실패합니다. `shunt init`이 덧붙이는 항목과 동일합니다.
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

`kimi_oauth`는 `claude_oauth`/`chatgpt_oauth`와 마찬가지로 풀링이 가능합니다 — `account` 대신 `accounts = [...]`를 사용하면 여러 개의 저장된 Kimi 계정에 부하를 분산할 수 있습니다. 어드민/`/usage` 풀 화면을 포함한 전체 안내는 [Kimi → Kimi Code (OAuth subscription)](https://shunt.dev/providers/kimi/#kimi-code-oauth-subscription)를 참고하세요. 디바이스 플로우·토큰 저장소·검증 내부 구조는 [M15 설계 노트](docs/m15-kimi-oauth.md)를 참고하세요.

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

전체 목록과 프로바이더별 참고 사항은 [프로바이더](https://shunt.dev/guides/providers/)를 참고하세요.

## 문서

모든 내용은 **[shunt.dev](https://shunt.dev)**에 있습니다.

- [빠른 시작](https://shunt.dev/getting-started/quickstart/) · [왜 shunt인가?](https://shunt.dev/getting-started/why-shunt/) · [프로바이더](https://shunt.dev/guides/providers/) · [구성](https://shunt.dev/guides/configuration/) · [문제 해결](https://shunt.dev/reference/troubleshooting/)
- **에이전트용:** 모든 페이지에는 Markdown 쌍둥이 페이지가 있으며(임의의 URL에 `.md`를 붙이거나 페이지의 *Copy Markdown* / *Open in AI* 버튼 사용), 사이트는 [llms.txt 스펙](https://llmstxt.org/)에 따라 [`/llms.txt`](https://shunt.dev/llms.txt), [`/llms-small.txt`](https://shunt.dev/llms-small.txt), [`/llms-full.txt`](https://shunt.dev/llms-full.txt)를 게시합니다.

설계 노트와 마일스톤 스펙은 [`docs/`](docs/)에 있습니다([`docs/implementation-plan.md`](docs/implementation-plan.md)부터 시작하세요). Claude Code를 ChatGPT/Codex 구독으로 라우팅하려면 [Codex 구성 레퍼런스](docs/codex-configuration.md)를 참고하세요.

### 관측성 메트릭

| 시리즈 | 종류 | 속성 | 의미 |
| :-- | :-- | :-- | :-- |
| `shunt.failover` | Counter | `provider`, `state` | 순서가 있는 업스트림의 페일오버 전이: `attempted`, `advanced`, `exhausted`. |

전체 메트릭 표와 익스포트 구성은 [OpenTelemetry 가이드](https://shunt.dev/ko/guides/opentelemetry/)를 참고하세요.

## 왜

Claude Code는 모든 턴을 Anthropic API로 보냅니다. `shunt`는 그 앞(`ANTHROPIC_BASE_URL`을 통해)에 위치하여, 매핑한 모델에 한해 추론을 다른 프로바이더(OpenAI, Codex/ChatGPT 등)로 우회시킵니다. 라우팅이 HTTP/추론 계층에서 일어나며 작업을 다른 CLI로 넘기는 것이 아니기 때문에, 세션은 계속 Claude Code의 하네스 안에서 실행됩니다. 동일한 도구 루프, 동일하게 프리로드된 스킬, 동일한 번들 스크립트 경로 해석이 유지됩니다. 오직 토큰 생성만 외주됩니다.

이는 대안적 접근(작업을 `subagent_type`으로 Codex CLI 같은 다른 런타임에 넘기는 방식)과 대조됩니다. 그 방식은 스택의 더 위쪽을 끊어내어 페르소나와 프리로드된 스킬을 잃습니다.

### 에이전트별이 아닌 모델별 — 그리고 전역 교체가 아님

선택성은 **각 요청의 `model` id**로 결정되며, Claude Code는 이미 이를 컨텍스트별로 선택할 수 있게 해줍니다. 메인 세션은 `/model` 선택기, 서브에이전트 정의는 `model:` 프론트매터, 모든 서브에이전트는 `CLAUDE_CODE_SUBAGENT_MODEL`, 선택기에 커스텀 항목을 추가하려면 `ANTHROPIC_CUSTOM_MODEL_OPTION`을 사용합니다. 따라서 "이 에이전트만 / 이 세션만 우회"는 Claude Code에서 결정되고, shunt는 받은 model id만 그대로 존중합니다. 취약한 에이전트별 시스템 프롬프트 지문 인식은 없습니다. 전역 모델 교체 프록시와 달리, 메인 세션은 Claude에 그대로 두고 지정한 모델만 우회할 수 있습니다.

## Claude Code 통합(공식 표면)

Claude Code는 `ANTHROPIC_BASE_URL` 뒤에 **1급 게이트웨이 계약**을 노출합니다. `shunt`는 이전 Claude Code 프록시들이 의존하던 취약한 "서브에이전트 시스템 프롬프트 해시" 휴리스틱 대신 이 계약을 구현합니다.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — API 계약입니다. 엔드포인트, 전달 대 소비할 헤더/바디 필드, 기능 패스스루, 어트리뷰션을 정의합니다. 실행 중인 게이트웨이는 `GET /protocol`에서 기계 판독 가능한 스펙을 제공합니다.
  - [모델 디스커버리](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code는 시작 시 `GET /v1/models?limit=1000`을 쿼리하고(`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`로 옵트인), 반환된 모델을 `/model` 선택기에 추가합니다. 기본값인 `auto_include_builtin_models = true`는 자동으로 발견된 모델을 선별된 `[[models]]` 항목 뒤에 id 기준으로 중복을 제거해 덧붙입니다. 엄격하게 선별된 목록만 원하면 `false`로 설정하세요. 이 모델들은 `server.default_provider`가 Anthropic 계열일 때 그 프로바이더에 대한 실시간 `GET /v1/models` 호출에서 오며, 해당 프로바이더의 인증 모드를 사용합니다. `passthrough`는 호출자의 자격 증명을 전달하므로 호출자마다 자신이 권한을 가진 목록을 보게 되고, `api_key`는 구성된 키를 사용하며, `claude_oauth`는 추론과 동일한 유효 계정 집합에서 — `account_scope` 순서의 스토어 검색 계정을 포함해 — 처음으로 확인 가능하고 비활성화되지 않은 계정을 풀 선택, 쿨다운, 쿼터 집계 없이 사용합니다. 뒤의 두 모드는 게이트웨이 자격 증명 범위의 공유 카탈로그를 노출합니다. shunt는 아무것도 캐시하지 않으며, 기본 프로바이더가 Anthropic 계열이 아니거나 자격 증명이 없거나 호출이 실패하면(2초 상한) 내장 Claude 카탈로그 스냅샷으로 폴백합니다. 선별된 항목에는 `[models.upstream_model]` 맵을 포함할 수도 있는데(순서가 있는 `[[upstreams]]`에서는 여러 항목, 레거시 프로바이더에서는 한 항목), 이 경우 광고된 id가 매핑된 업스트림을 통해 라우팅 가능해지고 별도의 `[[routes]]` 항목 없이 각 매핑 백엔드 id로 변환됩니다. **제약:** `id`가 `claude`/`anthropic`으로 시작하지 않는 항목은 무시됩니다. Claude가 아닌 모델은 별칭을 붙이거나 수동으로 추가해야 합니다.
  - **시스템 프롬프트 어트리뷰션 블록** — Claude Code는 클라이언트 버전 + 대화 지문을 시스템 프롬프트 앞에 추가하며, 대화 수명 동안 안정적으로 유지됩니다(v2.1.181+). `shunt`는 이를 변경 없이 전달합니다(절대 제거하지 않으며, 그것은 `CLAUDE_CODE_ATTRIBUTION_HEADER=0`을 통한 개발자의 선택입니다).
- [커스텀 모델 옵션 추가](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION`은 내장 별칭을 대체하지 않고 게이트웨이로 라우팅되는 항목을 `/model` 선택기에 추가합니다. 이 ID는 검증을 건너뛰므로 게이트웨이가 받아들이는 문자열이면 무엇이든 동작합니다. 디스커버리가 `claude`/`anthropic`으로 시작하지 않는 id를 무시하므로, **이것이 Claude가 아닌 모델을 선택하는 기본 방법**입니다(예: `gpt-5.6-sol`).
- **도구 검색**(`ENABLE_TOOL_SEARCH`) — Claude Code는 MCP/LSP 도구 스키마를 지연시켜 두었다가 `ToolSearch` 도구로 필요할 때 드러내며, 모델이 호출하지도 않을 도구에 쓰였을 컨텍스트를 되찾습니다. shunt는 퍼스트파티 Anthropic 호스트가 아니므로, `ENABLE_TOOL_SEARCH=true`로 옵트인하지 않는 한 Claude Code는 이 기능을 **꺼 둡니다**. Messages 경로에서 지연이 유지되는지는 설정이 아니라 업스트림 모델이 결정합니다. `claude*`와 `anthropic/*` id는 프로토콜을 바이트 단위로 유지하지만, Anthropic이 아닌 id(OpenRouter의 스텔스 슬러그, Kimi 등)는 `defer_loading` 마커와 `tool_search_tool_*` 항목이 제거됩니다. 그런 호스트는 이를 그대로 거부하기 때문입니다(`400 Deferred custom tools are only supported on Anthropic models...`). 도구 자체는 여전히 전달되지만 전체 스키마와 함께 즉시 전달되므로, 그런 모델에서는 도구 검색이 컨텍스트를 되찾아 주지 못합니다. Codex/Responses 경로에서 `[providers.<name>]` 아래의 `tool_search`는 세 가지 상태를 갖는 설정입니다. 설정하지 않으면(기본값 "auto") 이미 구현한 것으로 알려진 업스트림 — ChatGPT/Codex 백엔드와 `api.openai.com` — 에 대해서만 Responses API 자체의 네이티브 클라이언트 실행 `tool_search` 프로토콜에 매핑하고, 그 외 모든 OpenAI 호환 엔드포인트(LiteLLM, vLLM, OpenRouter, 자체 호스팅 프록시 등)에는 #43 텍스트 shim을 유지합니다. `tool_search = true`는 업스트림의 flavor와 모델이 조건을 만족하면(xAI/Grok이 아니고 gpt-5.4 이상) 네이티브를 강제하므로 검증한 커스텀 엔드포인트를 옵트인할 수 있습니다. `tool_search = false`는 항상 shim을 강제하며, 이 shim은 드러난 도구를 캐시된 `tools` 프리픽스에 추가하고 드러날 때마다 프리픽스를 무효화합니다. [도구 검색](https://shunt.dev/ko/guides/codex/#tool-search) 가이드를 참고하세요.

**설계 원칙:** 스펙을 준수하는 Anthropic-Messages 게이트웨이가 되고(`/v1/messages`, `/v1/models`, 올바른 헤더/어트리뷰션 패스스루), 요청의 `model` id로 라우팅하며, 매핑된 모델에 대해 Anthropic Messages ⇄ OpenAI Responses API를 변환합니다. Claude Code 프롬프트가 바뀔 때마다 깨지는 프롬프트 형태 휴리스틱은 없습니다.

## 관련 작업 / 선행 사례

**Claude Code 전용 라우터 및 프록시**

- [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) — 이 분야에서 가장 큰 프로젝트로, Claude Code를 기반으로 요청이 서로 다른 모델/프로바이더에 도달하는 방식을 결정합니다.
- [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) — Claude Code를 OpenAI 모델에서 실행합니다.
- [fuergaosi233/claude-code-proxy](https://github.com/fuergaosi233/claude-code-proxy) — Claude Code → OpenAI API 프록시.
- [seifghazi/claude-code-proxy](https://github.com/seifghazi/claude-code-proxy) — 진행 중인 Claude Code 요청을 캡처/시각화하며, 다른 프로바이더로의 선택적 **에이전트별** 라우팅을 지원합니다(`shunt`의 서브에이전트 라우팅 아이디어에 직접적인 영감을 준 프로젝트).
- [luohy15/y-router](https://github.com/luohy15/y-router) — Claude Code가 OpenRouter와 함께 작동하도록 하는 간단한 프록시입니다.
- [tingxifa/claude_proxy](https://github.com/tingxifa/claude_proxy) — Claude API 요청을 OpenAI 형식으로 변환하는 Cloudflare Workers 프록시(Gemini, Groq, Ollama).
- [badlogic/claude-bridge](https://github.com/badlogic/claude-bridge) — Claude Code에서 어떤 모델 프로바이더든 사용합니다.
- [jimmc414/claude_n_codex_api_proxy](https://github.com/jimmc414/claude_n_codex_api_proxy) — 런타임 간 라우터: Anthropic **또는** OpenAI API 호출을 로컬 **Claude Code 또는 Codex** CLI로 프록시합니다(API 키가 모두 9인 경우 로컬 CLI로, 아니면 실제 클라우드 API로 라우팅). 방향이 반대라는 점에 유의하세요. Claude Code 에이전트를 클라우드 프로바이더로 *내보내는* 것이 아니라 클라우드 API 호출을 로컬 CLI로 라우팅합니다.
- [insightflo/chatgpt-codex-proxy](https://github.com/insightflo/chatgpt-codex-proxy) — Claude Code 추론을 **ChatGPT Codex 백엔드**에서 제공하는 Anthropic 호환 `/v1/messages` 프록시입니다(API 키 대신 ChatGPT Plus/Pro 구독 사용). `shunt`와 동일한 추론 계층 교체로, Claude Code의 UI와 MCP 도구를 유지하면서 Codex/GPT 구독 백엔드를 대상으로 합니다.

**범용 AI 게이트웨이(인접 인프라 — 백엔드가 될 수 있음)**

- [BerriAI/litellm](https://github.com/BerriAI/litellm) — 100개 이상의 LLM API를 OpenAI 형식으로 호출하는 SDK + 프록시/AI 게이트웨이로, 비용 추적, 가드레일, 로드 밸런싱을 제공합니다.
- [Portkey-AI/gateway](https://github.com/Portkey-AI/gateway) — 통합 가드레일과 함께 1,600개 이상의 LLM으로 라우팅하는 빠른 AI 게이트웨이입니다.
- [maximhq/bifrost](https://github.com/maximhq/bifrost) — 적응형 로드 밸런싱과 1000개 이상 모델 지원을 갖춘 고성능 AI 게이트웨이입니다.
- [mazori-ai/modelgate](https://github.com/mazori-ai/modelgate) — 오픈소스 LLM 게이트웨이 + MCP 서버(Go): RBAC/정책 시행, 멀티 프로바이더(OpenAI, Anthropic, Gemini, Bedrock, Azure, 로컬 Ollama), 시맨틱 도구 검색을 갖춘 MCP 게이트웨이, 시맨틱 응답 캐싱을 제공합니다.

### `shunt`는 어떻게 다른가

위의 대부분의 Claude Code 프록시는 **모든** 트래픽을 하나의 대체 프로바이더로 라우팅합니다(전역 모델 교체). `shunt`의 초점은 요청의 `model` id로 결정되는 **선택적, 모델별** 우회입니다. 메인 세션은 Claude에 두고, 지정한 모델만 다른 프로바이더로 우회합니다. 스위치보드/패치베이 활용 사례입니다. Claude Code는 이미 컨텍스트별로 모델을 바인딩할 수 있게 해주므로(메인 세션, 서브에이전트 `model:` 프론트매터, `CLAUDE_CODE_SUBAGENT_MODEL`), 그 동일한 선택성이 shunt가 호출자가 누구인지 조사하지 않고도 개별 에이전트까지 도달합니다.

## 기여

이슈와 PR을 환영합니다. 빌드/테스트 명령과 컨벤션은 [`CONTRIBUTING.md`](CONTRIBUTING.md)와 [`AGENTS.md`](AGENTS.md)를, 취약점 보고는 [`SECURITY.md`](SECURITY.md)를 참고하세요.

### 코드 리뷰

`shunt`의 풀 리퀘스트는 두 개의 AI 코드 리뷰어 도구가 검토하며, 둘 다 오픈소스 프로젝트에 무료로 제공됩니다.

- [Greptile](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source) — OSS 프로그램에 따라 비상업적 MIT/Apache 프로젝트에 무료.
- [cubic](https://cubic.dev/) — 공개 저장소에 무료.

## 라이선스

[Apache License, Version 2.0](LICENSE-APACHE) 또는 [MIT license](LICENSE-MIT) 중 하나를 선택하여 사용할 수 있습니다. 명시적으로 달리 명시하지 않는 한, Apache-2.0 라이선스에 정의된 대로 귀하가 이 크레이트에 포함하기 위해 의도적으로 제출한 모든 기여는 추가 조건 없이 위와 같이 이중 라이선스가 부여됩니다.

---

Made with Orca 🐋

- https://github.com/stablyai/orca
- https://www.onorca.dev/
