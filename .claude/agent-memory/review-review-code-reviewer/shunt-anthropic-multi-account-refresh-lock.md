---
name: shunt-anthropic-multi-account-refresh-lock
description: shunt PR #70 (Anthropic multi-account load balancing) — refresh_lock scope and RefreshRetry branch findings in src/adapters/anthropic.rs forward_claude_oauth()
metadata:
  type: project
---

In `/Users/lms/orca/workspaces/shunt/claude-multi-account/src/adapters/anthropic.rs`,
`forward_claude_oauth()` acquires `state.accounts.refresh_lock(&route.provider, &account.name)`
at the top of each per-account loop iteration (`let _guard = lock.lock().await;`) and holds it
for the entire per-account attempt — including the `FailoverAction::PauseSame` branch's
`tokio::time::sleep(delay)` (up to 300s) and its retry POST. The lock's doc comment in
`src/accounts.rs` (`refresh_lock()`) says it should "serialize token refreshes for one account",
a narrower scope than actual usage. Net effect: two concurrent requests routed to the same
account (round-robin or session-sticky hash collision) fully serialize, with the second
blocking up to ~300s behind an unrelated plain-429 retry-after sleep on the first.

**Why:** `src/auth/claude_auth.rs`'s `ClaudeAuthStore` has no internal locking of its own — it
relies entirely on the caller's external lock to prevent concurrent-refresh races. That's real
justification for *some* external lock, but doesn't require it to span the whole HTTP
round-trip + sleep, only the refresh operation itself.

**How to apply:** If asked to review future changes to this file, check whether the guard scope
has been narrowed (e.g., moved inside just the `RefreshRetry` branch's `force_refresh()` call)
or whether this was deliberately accepted (docs/m8-anthropic-multi-account.md does not mention
this trade-off either way as of this PR). Flag at ~Important/60-70 confidence if unchanged and
newly touched.

Separately, in the same function's `FailoverAction::RefreshRetry` branch (around line 250-339):
after a successful `force_refresh()` + retry POST, the code only special-cases
`retry.status() == StatusCode::UNAUTHORIZED` (rotate) and `.is_success()` (mark_healthy) — any
other status (e.g. a 5xx, or a 429 with a quota-rejected header on the *retry* response) falls
through to an unconditional `return relay_response(&state, retry, Some(&account.name))`. This
terminates the failover loop early and relays that response to the client even when other
accounts remain untried in `order`, which arguably contradicts the "never fails closed" design
goal stated in docs/m8-anthropic-multi-account.md (that goal is proven true for `select_order()`
itself, but this early-return can defeat it in practice for this one sub-case). No unit test or
integration test in `tests/multi_account.rs` exercises a non-401/non-2xx retry response in this
branch — it's untested behavior. Confidence ~50-55 (spec only explicitly defines the 401
sub-case for this branch, so this may be accepted-as-is scope-limiting rather than a bug).
