# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#라이선스)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

[English](README.md) · **한국어** · [日本語](README.ja.md) · [简体中文](README.zh-CN.md)

> Claude Code를 어떤 모델로든 우회(shunt)하세요.

`shunt`는 스펙을 준수하는 [Claude Code LLM 게이트웨이](https://code.claude.com/docs/en/llm-gateway-protocol)입니다. **매핑한 모델**에 한해 추론을 **추론 계층**에서 다른 LLM 프로바이더로 우회시키는 투명 프록시입니다. 요청의 `model` id를 기준으로 라우팅하며, 그 외 모든 것은 변경 없이 Anthropic으로 그대로 전달됩니다(이것이 "shunt"이며, 폴백은 `server.default_provider`로 구성할 수 있습니다).

이름 자체가 동작 방식을 나타냅니다. 전기/철도의 *shunt*는 흐름의 일부를 선택해 병렬 경로로 우회시킵니다. 여기서는 매핑된 모델의 추론이 다른 프로바이더로 우회되는 동안 Claude Code의 도구와 스킬은 그대로 유지됩니다.

OpenAI, ChatGPT/Codex, xAI, Grok, Cursor, Kimi Code, Zhipu, MiniMax 중국, Gemini, Antigravity, Anthropic 패스스루가 기본 내장되어 있고, 이 가운데 여럿은 이미 결제 중인 구독을 그대로 재사용합니다. Anthropic Messages 호환 백엔드라면 무엇이든 구성 테이블 하나로 붙일 수 있으며, 코드 변경은 필요 없습니다. [프로바이더](#프로바이더)를 참고하세요.

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

### 기본 내장

다음 프로바이더는 기본으로 시드되어 있어, 직접 작성한 `[providers.*]` 테이블 없이 `provider = "<이름>"`만으로 라우팅됩니다 — **단 `[[upstreams]]`를 선언하지 않은 경우에만** 해당합니다. 순서가 있는 `[[upstreams]]` 목록은 프로바이더 맵을 통째로 대체하므로, 그 형식에서는 프리셋을 포함해 라우팅 대상 프로바이더를 모두 거기에 선언해야 합니다.

| 이름 | 종류 | 인증 | 백엔드 |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough 또는 Claude OAuth 계정 풀 | `api.anthropic.com` — 기본적으로 호출자 본인의 자격 증명을 전달하며, `auth = "claude_oauth"`를 쓰면 풀링된 구독 자격 증명을 사용합니다 |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — `~/.codex/auth.json`(`codex login`)을 재사용 |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` — 개발자 API, 토큰당 과금 |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` — Grok CLI 프록시, `~/.shunt/xai-auth.json`을 재사용(SuperGrok / X Premium+ 구독으로 `shunt login xai`) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` — `~/.shunt/cursor-auth.json`(`shunt login cursor`)을 재사용 |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` — Google Code Assist 백엔드, `~/.gemini/oauth_creds.json`을 재사용 |
| `antigravity` | `antigravity` | Antigravity OAuth | `daily-cloudcode-pa.googleapis.com` — HTTP로 통신하는 Google Antigravity 백엔드, `~/.shunt/antigravity-auth.json`(`shunt login antigravity`)을 사용 |
| `antigravity-cli` | `antigravity_cli` | 없음(로컬 CLI) | **Deprecated.** 로컬 `agy` 바이너리 — 서브프로세스로 동일한 백엔드를 사용하며, 위의 `antigravity`로 대체되었습니다 |

순서가 있는 `[[upstreams]]` 항목은 여기에 더해 `kimi`, `kimi-code`, `zhipu`, `minimax-cn` 프리셋도 받으며, 각 백엔드의 `kind`, `base_url`, 기본 인증을 채워 넣습니다.

프로바이더별 설정과 모델 id, 주의 사항은 [프로바이더](https://shunt.dev/ko/guides/providers/)에 정리되어 있습니다. xAI의 OAuth 등급 제한([xAI / Grok](https://shunt.dev/ko/guides/xai/)), Cursor의 에이전트 모드 프리픽스([Cursor](https://shunt.dev/ko/providers/cursor/)), Antigravity의 두 가지 전송 방식과 `kind = "antigravity"` 마이그레이션([Antigravity](https://shunt.dev/ko/providers/antigravity/))도 그곳에 있습니다.

> [!WARNING]
> `antigravity-cli`는 더 이상 권장되지 않으며 **임의 코드 실행**입니다. 로컬 `agy` 바이너리를 `--dangerously-skip-permissions`와 함께 에이전트 모드로, shunt를 실행한 사용자 권한으로 구동합니다. `sandbox` 설정을 켠 채로 두고 바인드는 루프백에 두세요. 이런 것이 전혀 필요 없는 위의 `antigravity` 프로바이더를 권장합니다. [더 이상 사용되지 않는 전송 방식](https://shunt.dev/ko/guides/providers/#더-이상-사용되지-않는-antigravity_cli-전송)을 참고하세요.

### Anthropic 호환 백엔드

테이블 하나면 되고, 코드 변경은 필요 없습니다.

| 프로바이더 | `base_url` | 예시 모델 ID |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| Kimi Code (구독, OAuth) | `https://api.kimi.com/coding` | 구독에서 제공하는 ID 사용 |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| Zhipu (GLM 중국) | `https://open.bigmodel.cn/api/anthropic` | `glm-5.3`, `glm-5.3-flash` |
| MiniMax | `https://api.minimax.io/anthropic` | [MiniMax 문서](https://platform.minimax.io/docs/token-plan/claude-code) 참고 |
| MiniMax 중국 | `https://api.minimax.cn/anthropic` | `MiniMax-M3` |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"
```

위 표의 행은 대부분 `auth = "api_key"`를 사용합니다. **Kimi Code**만 예외입니다. 종량 과금인 Moonshot API와는 별개인 구독 기반 서비스로, 호스트가 다르고 API 키 대신 OAuth를 쓰며, 내장 `kimi-code` 프리셋이 있습니다. 이 프리셋은 순서가 있는 `[[upstreams]]` 항목 안에서만 해석되므로(시드된 프로바이더 맵에는 없습니다) 거기에 선언한 뒤 로그인하세요. [Kimi Code](https://shunt.dev/ko/providers/kimi/#kimi-code-oauth-구독)를 참고하세요.

### 구독 재사용

OpenAI의 Thibault Sottiaux는 다른 코딩 하네스를 통해 Codex를 실행하는 것을 공개적으로 환영했습니다.

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([출처](https://x.com/thsottiaux/status/2075830097488249060))

그는 [후속 글](https://x.com/thsottiaux/status/2076119366647894371)에서 Claude Code("당신의 주황색 게")를 GPT-5.6 Sol에 직접 연결하는 과정을 설명했습니다. `shunt`가 수행하는 추론 계층 교체와 정확히 같으며, 별도의 앱이 필요 없습니다.

다만, 비공식 클라이언트에서 ChatGPT/Codex나 SuperGrok 구독(또는 Kimi, Cursor 등 다른 백엔드)을 재사용하는 것은 본인의 판단입니다. 공개적인 환영이 향후 정책이나 계정 제재가 없음을 보장하지는 않습니다. 사용에 따른 책임은 본인에게 있습니다.

**Antigravity는 약관이 이를 명시한 예외입니다.** Google의 [Antigravity 약관](https://antigravity.google/terms)은 "서드파티 소프트웨어, 도구, 서비스로 서비스에 접근하는 것(예: OpenClaw를 Antigravity OAuth와 함께 사용)은 본 계약 위반"이며, 그러한 위반은 "Antigravity 및/또는 Gemini CLI 계정의 정지 또는 해지 사유가 될 수 있다"고 명시합니다. shunt의 `antigravity` 프로바이더가 바로 그것 — Antigravity OAuth를 사용하는 서드파티 소프트웨어 — 이므로, 이 프로바이더로 라우팅하는 것은 그 조항에 그대로 해당합니다. `shunt login antigravity`를 실행하기 전에 이를 감안해 결정하세요.

## 선택 기능

행에 별도 표기가 없으면 **기본 비활성**입니다. 해당 구성 테이블이 없으면 라우트도 등록되지 않고 백그라운드 작업도 시작되지 않습니다.

| 기능 | 활성화 키 | 문서 |
| :-- | :-- | :-- |
| Anthropic 멀티 계정 풀링 — 스티키 세션, 쿼터 인식 로테이션, 예측 회피 | 계정 2개 이상인 `auth = "claude_oauth"`; `[server.pool]`은 선택적 튜닝 | [가이드](https://shunt.dev/ko/guides/anthropic-multi-account/) |
| Codex 멀티 계정 풀링 — `x-codex-*` 윈도우 추적, 슬로우 스타트 램프, 재프로브 | 계정 2개 이상인 `auth = "chatgpt_oauth"`; `[server.pool]`은 선택적 튜닝 | [가이드](https://shunt.dev/ko/guides/codex-multi-account/) |
| 인바운드 Codex 엔드포인트 — **Codex CLI**를 shunt로 향하게 해 같은 풀에 태우고, 모델별 라우팅도 선택할 수 있음 | `[server.codex_endpoint]` | [가이드](https://shunt.dev/ko/guides/inbound-codex-endpoint/) |
| Claude 앱 게이트웨이 로그인 — OAuth device flow, managed settings, 사용자별 정책 | `public_url`, 32바이트 이상 JWT 시크릿, 정적 사용자 또는 `[server.gateway.oidc]`를 갖춘 `[server.gateway]` | [가이드](https://shunt.dev/ko/guides/gateway-login/) |
| 게이트웨이 텔레메트리 인제스트 — 관리 클라이언트의 OTLP를 그대로 릴레이 | 구성된 `[server.gateway]`와 `forward_to`가 비어 있지 않은 `[server.gateway.telemetry]` | [레퍼런스](https://shunt.dev/ko/reference/configuration/#servergatewaytelemetry-선택) |
| 관리자 웹 화면 — 계정·사용량 대시보드, 브라우저 프로비저닝 | `[server.admin]`, `shunt dashboard setup` | [가이드](https://shunt.dev/ko/guides/admin-remote-provisioning/) |
| 지출 한도 Admin API — 조직·사용자 단위 상한(1단계는 저장만 하고 아직 적용하지 않음) | `[server.admin]` + `[server.spend]` | [레퍼런스](https://shunt.dev/ko/reference/configuration/#serverspend-선택) |
| 클라이언트 사용량 엔드포인트 — `GET /usage`가 정제·집계된 풀 여유를 반환 | `[server.auth]` + `[server.usage]` | [레퍼런스](https://shunt.dev/ko/reference/configuration/#serverusage-선택) |
| Claude Code CLI 네이티브 사용량 막대 — `GET /api/oauth/usage` 제공 | `[server.oauth_usage]`, 루프백이 아닌 bind에서는 `[server.auth]` 또는 `[server.gateway]` 추가 필요 | [레퍼런스(영문)](https://shunt.dev/reference/configuration/#serveroauth_usage-optional) |
| 업스트림 상태 폴링 — 대시보드와 메트릭에 Statuspage 지표 노출 | `[[server.status.sources]]` 항목이 하나 이상 있는 `[server.status]` | [레퍼런스](https://shunt.dev/ko/reference/configuration/#serverstatus-선택) |
| 제한된 업스트림 재시도 — **기본 활성**, 보수적이며 스트림 도중에는 재시도하지 않음 | `[providers.<name>.retry]` | [레퍼런스(영문)](https://shunt.dev/reference/configuration/#providersnameretry) |
| 공유 배포 제한 — **기본 활성**(동시 1024, 본문 32 MiB, TTFB 120초, device-flow 레이트 리밋), CIDR·헤더·URL 제한은 선택 | `[server] max_concurrent_requests`, `[server.access_control]`, `[server.limits]`, `[server.timeouts]`, `[server.rate_limits]` | [가이드](https://shunt.dev/ko/guides/shared-gateway/) |
| 시크릿 참조 — 모든 문자열 값에 `${VAR}` 또는 `${file:/abs/path}`, 핫 리로드마다 다시 확인(`[sentry]`·`[otel]` 제외 — 기동 시 1회 구성이라 재시작 필요) | 구성의 모든 문자열(**항상 활성**) | [레퍼런스](https://shunt.dev/ko/reference/configuration/) |
| OpenTelemetry 메트릭과 트레이스 | `endpoint`가 비어 있지 않은 `[otel]` | [가이드](https://shunt.dev/ko/guides/opentelemetry/) |

## 문서

사용자 문서는 모두 **[shunt.dev](https://shunt.dev)**에 있습니다.

- [빠른 시작](https://shunt.dev/getting-started/quickstart/) · [왜 shunt인가?](https://shunt.dev/getting-started/why-shunt/) · [프로바이더](https://shunt.dev/guides/providers/) · [구성](https://shunt.dev/guides/configuration/) · [문제 해결](https://shunt.dev/reference/troubleshooting/)
- **에이전트용:** 모든 페이지에는 Markdown 쌍둥이 페이지가 있으며(임의의 URL에 `.md`를 붙이거나 페이지의 *Copy Markdown* / *Open in AI* 버튼 사용), 사이트는 [llms.txt 스펙](https://llmstxt.org/)에 따라 [`/llms.txt`](https://shunt.dev/llms.txt), [`/llms-small.txt`](https://shunt.dev/llms-small.txt), [`/llms-full.txt`](https://shunt.dev/llms-full.txt)를 게시합니다.

기여자를 위한 설계 노트와 마일스톤 스펙은 [`docs/`](docs/)에 있습니다. [`docs/implementation-plan.md`](docs/implementation-plan.md)부터 보세요.

## 왜

Claude Code는 모든 턴을 Anthropic API로 보냅니다. `shunt`는 그 앞(`ANTHROPIC_BASE_URL`을 통해)에 위치하여, 매핑한 모델에 한해 추론을 다른 프로바이더(OpenAI, Codex/ChatGPT 등)로 우회시킵니다. 라우팅이 HTTP/추론 계층에서 일어나며 작업을 다른 CLI로 넘기는 것이 아니기 때문에, 세션은 계속 Claude Code의 하네스 안에서 실행됩니다. 동일한 도구 루프, 동일하게 프리로드된 스킬, 동일한 번들 스크립트 경로 해석이 유지됩니다. 오직 토큰 생성만 외주됩니다.

이는 대안적 접근(작업을 `subagent_type`으로 Codex CLI 같은 다른 런타임에 넘기는 방식)과 대조됩니다. 그 방식은 스택의 더 위쪽을 끊어내어 페르소나와 프리로드된 스킬을 잃습니다.

### 에이전트별이 아닌 모델별 — 그리고 전역 교체가 아님

선택성은 **각 요청의 `model` id**로 결정되며, Claude Code는 이미 이를 컨텍스트별로 선택할 수 있게 해줍니다. 메인 세션은 `/model` 선택기, 서브에이전트 정의는 `model:` 프론트매터, 모든 서브에이전트는 `CLAUDE_CODE_SUBAGENT_MODEL`, 선택기에 커스텀 항목을 추가하려면 `ANTHROPIC_CUSTOM_MODEL_OPTION`을 사용합니다. 따라서 "이 에이전트만 / 이 세션만 우회"는 Claude Code에서 결정되고, shunt는 받은 model id만 그대로 존중합니다. 취약한 에이전트별 시스템 프롬프트 지문 인식은 없습니다. 전역 모델 교체 프록시와 달리, 메인 세션은 Claude에 그대로 두고 지정한 모델만 우회할 수 있습니다.

## Claude Code 통합(공식 표면)

Claude Code는 `ANTHROPIC_BASE_URL` 뒤에 **1급 게이트웨이 계약**을 공개합니다. `shunt`는 이전 Claude Code 프록시들이 기대던 "서브에이전트 시스템 프롬프트 해싱"이라는 취약한 휴리스틱 대신 이 계약을 구현합니다.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — 엔드포인트, 전달할 헤더·본문 필드와 소비할 필드, 기능 패스스루, 어트리뷰션을 규정한 API 계약입니다. 실행 중인 게이트웨이는 `GET /protocol`에서 기계가 읽을 수 있는 스펙을 제공합니다. Claude Code는 클라이언트 버전과 대화 지문을 시스템 프롬프트 앞에 붙이는데, 이를 없앨지는 `CLAUDE_CODE_ATTRIBUTION_HEADER=0`으로 개발자가 정할 몫이므로 shunt는 그 어트리뷰션 블록을 그대로 전달합니다.
- [모델 디스커버리](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code는 시작 시 `GET /v1/models?limit=1000`을 조회해(`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`로 옵트인) 반환된 모델을 `/model` 선택기에 추가합니다. shunt는 큐레이션된 `[[models]]` 항목에 더해, `auto_include_builtin_models`가 `true`인 동안에는 호출자의 라이브 카탈로그로 응답합니다 — 이 조회는 `server.default_provider`가 Anthropic 종류일 때만 이뤄지며, 그렇지 않거나 크리덴셜이 없거나 조회가 실패하면 내장 스냅샷으로 대체됩니다. **제약:** `id`가 `claude`/`anthropic`으로 시작하지 않는 항목은 무시되므로, Claude 계열이 아닌 모델은 별칭을 만들거나 수동으로 추가해야 합니다. [모델 디스커버리](https://shunt.dev/ko/guides/model-discovery/)를 참고하세요.
- [커스텀 모델 옵션 추가](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION`은 내장 별칭을 대체하지 않으면서 게이트웨이로 라우팅되는 항목을 `/model` 선택기에 추가합니다. ID는 검증을 거치지 않으므로 게이트웨이가 받아들이는 문자열이면 무엇이든 됩니다. 위의 디스커버리 제약 때문에 **Claude 계열이 아닌 모델을 고르는 주된 방법**입니다(예: `gpt-5.6-sol`).
- **도구 검색**(`ENABLE_TOOL_SEARCH`) — Claude Code는 MCP/LSP 도구 스키마를 지연시켰다가 필요할 때 드러내어 컨텍스트를 회수합니다. shunt는 Anthropic 1급 호스트가 아니므로 직접 옵트인하지 않는 한 이 기능은 **꺼진 상태**입니다. 옵트인 후 지연이 유지되는지는 설정이 아니라 업스트림이 결정합니다. `claude*`와 `anthropic/*` id는 프로토콜을 바이트 단위로 유지하고, 그 외 id는 해당 호스트가 거부하므로 `defer_loading` 표식이 제거되며, Responses 경로에는 자체적인 3-상태 `tool_search` 설정이 있습니다. [도구 검색](https://shunt.dev/ko/guides/codex/#도구-검색)을 참고하세요.

**설계 원칙:** 스펙을 준수하는 Anthropic-Messages 게이트웨이가 되고(`/v1/messages`, `/v1/models`, 올바른 헤더·어트리뷰션 패스스루), 요청의 `model` id로 라우팅하며, 매핑된 모델에 대해 Anthropic Messages ⇄ OpenAI Responses API를 번역합니다. Claude Code 프롬프트가 바뀔 때마다 깨지는 프롬프트 형태 휴리스틱은 쓰지 않습니다.

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