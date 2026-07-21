---
name: shunt ordered-upstreams-failover-coverage
description: PR #224 ordered cross-provider failover coverage map; generic post-2xx stop is now guarded, adapter-specific read failures and pooled WS gate remain
metadata:
  type: project
---

PR #224's original `tests/failover.rs` covers chain order, advance statuses, connect failures, immediate 400 stop, best-failure selection, synthesized exhaustion, Responses raw-status propagation, whole-chain inbound gating/credential stripping, count_tokens pinning, and gateway metadata headers. Follow-up commit `6d7bc66` added a strong two-upstream regression for a Responses `200` followed by `response.failed`: it asserts 502/message, first upstream exactly once, and second upstream zero requests. Existing Codex WebSocket integration tests also genuinely mutation-guard the pre-first-event WS→HTTP fallback and single-account post-first-event no-fallback boundary.

The remaining fix-delta gaps are adapter-specific: Anthropic aliased non-streaming 2xx body-read failure (`post_header_error`) and Responses non-streaming 2xx body-read failure have no truncated-wire/no-replay tests. The new pooled Codex WS fallback gate (`Err(error) if failure.is_some()`) lacks a pooled post-first-event test; mutating it back to unconditional fallback still passes current pooled and single-account tests. Cursor's new stream-error classification is unit-asserted (`failure.is_none`, mapped status) but not itself driven through a two-upstream chain. Config `AccountConfig::deny_unknown_fields`, empty `accounts=[]`, scoped-health cleanup, refreshable-alias ordering, and Anthropic admission release tests are substantive.

**Why:** A generic outer-loop contract test does not execute every adapter's classification point; regressions there can replay a non-idempotent LLM turn. Pooled WS fallback is independently wired from the single-account arm.

**How to apply:** Require a real 200-then-truncated-body test for Anthropic/Responses classification points, and a pooled WS post-first-event test that asserts zero HTTP fallback/account rotation. Keep the existing second-upstream zero-hit assertion pattern.