---
name: shunt-codex-ws-fallback-coverage
description: PR #111/issue #46 tests/codex_websocket_fallback.rs gap analysis — dual WS+HTTP mock-on-one-port pattern, matrix gaps (json_events_response error arm, continuation-retry peek), and a reusable test-matrix-coverage check.
metadata:
  type: project
---

Issue #46 (shunt): Codex WS adapter falls back to HTTP on a pre-first-token WS
failure, and surfaces a clean `error` SSE on a post-first-token (mid-stream)
failure. Implemented in `src/adapters/responses/mod.rs` via `open_ws_turn` →
`peek_first_event` → `commit_or_fallback` (peeks the first channel item before
committing to the WS response; `Ok` commits+buffers-for-replay, `Err`/`None`
returns `Err` so `forward()` re-drives over HTTP).

Test infra pattern worth reusing: `tests/codex_websocket_fallback.rs`'s
`spawn_dual_upstream` binds ONE TcpListener and peeks the first 4 bytes
(`GET ` vs otherwise) to route a connection to a WS-upgrade handler or a
plain-HTTP handler, so one mock server can play both the WS and HTTP-fallback
role needed to prove a single turn opened a socket, had it die, and re-drove
over HTTP against the same base_url. `ENV_LOCK: Mutex<()>` held for the whole
test body (not just around the mutation) serializes `CODEX_AUTH_FILE` env
access across tests in the file — correct pattern, verify future additions to
this file keep the lock held across all `.await` points, not just acquired
early and dropped before the request completes.

**Gaps found in the 2 new tests (both confirmed real via reading the diff,
not just the tests):**
1. `open_ws_turn`'s **retry branch** (`used_continuation &&
   previous_response_missing` → retry once with full input, THEN
   `commit_or_fallback` again) is genuinely new in this diff — previously that
   branch returned `Ok((None, events))` unconditionally, skipping the peek
   entirely. No test exercises a pooled/reused connection (same
   `x-claude-code-session-id`) whose continuation gets rejected and then the
   *retried* connection also drops before/after its first event. Both new
   tests use no session-id header, so `used_continuation` is always false —
   this whole second commit_or_fallback call site is dead in test coverage.
2. `json_events_response`'s mid-stream-error arm (non-streaming client,
   post-first-event drop → `ShuntError::bad_gateway`, not an SSE `error`
   event) is never invoked by any test: the "before-first-event" test uses
   `stream:false` but resolves via the *fallback* path (never reaches
   `json_events_response`), and the "after-first-event" test uses
   `stream:true` (`stream_events_response`, not `json_events_response`). The
   matrix (stream × before/after) has only 2 of 4 cells covered, and they're
   the "easy" diagonal.

**General lesson: for a 2×2 (or larger) behavior matrix (streaming ×
failure-timing, in this case), check which cells the tests actually hit before
trusting "both scenarios are tested" — it's easy to cover only one diagonal
and call it done.**

Minor/low-severity notes from the same review: `http_hits.load() >= 1`
(rather than `== 1`) in the before-first-event test is looser than needed —
`forward_http` is called at most once per request on this path, so `==1`
would still be correct and would catch an accidental double-POST (relevant
for a metered upstream). `request_is_websocket`'s byte-peek loop
(`tests/codex_websocket_fallback.rs`) doesn't yield between partial reads
(`TcpStream::peek` returns `Ready` for any n>0), a theoretical busy-spin under
segmented delivery — not observed in 15 local repeated runs, loopback
virtually always delivers the whole request line atomically, so this is
low-confidence/low-severity.

See also [[shunt-admin-surface-coverage]] and [[shunt-multi-account-failover-coverage]]
for the org's other review-pr-test-analyzer findings on this repo.
