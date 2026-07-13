---
name: shunt-admin-surface-coverage
description: shunt PR #85 (M9 admin web surface, src/admin/*) tests/admin_surface.rs gap analysis — happy paths solid, session/CSRF success path and several security-relevant negative branches untested.
metadata:
  type: project
---

Reviewed `tests/admin_surface.rs` (5 integration tests + 1 startup-validation test)
against `src/admin/{mod,session,html}.rs` and the new `claude_store.rs` metadata
functions. Strong: default-off gate, header-token 401 rejection, and the full
provisioning happy path (add → complete → list → pool → delete) with explicit
no-token-leak assertions on both the JSON response and the store file contents.

Real gaps found (see PR #85 review output for full confidence/severity):
1. No test proves a session-cookie + *correct* CSRF token actually succeeds — the
   only CSRF test (`cookie_session_mutations_require_a_csrf_token`) checks the
   missing-token 403 path only. The entire browser happy path (login → dashboard
   → add account via cookie+CSRF) is unverified end-to-end.
2. `logout` (src/admin/mod.rs) has zero test coverage — no test confirms the old
   session cookie stops working after logout.
3. `complete_account`'s OAuth state-mismatch / malformed-`<code>#<state>` rejection
   is untested; every completion test pastes a well-formed, matching code.
4. Token-exchange-failure and missing-account-UUID error branches (deliberately
   generic `bad_gateway`, per an inline comment about not echoing upstream detail)
   are untested.
5. Forged/unknown session cookie rejection is only unit-tested against
   `SessionStore::csrf_for` directly, never via an actual HTTP request with a
   garbage cookie value.
6. `escape_html` (src/admin/html.rs) has literally zero tests — no `#[cfg(test)]`
   block in that file at all. Low exploitability today (only static strings /
   random b64 IDs flow through it) but a real gap in the one XSS-relevant helper.

Recurring pattern to check on future PRs to this codebase: a "kind"-classifying
function that reads a store file and branches on some field's presence (e.g.
`read_account_meta`'s `AccountKind::SetupToken` vs `AccountKind::Imported` based
on `refreshToken` presence, in `src/auth/claude_store.rs`) tends to get only ONE
of its branches exercised by the integration tests, because the integration flow
always produces one specific account shape (here: setup-token accounts only,
never an imported/refresh-token account). This is the same shape of gap flagged
in PR #70's `account_is_static_store_token()` — see
[[shunt-multi-account-failover-coverage]]. When reviewing a store/kind-detection
function in this repo, explicitly check whether the test suite ever produces an
account of the *other* kind.

General note: this codebase's test style (per `tests/AGENTS.md`) is strict
behavior-first HTTP integration tests via a real axum server + wiremock, matching
`tests/multi_account.rs`'s style from PR #70. `tests/admin_surface.rs` follows
this well for the happy path but, unlike `multi_account.rs`'s thorough negative-
case coverage of the failover matrix, under-covers `complete_account`'s several
`bad_request`/`bad_gateway` branches and the session/CSRF success path.
