---
name: shunt-perf-hot-path-coverage
description: PR #182 hot-path coverage — auth single-flight and quota behavior are already strong; WS idle-timer and BoundaryTracker len-0/3 regressions remain unguarded.
metadata:
  type: project
---

PR #182 removes the outer Codex account-pool refresh lock, moves quota assessment
outside the health lock, reuses the Codex WebSocket idle timer, and bulk-updates the
SSE boundary tail.

Coverage conclusions:
- The lock-removal safety invariant is directly covered in
  `src/auth/codex/auth.rs::concurrent_get_valid_single_flights_refresh`: two stores
  resolving the same expired credential produce exactly one token request, both
  receive the refreshed access token, and the rotated refresh token is persisted.
  This test already exists on `origin/main`.
- The quota clone-then-assess split is well guarded by the broad `src/accounts.rs`
  selection/snapshot suite (thresholds, stale expiry, Fable bucket choice,
  rejections, headroom ordering, hard backstop, cooldown-only selection).
- `BoundaryTracker::push` tests exercise long chunks, a 1-byte split, a 2-byte
  split, and old-tail retention, but not the new empty-chunk branch or retained=3
  arithmetic. A table case for `\r` + `\n\r\n` and an empty push would guard SSE
  boundary equivalence.
- No test reaches the rewritten 300-second Codex WS turn idle-timer branch or
  proves the reused timer resets after each frame. A paused-time test should cover
  reset-after-activity, timeout-after-silence, and frame-wins-at-deadline behavior.

Reusable review pattern: when a per-item loop becomes a fixed-tail bulk copy, test
each sub-tail length (0..tail length) and a chunk at/above tail length; when
`timeout(future)` becomes a pinned reusable `Sleep` + `select!`, test both reset
semantics and deadline tie-breaking.
