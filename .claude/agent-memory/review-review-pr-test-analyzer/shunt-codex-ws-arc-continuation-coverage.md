---
name: shunt-codex-ws-arc-continuation-coverage
description: PR #270 Arc continuation ownership refactor is compiler-enforced and existing transport tests cover observable behavior and overflow isolation.
metadata:
  type: project
---

PR #270 changes the Codex WebSocket connection’s stored continuation from an owned clone to `Arc<StoredContinuation>` without creating a behavioral coverage gap. `StoredContinuation` has no interior mutability; callers receive only shared access, while `Arc::make_mut` would clone rather than mutate the connection-held value. The per-connection turn lock prevents simultaneous turns, and the existing dedicated-overflow test verifies that a concurrent overflow turn receives no continuation and does not replace the pooled connection. The existing real-completion test verifies the stored continuation’s observable contents. No tests were changed or removed.

**Why:** The shared-immutable invariant is enforced by Rust’s type system rather than a runtime contract, so a test for pointer identity, strong counts, or inability to mutate would test implementation details rather than behavior.

**How to apply:** For future ownership-only changes in this path, require new tests only if interior mutability, lock scope, replacement timing, or overflow routing changes; otherwise rely on the existing completion/reuse and overflow-isolation tests.
