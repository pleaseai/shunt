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

**The flock concurrency guard IS covered (do not re-flag it as a gap):**
`store::lock_session`/`lock_blocking` is the reason the design exists (doc comment: prevents two
`apiKeyHelper` racers from both replaying a single-use refresh token and revoking the whole family),
and it has real tests beyond `lock_path_is_a_sibling_of_the_session_file` (pure path computation):
- In `src/auth/gateway/store.rs`: `a_held_session_lock_blocks_the_next_acquisition_until_it_drops`
  (the waiter is proven not to acquire while the holder lives, and to acquire once it drops),
  `waiting_for_a_stuck_holder_times_out_and_names_the_lock` (timeout path, error names the lock file),
  `logout_waits_for_an_in_flight_refresh_instead_of_racing_it`, and a logout test asserting the lock
  inode survives `remove_session` so later runs still serialize on it (that last one has been renamed
  at least once — match it by what it asserts about the lock file, not by name). The flock-dependent
  ones are `#[cfg(unix)]` and use `multi_thread` + real time, since a paused clock cannot advance
  through a blocking `flock(2)`.
- The double-check-under-lock path — reached through `resolve_token_at` and implemented in
  `resolve_token_bounded` (`src/auth/gateway/auth.rs`, "waiter re-reads and usually finds the token
  the winner already persisted") — is covered by `concurrent_resolvers_perform_exactly_one_refresh`
  in `src/auth/gateway/auth/tests.rs`: two spawned `resolve_token_at` calls against a wiremock
  gateway that honors the refresh exactly once and 401s `invalid_grant` on any replay. It asserts
  both resolvers return the winner's rotated token AND that exactly one POST reached the server, so
  a losing resolver that replayed the spent token fails it.
- `the_lock_timeout_leaves_slack_over_the_worst_case_legitimate_hold` (auth/tests.rs) pins the
  timeout constant against the worst-case legitimate hold.

Cite these by symbol name, not line number: an earlier revision of this note carried line anchors
(`auth.rs:238-243`, `store.rs:170-208`) that were already wrong when written.

**Minor gap:** invalid_grant refresh-failure test only asserts stdout content, not that the file is
byte-unchanged beyond refresh_token field (already covered adequately though — access_token check
would be a nice-to-have, not worth flagging again).
