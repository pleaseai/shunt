---
name: shunt-gateway-cli-login-coverage
description: shunt gateway login/token/logout/claude CLI (src/auth/gateway/*) test-coverage gap analysis
metadata:
  type: project
---

New `shunt gateway login|token|logout|claude` CLI (src/auth/gateway/{mod,login,store,auth,launch}.rs +
tests/gateway_cli.rs). Reviewed against origin/main diff of ~2760 lines.

**What's exceptionally well covered (do not re-flag on future passes):**
- The load-bearing "HTTP 400 carries authorization_pending, parse body before status" invariant
  has a real regression test (`poll_continues_through_a_400_authorization_pending` in login.rs) —
  a status-first implementation genuinely fails it.
- Refresh-token rotation asserts the NEW token lands on disk, not just that refresh succeeded
  (`refresh_persists_the_rotated_refresh_token` in auth.rs).
- launch.rs covers shell-quoting of spaced paths, single-quote escaping, hostile gateway URLs
  (quotes/backslashes) via serde_json (not format!), and explicit assertions that
  CLAUDE_CODE_USE_GATEWAY / ANTHROPIC_AUTH_TOKEN are absent from the settings doc.
- Error paths: expired_token, access_denied, invalid_grant, deadline-exceeded (poll_stops_at_the_device_code_deadline),
  transport-failure cap (3 consecutive), missing session (multiple), `claude` not on PATH — all tested with
  non-vacuous assertions (specific message content, not just "is_err").
- `slow_down` interval-widening test deliberately uses real time (not `start_paused`) with a code comment
  explaining why paused-clock would make it vacuous — team already knows this trap.

**Real gap found: the flock concurrency guard is untested.**
`store::lock_session`/`lock_blocking` (store.rs:170-208) is the entire reason the design exists
(doc comment: prevents two `apiKeyHelper` racers from both replaying a single-use refresh token
and revoking the whole family) — but only `lock_path_is_a_sibling_of_the_session_file` (pure path
computation) exists. No test ever calls `lock_session` and checks it actually acquires/blocks/releases,
and no test exercises `resolve_token_at`'s double-check-under-lock path (auth.rs:238-243, "waiter
re-reads and usually finds the token the winner already persisted"). This is the single biggest
finding in this PR — flag it plainly as manually-verified-only, not automated, in any review of this
diff or its follow-ups.

**Minor gap:** invalid_grant refresh-failure test only asserts stdout content, not that the file is
byte-unchanged beyond refresh_token field (already covered adequately though — access_token check
would be a nice-to-have, not worth flagging again).
