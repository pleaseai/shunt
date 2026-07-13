---
name: shunt-multi-account-failover-coverage
description: shunt PR #70 (Anthropic multi-account/claude_oauth) tests/multi_account.rs + accounts.rs + claude_store.rs — behavioral gaps found in the failover matrix
metadata:
  type: project
---

Reviewed `tests/multi_account.rs` (10 wiremock integration tests) against
`forward_claude_oauth()` in `src/adapters/anthropic.rs`. The 9-case failover matrix
(quota-429 Rotate, plain-429 PauseSame, 401 static, 401 refresh→success, 401 refresh→still-401,
5xx Rotate, resolve-failure, all-fail 502, exhausted-pool relay) is all covered — solid,
strong assertions (status, `x-shunt-account` header, verbatim body, `upstream.verify()` call
counts). Env-var races: `SHUNT_CLAUDE_ACCOUNTS_DIR`/`SHUNT_CLAUDE_TOKEN_URL` are process-global
but only the 3 refresh tests touch them, all three correctly take `REFRESH_ENV_LOCK` before
setting/reading and hold it through cleanup — no race found there.

Real gaps found (see PR #70 review output for full confidence/severity):
1. `FailoverAction::PauseSame`'s retry-succeeds path (line ~256 relay after sleep) is never
   exercised — every PauseSame test keeps the retry at 429 too. The success branch
   (`mark_healthy` + relay 200) is untested.
2. `account_is_static_store_token()` (anthropic.rs:416) — the JSON-file-based
   `shuntCredentialKind == "setup_token"` detection — has zero test coverage, unit or
   integration. All "static/non-refreshable" integration tests use `token_env` accounts, which
   short-circuit before ever calling this function.
3. `claude_store.rs`'s two `anyhow::bail!` validation paths (`import_credentials` rejecting a
   credentials file missing accessToken/refreshToken, `store_setup_token` rejecting
   empty/whitespace tokens) are untested — only happy paths are covered. This is exactly the
   "negative test case for validation logic accepting external input" pattern to watch for in
   this repo's auth/*_store.rs files.
4. No test exercises concurrent requests to the *same* account to verify `refresh_lock` actually
   serializes overlapping `force_refresh()` calls (the entire reason the lock exists — Claude
   refresh tokens are typically single-use/rotating).
5. `rewrite_account_uuid()` is unit-tested in isolation but no integration test asserts the
   outgoing upstream body actually contains the selected account's `account_uuid` — the
   `BearerToken` wiremock matcher only checks the Authorization header, never body content.

General note: this codebase's test style (per `tests/AGENTS.md`) is strict behavior-first
(assert status/headers/body verbatim, use `.expect(n)` + `upstream.verify()`), which this PR's
tests follow well. The pattern worth checking on future PRs to this file: tests named
"...cools_down_and_rotates" should include a *second* request (via `session_id_for_account`) to
prove the cooldown persisted, not just that the first request rotated — `unresolvable_account_cools_down_and_rotates`
breaks this pattern (only issues one request) while its siblings (quota_429, unauthorized_static,
server_error) follow it correctly.
