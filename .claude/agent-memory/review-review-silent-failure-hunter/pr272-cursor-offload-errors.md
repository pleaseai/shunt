---
name: pr272-cursor-offload-errors
description: PR #272 fix b73f31a fully resolves Cursor framing failover and observability; request-prep and gzip offload failures remain propagated.
metadata:
  type: project
---

PR #272 (issue #259) moves Cursor request framing and large base64 image decoding to bounded Tokio blocking-pool classes. Fix commit `b73f31a` fully resolves the first-pass framing failure: framing now happens in `cursor::forward`, maps through `request_prep_error`/`own_error` with `failure: None`, and therefore returns immediately through `failover.rs` rather than advancing the route chain. The client receives an Anthropic-shaped HTTP 500 containing `cursor request framing: {error}`; `own_error` also puts that detail into `AdapterError.message`, so `proxy::post` logs the concrete cause. The image-offload error path has the same client and operator visibility.

`spawn_bounded` returns semaphore-acquisition and blocking-task `JoinError`s via `io::Error::other`; neither is swallowed. The process-static semaphores have no production close call, so acquisition failure is presently unreachable, but it still propagates if that changes. A blocking-task panic is verified to surface as a join error, and request-preparation callers convert it to the detailed local 500. Gzip offload still propagates acquisition/join/decode errors into the Cursor stream error path. Invalid base64 images remain deliberately dropped, preserving the pre-PR request semantics.

**Why:** The original bug made a deterministic local framing panic look like an upstream outage and erased its cause. The fix now preserves both routing semantics and observability end to end.

**How to apply:** Keep deterministic adapter preparation failures at `failure: None`; include the concrete cause in both `AdapterError.message` (operator log path) and `ShuntError` (client body). Do not route local CPU-task errors through `map_client_error`, which still classifies pre-header transport failures as failover-eligible.
