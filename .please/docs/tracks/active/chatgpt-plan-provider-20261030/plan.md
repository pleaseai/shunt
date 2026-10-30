# Plan: ChatGPT Plan Provider (Sign in with ChatGPT)

> Track: chatgpt-plan-provider-20261030
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: chatgpt-plan-provider-20261030
- **Issue**: #863
- **Created**: 2026-10-30
- **Approach**: New `chatgpt_plan_oauth` auth mode with its own account store and pool family, reusing the `OpenAi` Responses dialect unchanged; pool rotation extended with a pre-content check that classifies an early in-stream SIWC failure like an HTTP status (ADR-0007)
- **Plan Depth**: deep
- **Planned At**: 2226f47

## Context

### Problem

shunt reaches a ChatGPT subscription only through the Codex CLI login; OpenAI's SIWC preview — the sanctioned path for open-source tools — is not supported (spec Overview).

### Requirements Summary

Values the code must use, confirmed live on 2026-10-30 (requirements themselves are in the spec):

- Issuer `https://auth.openai.com`; authorize `…/api/accounts/authorize`; token `…/api/accounts/oauth/token`; revocation `…/api/accounts/oauth/revoke` (read from `/.well-known/openid-configuration`, public client, form POST `token`, `token_type_hint=refresh_token`, `client_id`; HTTP 200 = success).
- Authorize params: first sign-in `client_id=dynamic_agent_client` + `agent_name_hint=shunt`; re-sign-in the saved `client_id` and no hint. Always `ext_agent_host_id`, `response_type=code`, `redirect_uri=http://127.0.0.1:1455/auth/callback`, `scope=openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, `resource=https://api.openai.com/v1`, `state`, `nonce`, S256 challenge. The new-registration callback carries `client_id=oaiapp_…`.
- Token exchange and refresh POST the issued `client_id` and `resource`. The response has `access_token`, `refresh_token`, `id_token`, `expires_in` (3600), `scope`, `earliest_refresh_at` (Unix seconds, integer).
- ID token: RS256 with `kid`; `aud` is a JSON array containing the issued client id; no `azp`; `exp - iat` = 3600; `nonce` present.
- Error codes and statuses: `subscription_sharing_usage_limit_exceeded` 429, `_usage_unavailable` 503, `_user_unavailable` 503, `_unsupported_capability` 400, `_user_not_eligible` 403, `_route_not_supported` 403, `_invalid_user` 401, `chatpass_v2_scope_not_authorized` / `chatpass_v2_invalid_authorization_context` 403. Parameter rejections arrive as `{"detail": "..."}`.

### Constraints

- NFR-1 streaming: only Responses lifecycle frames (`response.created`, `response.in_progress`, `response.queued`) may be held before relaying; the Anthropic client already has its synthetic `message_start`.
- NFR-3: the codex provider, its store, flavor, headers, and `Config::default()` / `upstream_order` stay unchanged.
- New files only for credentials; no existing credential file changes writeback (AGENTS.md ask-first rule not triggered).
- `pool.rs` is already far over the 500-line guideline, so new pool logic goes in a sibling file.

### Non-Goals

Everything in the spec's Out of Scope. Considered and not built:

- A shared refresh-store trait across xAI/Kimi/plan — two near-identical stores do not yet justify it; revisit at a fourth pooled OAuth family.
- A public config key for the `max_output_tokens` switch — a code constant is enough until OpenAI enforces its published limitation; a key would need sign-off.
- Hard exclusion of needs-sign-in accounts — the user chose the existing cooldown + flag (spec AC-012).

### STOP Conditions

- If the live smoke check (Verification → Manual Testing) shows the preview rejects the session-affinity headers or `web_search`, stop and report before changing request shaping.
- If an in-stream usage-limit failure turns out to arrive only after content frames, stop: ADR-0007's rotation premise does not hold.

## Architecture Decision

ADR-0007 records the choice. A separate auth mode avoids the codex store's `chatgpt_account_id` requirement and keeps codex untouched. The `OpenAi` dialect is reused because the probe accepted it unchanged; the only dialect gate is keeping native `tool_search` off for this auth mode (`native_tool_search` would otherwise auto-enable it on `api.openai.com`), and `max_output_tokens` forwarding sits behind one constant. Hosted `web_search` gets no new handling: it passes through the existing `OpenAi` translation as it does today (the spec puts new hosted-tool work out of scope), which is why the smoke check confirms the preview accepts it.

Credentials live in `~/.shunt/accounts/chatgpt-plan/<name>.json` (override `SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR`), written born-private through the shared account-file writer, with the installation's host id in `.host-id` in the same directory (the account scan only reads `*.json`). A signed-out account is the file reduced to `{version, client_id}`, so re-sign-in reuses the registration and the pool resolves it as a terminal needs-sign-in. Login, refresh writeback, and logout take the shared cross-process file lock (Antigravity precedent) because the refresh token rotates and the CLI can run beside the gateway; refresh also keeps the in-process single-flight lock and the detached, cancellation-safe write of the xAI store, which is the template. Refresh errors reuse `ChatGptAuthError` / `TerminalRefresh` so the pool's existing cooldown + `needs_relogin` tail applies.

The ID token goes through `auth::inbound_jwt::verify` with a per-attempt `JwtIssuerRule`: issuer pinned, audience = issued client id, RS256, `require` = nonce, `clock_skew_seconds` = 60 on `exp`/`nbf` (jsonwebtoken does not check `iat`, which is the check that failed under the probe's 5 s lag, so AC-007 holds by construction and the test pins it), `max_token_age_seconds` comfortably above 3600, empty `authorized_parties` (no `azp`), `identity_claim` = `sub` with empty email rules (`verify` rejects a token that lacks the identity claim, and an `email` identity would also require `email_verified`); the email stored with the account is read from the token's claims after `verify` accepts it. `inbound_jwt.rs` is not edited. The callback server is widened additively — a `wait_for_callback` returning code plus the `client_id` and `scope` params, with `wait_for_code` delegating to it — and the CLI races it against a pasted callback URL read on a plain thread (a blocking-pool stdin read would hang runtime shutdown).

One SIWC code table in the model layer feeds both `backend_error_status` (in-stream) and `map_error_value` (type override, so a 503 `usage_unavailable` becomes `overloaded_error`), and appends `param` to the message. In the pool, the new mode joins the pooled dispatch; for the `ChatgptPlan` family `classify_first` and `classify_retry` first run a pre-content check on a 2xx response: hold lifecycle frames, and if the first other frame is a mapped SIWC failure, continue with a synthetic response of that status; otherwise replay the held bytes and relay untouched. The existing classification then rotates 429/5xx, refreshes once on 401, and relays other 4xx, in both the streaming and non-streaming loops. The check is skipped on idle-gated calls. A usage-limit failure that arrives after content has reached the client is not rotated: the relay ends the client stream with `rate_limit_error` (AC-019) and also cools that account (FR-12) — `backend_error_status` today never cools a pool account, so the relay phase reports the failure to the pool.

Rejected alternatives are in ADR-0007. Two more: persisting tokens before validation, as the probe script did, conflicts with AC-008; env overrides for the issuer or token URL would open an exfiltration surface, so tests inject endpoints through constructors.

## Tasks

- [x] T001 Register the chatgpt-plan auth mode, preset, and config validation (file: src/config.rs) [FR-1, FR-14, NFR-3, AC-027, AC-028]
  Validation: `auth = "chatgpt_plan_oauth"` with kind `responses` on `https://api.openai.com/v1` passes `shunt check`; a different kind, a plaintext remote URL, a non-`api.openai.com` host, or a provider named `chatgpt-plan` with another auth fails naming the offending key (AC-027); the preset is listed by `shunt init`; native `tool_search` resolves off for this mode; a codex-only config, `Config::default()`, and `upstream_order` are unchanged (AC-028); the crate compiles with the new `AuthMode`, `StoreFamily`, and `Credential` variants wired through every exhaustive match (unresolvable credentials error "run `shunt login chatgpt-plan --name <account>`").
  Method: `cargo test --all-features -- config presets upstreams init` plus `tests/check_cli.rs`, `tests/init_cli.rs`
- [x] T002 Map SIWC error codes to Anthropic errors (file: src/model/siwc_errors.rs) [FR-11, AC-017, AC-018, AC-019, AC-020, AC-021]
  Validation: for HTTP and in-stream (`response.failed`) inputs, each listed code yields its status and Anthropic type — usage limit `rate_limit_error`, usage/user unavailable `overloaded_error`, not-eligible / route / `chatpass_v2_*` `permission_error`, unsupported capability `invalid_request_error` with `(parameter: tools)` in the message, invalid user `authentication_error`; `{"detail": "Unsupported parameter: temperature"}` maps to `invalid_request_error` with that text (AC-017..021 mapping half); unknown codes keep the existing status-based mapping.
  Method: `cargo test --all-features -- siwc_errors responses::` plus the `src/adapters/responses/error.rs` wiremock tests
- [x] T003 Surface the callback's client_id and scope from the loopback listener (file: src/auth/callback.rs) [FR-2]
  Validation: a callback to `/auth/callback?code=…&state=…&client_id=oaiapp_x&scope=…` yields all three values; params beyond the captured set are ignored and oversized values rejected; every existing `wait_for_code` caller and test is unchanged.
  Method: `cargo test --all-features -- auth::callback`
- [x] T004 Store chatgpt-plan accounts and the installation host id (depends on T001) (file: src/auth/chatgpt_plan/store.rs) [FR-4, FR-7, AC-010]
  Validation: account files are written 0600 in a 0700 directory and scanned into the pool by name; two names with the same email give two records (AC-010); `.host-id` is created once as `urn:uuid:<v4>` under concurrent first use and never scanned as an account; a signed-out `{version, client_id}` file round-trips; a store directory whose path contains spaces or is a symlink keeps host id and accounts together.
  Method: `cargo test --all-features -- chatgpt_plan::store`
- [ ] T005 Verify the SIWC ID token (file: src/auth/chatgpt_plan/id_token.rs) [FR-6, AC-007, AC-008]
  Validation: a token signed by a JWKS-served RS256 key with the attempt's nonce and an `aud` array containing the client id is accepted, including when `iat` is 5 s in the future (AC-007); a wrong nonce, a wrong audience, an `exp` in the past beyond 60 s, or an unknown `kid` is rejected (AC-008); `inbound_jwt.rs` is unmodified.
  Method: `cargo test --all-features -- chatgpt_plan::id_token` (loopback wiremock issuer, RSA fixtures from `inbound_jwt/tests.rs`)
- [ ] T006 Sign in with `shunt login chatgpt-plan --name` (depends on T003, T004, T005) (file: src/auth/chatgpt_plan/login.rs) [FR-2, FR-3, FR-4, FR-5, NFR-2, AC-001, AC-002, AC-003, AC-004, AC-005, AC-006, AC-009]
  Validation: against a loopback mock authorization server, AC-001..006 and AC-009 hold; every authorize URL carries `ext_agent_host_id` equal to the stored host id (FR-4); port 1455 already bound falls back to paste-only with a notice; a pasted URL with surrounding whitespace or quotes parses; the process exits promptly when the listener wins; `--long-lived`, `--mode`, and `--manual` are rejected (the paste path is always available); no token, code, or verifier appears in output.
  Method: `cargo test --all-features -- chatgpt_plan::login` plus `main.rs` CLI parse tests
- [ ] T007 Refresh chatgpt-plan tokens before expiry (depends on T004) (file: src/auth/chatgpt_plan/auth.rs) [FR-8, AC-011, AC-012]
  Validation: an access token inside its refresh window is renewed (no earlier than `earliest_refresh_at`) with the issued client id and `resource`, and the rotated refresh token is persisted (AC-011); concurrent and cancelled callers trigger one POST and never lose a rotated token; `invalid_grant` / `invalid_client` or a signed-out or corrupt file returns a terminal error (AC-012); refresh writeback holds the cross-process file lock; a writeback that finds the file signed out under that lock (a `shunt logout` ran during the POST) discards the refreshed pair and returns a terminal error, so a logout is never undone; error strings never contain token values.
  Method: `cargo test --all-features -- chatgpt_plan::auth` (wiremock token endpoint; template `src/auth/xai/auth.rs` tests)
- [ ] T008 Serve mapped models through pooled chatgpt-plan accounts (depends on T001, T007) (file: src/adapters/responses/pool_plan.rs) [FR-1, FR-9, FR-10, FR-12, AC-013, AC-014, AC-015, AC-016, AC-020, AC-021, AC-022, AC-023, AC-024]
  Validation: against a mock `/v1/responses`, AC-013..016 hold with `Authorization: Bearer <plan token>` and no `chatgpt-account-id` or `originator` header; an HTTP 429 rotates to the next account and a 401 refreshes once then retries (AC-022, AC-023 for HTTP errors); a 403 `user_not_eligible` / `route_not_supported` / `chatpass_v2_*` relays as `permission_error` with no retry and no refresh (AC-020); a 503 `usage_unavailable` / `user_unavailable` rotates, keeps the account's credentials, and returns `overloaded_error` once the pool is exhausted (AC-021); every account cooling on HTTP 429 returns a `rate_limit_error` pointing to ChatGPT usage settings (AC-024); codex pool tests pass unchanged.
  Method: new `tests/chatgpt_plan_multi_account.rs` (template `tests/xai_multi_account.rs`, `tests/codex_multi_account.rs`) plus `cargo test --all-features -- pool`
- [ ] T009 Rotate on a pre-content in-stream SIWC failure (depends on T002, T008) (file: src/adapters/responses/pool_plan.rs) [FR-11, FR-12, NFR-1, AC-019, AC-022, AC-023, AC-024]
  Validation: AC-019, AC-022, AC-023, and AC-024 hold for in-stream failures in both the streaming and non-streaming loops; a stream reaching 64 KiB or EOF without a non-lifecycle frame is replayed byte-identical; the first content delta reaches the client without waiting for later frames (NFR-1); a usage-limit failure after content has reached the client ends the stream with `rate_limit_error` and cools the account (AC-019, FR-12); idle-gated calls skip the check.
  Method: `cargo test --all-features -- pool_plan` (new `pool_plan_tests.rs`; templates `pool_xai_tests.rs`, `early_stream_tests.rs`)
- [ ] T010 Sign out with `shunt logout chatgpt-plan --name` (depends on T004) (file: src/auth/chatgpt_plan/logout.rs) [FR-13, AC-025, AC-026]
  Validation: a 200 from the revocation endpoint (discovered, https, issuer host only) clears the tokens, keeps client id and host id, and reports confirmed (AC-025); network or 5xx failures retry with bounded backoff, then clear locally and report "remote revocation not confirmed — finish in ChatGPT settings" (AC-026); an unknown provider or missing `--name` is a usage error; existing CLI parse tests pass unchanged.
  Method: `cargo test --all-features -- chatgpt_plan::logout` plus `main.rs` CLI parse tests
- [ ] T011 Document the chatgpt-plan provider on every surface (depends on T002, T006, T010) (file: site/src/content/docs/guides/chatgpt-plan.mdx) [FR-15]
  Validation: README (+ ko/ja/zh-CN), a site guide in en/ko/ja/zh-cn with a sidebar entry, `guides/providers.mdx`, `reference/cli.mdx` (login, logout, env vars), `reference/configuration.md` (auth value, preset), `shunt.toml.example`, `docs/m19-chatgpt-plan-provider.md`, and `docs/notes/chatgpt-plan.md` (probe findings, no token material) all describe setup, paste fallback, sign-out, preview limits (including that idle-gated escalation/advisor calls do not rotate on in-stream usage limits), and error meanings (FR-15); the site builds and no locale page links an anchor missing in its locale.
  Method: `bun run build` in `site/` plus a grep of `site/dist` for each locale fragment link; `wiki/` untouched

## Key Files

### Create

- `src/auth/chatgpt_plan/{mod,store,id_token,login,auth,logout}.rs` — constants, account store and host id, ID-token check, sign-in, refresh store, sign-out
- `src/model/siwc_errors.rs` — SIWC code → status / Anthropic type / message suffix
- `src/adapters/responses/pool_plan.rs` (+ `pool_plan_tests.rs`) — plan resolver, forced refresh, pre-content check
- `tests/chatgpt_plan_multi_account.rs`
- `site/src/content/docs/{,ko/,ja/,zh-cn/}guides/chatgpt-plan.mdx`, `docs/m19-chatgpt-plan-provider.md`, `docs/notes/chatgpt-plan.md`

### Modify

- `src/config.rs`, `src/config/presets.rs`, `src/config/upstreams.rs`, `src/accounts.rs`, `src/auth/mod.rs`, `src/auth/shared.rs` (nonce helper visibility, inline identity arm)
- `src/auth/callback.rs`, `src/main.rs` (login arm, `Command::Logout`; no new `Command::Login` fields)
- `src/adapters/responses/{mod,pool,request,error,websocket,inbound,inbound_routed}.rs`, `src/model/responses.rs`
- Compile-driven arms: `src/admin/{mod,plan}.rs`, `src/usage.rs`, `src/usage_poll.rs`, `src/init.rs`, `src/blueprints.rs` (+ `blueprints/upstream/chatgpt-plan.md`), `ui/src/accounts.ts`
- Docs: `README*.md`, `site/src/lib/i18n.ts`, `site/src/content/docs/**/guides/providers.mdx`, `**/reference/{cli.mdx,configuration.md}`, `shunt.toml.example`

### Reuse

- The xAI refresh store, shared auth helpers, `inbound_jwt::verify`, the `auth::shared::is_safe_refresh_url` check that `gateway::auth::parse_discovery` applies (that function is private and returns only the device and token endpoints, so `logout.rs` reads `revocation_endpoint` itself), `error_body.rs` response rebuild, and `SseParser` named in the Architecture Decision

## Verification

### Automated Tests

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features --workspace` all pass with `RUSTFLAGS=-D warnings`

### Manual Testing

- [ ] With a real ChatGPT account: `shunt login chatgpt-plan --name a` (listener and paste paths), a Claude Code turn with tools through a `chatgpt-plan` route, `shunt logout chatgpt-plan --name a`; confirm the session-affinity headers and a `web_search` tool are accepted (STOP condition otherwise)

### Acceptance Criteria Check

- [ ] AC-001..028 each covered by the task that cites it

## Review Focus

- A pasted callback URL with surrounding whitespace or quotes → parsed, not rejected (T006)
- Port 1455 already bound by another process → paste-only with a notice, not a crash (T006)
- A hand-edited, corrupt, or signed-out account file → terminal needs-sign-in, never a panic (T007)
- `SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR` with spaces or a symlink → host id and accounts stay together (T004)
- An upstream stream that hits 64 KiB or EOF before any non-lifecycle frame → replayed intact (T009)

## Decision Log

- 2026-10-30: Reuse the `OpenAi` dialect rather than a new flavor (user choice; ADR-0007).
- 2026-10-30: Needs-sign-in accounts keep the existing cooldown + flag instead of hard exclusion (user choice; spec AC-012 reworded).

## Progress

- [x] (2026-10-30 KST) T001 Register the chatgpt-plan auth mode, preset, and config validation
  Evidence: `cargo +1.98.1 test --features bench --workspace` (RUSTFLAGS=-D warnings) → exit 0, lib 3563 passed / 0 failed; clippy and fmt --check clean
- [x] (2026-10-30 KST) T005 Verify the SIWC ID token
  Evidence: `cargo +1.98.1 test --features bench --lib -- chatgpt_plan::id_token` (RUSTFLAGS=-D warnings; --all-features unbuildable here) → 11 passed / 0 failed (valid token + array aud + email; iat +5 s accepted; exp -20 s within skew accepted; wrong nonce, missing nonce, wrong aud, exp -120 s, unknown kid rejected; unreachable JWKS → KeysUnavailable); full lib 3592 passed / 0 failed; clippy --all-targets --features bench and fmt --check clean; inbound_jwt untouched
- [x] (2026-10-30 KST) T002 Map SIWC error codes to Anthropic errors
  Evidence: `cargo +1.98.1 test --features bench` (RUSTFLAGS=-D warnings; --all-features unbuildable here) → exit 0, 4423 passed / 0 failed (lib 3569, responses_translate 123) incl. 3 siwc_errors unit, 7 responses_translate SIWC/detail, 3 error.rs wiremock tests; clippy --all-targets --features bench and fmt --check clean
- [x] (2026-10-30 KST) T003 Surface the callback's client_id and scope from the loopback listener
  Evidence: `cargo +1.98.1 test --features bench --lib -- auth::` (RUSTFLAGS=-D warnings; --all-features unbuildable here) → exit 0, 569 passed / 0 failed incl. 3 new auth::callback tests (client_id+scope surfaced, extra params ignored, empty→None, oversized rejected with 400 and wait continues, at-cap accepted) and all claude/antigravity login tests; clippy --all-targets --features bench and fmt --check clean
- [x] (2026-10-30 KST) T004 Store chatgpt-plan accounts and the installation host id
  Evidence: `cargo +1.98.1 test --features bench --lib -- chatgpt_plan` (RUSTFLAGS=-D warnings; --all-features unbuildable here) → 17 passed / 0 failed incl. 8 new store tests (0600/0700, same-email two records, signed-out {version, client_id} round-trip, 8-thread host id created once as urn:uuid v4, .host-id and .json.lock never scanned, spaces + symlinked dir, corrupt host id replaced, Debug redaction); full lib 3581 passed / 0 failed; clippy --all-targets --features bench and fmt --check clean
- [x] (2026-10-30 KST) T005 Verify the SIWC ID token
  Evidence: `cargo +1.98.1 test --features bench --lib -- chatgpt_plan::id_token` (RUSTFLAGS=-D warnings; --all-features unbuildable here) → 11 passed / 0 failed (valid token + array aud + email; iat +5 s accepted; exp -20 s within skew accepted; wrong nonce, missing nonce, wrong aud, exp -120 s, unknown kid rejected; unreachable JWKS → KeysUnavailable); full lib 3592 passed / 0 failed; clippy --all-targets --features bench and fmt --check clean; inbound_jwt untouched
