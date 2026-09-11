---
name: shunt-multi-account-env-serialization
description: PR #532 tests/multi_account.rs ENV_LOCK-everywhere fix for a process-global env-var flake — how to verify a mutex-around-env-var fix is actually complete, not just narrowed
metadata:
  type: project
---

PR #532 widened `REFRESH_ENV_LOCK` (3 refresh tests) to `ENV_LOCK` held by all 29 tests in
`tests/multi_account.rs`, because `setenv` can reallocate the process's global `environ` array —
any unsynchronized `set_var` in a sibling test can make a concurrent `getenv` in another test
observe a var as transiently missing (issue #507). This is the second recurrence after #406
([[new-credential-kind-slot-audit]] area codebase, same "process env is shared" family of bug).

**How I verified the fix is actually complete (not just a narrower window):** the standing worry
with any "wrap the test body in a mutex" fix is a background task spawned inside the guarded
region that outlives the guard and keeps touching the global env after the next test's guard
region begins writing. Checked three things:

1. All `#[tokio::test]` in the file use the default **current-thread** runtime (grepped for
   `flavor`/`multi_thread` — none). On a current-thread runtime the axum server task spawned via
   `tokio::spawn` inside `start_gateway_with(_state)` cannot literally execute concurrently with
   the test body's own (synchronous) Drop sequence — only one task runs at a time on that thread.
   `TestGateway::drop` calls `task.abort()`; tokio guarantees an aborted task never resumes
   executing app code past its current suspension point (it just gets dropped on next poll), so
   there's no window for it to sneak in an extra `env::var` read after abort is requested.
2. Local-variable drop order: `_env` (the guard) is always declared *before* `gateway`
   (`TestGateway`) in every test body, so Rust's reverse-declaration-order drop means `gateway`
   (which aborts the task) drops **before** `_env` releases the lock — the safe order.
3. `axum::serve` spawns one task **per accepted connection**, separate from the outer accept-loop
   task the `JoinHandle` tracks — `task.abort()` does not reach those. This would be a real gap if
   any test left an HTTP request in flight when it returned. Checked every test with an inner
   `tokio::spawn` of a client request (only `storm_control_spills_concurrent_request_to_next_account`
   has one) and confirmed it `.await`s that JoinHandle before the test function returns. So no
   live per-connection task exists at guard-drop time in the current suite — the fix is complete
   for what's here today, though it stays contingent on every future test continuing to fully
   await any request it spawns rather than fire-and-forget one.

Conclusion: reported as high-confidence "fix is behaviorally complete for the current suite",
with a lower-confidence forward-looking note (not a blocking finding — no live bug, just a design
fragility) that nothing stops a future test from firing off an unawaited request against the
shared gateway.

Also: 1:1 check (`grep -c '#\[tokio::test\]'` vs `grep -c 'ENV_LOCK.lock'` — both 29) is a fast way
to confirm full coverage of a "hold this lock in every test" fix without reading all 29 bodies.
