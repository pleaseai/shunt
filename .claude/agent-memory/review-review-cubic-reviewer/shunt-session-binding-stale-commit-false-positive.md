---
name: shunt-session-binding-stale-commit-false-positive
description: cubic flags session_binding.rs commit() None-arm as "stale commit recreates evicted binding" — verified as by-design; do not re-raise without a live newer binding case.
metadata:
  type: project
---

cubic (PR #884, HEAD ba00ed94) reported P1 "Stale commits can recreate an evicted binding" at `src/accounts/session_binding.rs:212`, the `None => insert_new` arm of `SessionBindings::commit`.

**Why it is not a bug:** the `Some(current) if observed_generation < current => Stale` arm already protects any *live* newer binding. The `None` arm only runs when no live binding exists (absent, expired, removed, or evicted), so there is nothing newer to overwrite. `insert_new` assigns a fresh table-wide generation, so the recreated binding outranks every earlier ticket. `session_binding/tests.rs` (`a_ticket_from_before_removal_cannot_overwrite_a_replaced_binding`) covers the removal, expiry, and eviction cases and asserts `Stale` for a replaced binding.

**How to apply:** if cubic raises this again, check whether a *live* newer binding exists at commit time. Only a live newer binding that gets overwritten is a real regression (spec NFR-1: `.please/docs/tracks/active/session-account-binding-20261031/spec.md`). Also note cubic exited 1 while printing valid issues JSON (see [[cubic-nonzero-exit-with-issues]]).
