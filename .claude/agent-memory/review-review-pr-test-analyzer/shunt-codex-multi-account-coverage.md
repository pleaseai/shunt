---
name: shunt-codex-multi-account-coverage
description: shunt PR #114 (Codex/ChatGPT multi-account pooling) tests/codex_multi_account.rs + adapters/responses/mod.rs — gap analysis, mirrors PR #70's Anthropic review
metadata:
  type: project
---

Reviewed `tests/codex_multi_account.rs` (9 wiremock integration tests) against
`forward_chatgpt_oauth()` in `src/adapters/responses/mod.rs`. Status-code failover matrix
(401 static/refresh-success/refresh-still-401, 429 always-rotate, 5xx rotate, resolve-failure,
all-fail 502, exhausted-pool translated envelope) is solidly covered with strong assertions
(status + `x-shunt-account` header + body content + `upstream.verify()` call counts) — same
strict style as [[shunt-multi-account-failover-coverage]] (PR #70).

Real gaps found (see PR #114 review output for full confidence/severity):
1. **WS `account_pool_key` isolation has zero test coverage.** `test_config()` never sets
   `provider.websocket = true`, so `ws_enabled` is always false in every one of the 9 tests —
   the entire `if ws_enabled { forward_websocket(...) }` branch (mod.rs ~226-299), including the
   account-name-prefixed pool key the PR's own doc comment calls "the key correctness
   requirement of the WS integration," is never exercised. `tests/codex_websocket_fallback.rs`
   tests ws-fallback but has no `AccountConfig`/pool at all (single-account, pre-pool path).
2. **`FailoverAction::Relay` with a non-success status (e.g. 400) is untested for the pool
   path.** No test proves a 4xx client error from account-a returns immediately (mod.rs
   ~327-346) without rotating to account-b — a regression here would burn through the whole
   pool on a single bad client request.
3. **`account.credentials` explicit path override is untested**, both in
   `resolve_chatgpt_account` (auth/mod.rs:155, unit tests at 358/410 only cover `token_env` and
   default-store-path name-only accounts) and in the `RefreshRetry` arm's `credentials_path`
   construction (mod.rs:386-391).
4. **`select_order` session-stickiness wiring is asserted but not actually proven** for the
   codex pool: every `session_id_for_account(0, 2)` check in this suite happens to target an
   account already cooled down from an earlier request in the same test, so the assertion would
   still pass even if `session_id.as_deref()` (mod.rs:218) were silently replaced with `None`.
   The underlying `stable_session_index` algorithm itself is pre-existing/shared and presumably
   unit-tested elsewhere, so this is about the call-site wiring, not the algorithm.
5. **`refresh_lock` concurrency is untested** — no test issues two concurrent requests to the
   same refreshing account to prove serialization. Same accepted gap as PR #70 item 4; not
   newly introduced by this PR but still applies to the new `forward_chatgpt_oauth` call site.

Minor (low confidence/severity, still reported per stage-5 instructions):
6. `import_auth`'s `has_access`/`has_refresh` checks (store.rs:126-134) reject a *missing*
   refresh_token key but an empty-string token value is untested.
7. `auth/codex/login.rs`'s `run()`/`import_current_login()` glue has no direct test — thin
   wrapper around already-tested `import_auth` + `default_codex_auth_path`, low value.

Pattern confirmed from PR #70: `test_config(upstream_base_url, first, second)` helper pattern +
`REFRESH_ENV_LOCK` for process-global env var tests carries over cleanly to the Codex suite,
same discipline (lock held through cleanup, no races found).
