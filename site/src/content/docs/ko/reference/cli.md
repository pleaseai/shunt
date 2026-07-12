---
title: CLI
description: shunt 커맨드 라인 — run, check, token.
---

## `shunt run`

게이트웨이를 시작합니다. `run`은 기본 서브커맨드이므로, 맨 `shunt`만으로도 동작합니다.

```bash
shunt run
shunt run --config /path/to/shunt.toml
```

시작 시 바인딩된 주소(기본 `127.0.0.1:3001`)와 함께 `shunt listening`을 로깅합니다. 로그 상세도는 `RUST_LOG`로 설정하세요, 예: `RUST_LOG=shunt=debug shunt run`.

`--config` 없이 shunt는 `./shunt.toml` → `~/.config/shunt/shunt.toml` → `$HOMEBREW_PREFIX/etc/shunt.toml` 순서로 검색합니다; `--config`를 사용하면 파일이 없는 것은 오류입니다. [구성](/ko/guides/configuration/)을 참고하세요.

## `shunt check`

해석된 구성을 검증하고 종료합니다(`shunt --check`도 동작):

```bash
shunt check
# -> config ok
```

구체적인 오류를 보고합니다: 잘못된 bind 주소, 라우트의 알 수 없는 프로바이더, 누락된 `api_key_env`, 잘못된 `base_url`, 잘못된 어댑터/인증 조합.

## `shunt token`

Claude 구독 OAuth 토큰을 **stdout**으로 출력하며(로그는 stderr로), Claude Code의 `apiKeyHelper`에 연결되도록 설계되었습니다. 두 가지 모드:

- **정적** — `SHUNT_GATEWAY_TOKEN` 또는 `CLAUDE_CODE_OAUTH_TOKEN`이 설정되어 있으면, 그 값을 변경 없이 그대로 출력합니다. `claude setup-token` 값을 가리키게 하면 아무것도 갱신되지 않습니다.
- **자동 갱신** — 그렇지 않으면 `~/.claude/.credentials.json`을 읽고(경로는 `CLAUDE_CREDENTIALS`로 오버라이드), `claudeAiOauth` 액세스 토큰을 반환하며, `expiresAt` 5분 이내일 때 `platform.claude.com/v1/oauth/token`에 대해 갱신하여(Claude Code가 사용하는 것과 동일한 grant), 새 토큰을 `0600`으로 원자적으로 다시 쓰고 다른 모든 필드를 보존합니다. 갱신은 엔드포인트의 rate limit을 존중하기 위해 실제 만료 시에만 일어납니다.

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

이것이 필요한 경우는 [Claude Code 연결](/ko/guides/connect-claude-code/#2-choose-the-anthropic-credential)을 참고하세요.

## 환경 변수

| 변수 | 효과 |
| :-- | :-- |
| `SHUNT_*`(예: `SHUNT_SERVER__BIND`) | 임의의 구성 키를 오버라이드; `__`는 중첩 키를 구분 |
| `RUST_LOG` | 로그 필터, 예: `shunt=debug` |
| `SHUNT_CLIENT_TOKENS` | [`[server.auth]`](/ko/guides/shared-gateway/)용 클라이언트 토큰(이름은 `tokens_env`로 구성 가능) |
| `SHUNT_GATEWAY_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN` | `shunt token`용 정적 토큰 |
| `CLAUDE_CREDENTIALS` | `shunt token`용 대체 자격 증명 파일 경로 |
| `OPENAI_API_KEY` | `openai` 프로바이더용 기본 키 env(프로바이더별로 `api_key_env`를 통해) |
