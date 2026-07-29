---
name: pr272-cursor-offload-errors
description: PR #272 Cursor CPU-offload review found no silent-failure regressions; panic and admission errors surface and invalid-image dropping preserves prior semantics.
metadata:
  type: project
---

PR #272 (issue #259) preserves most Cursor offload error propagation while moving request framing and large base64 image decoding to a shared bounded Tokio blocking pool. `spawn_bounded` returns semaphore-acquisition and blocking-task `JoinError`s to callers; large-image decode maps them to a client-visible Anthropic-shaped 500, and gzip callers still propagate them. Invalid base64 images are still deliberately dropped, while `render_cursor_prompt` continues to include their text placeholder, matching `origin/main` behavior.

The request-framing path has one masking bug: `open_turn` turns `spawn_bounded` failure into non-transient `CursorError::internal`, but `mod.rs::map_client_error` unconditionally labels every such error `AdapterFailure::BeforeHeaders`. The failover driver then discards the detailed response and either silently tries another provider or returns `all upstreams failed`; its warning contains only the generic `AdapterError.message` (`Cursor adapter failed`). This defeats the otherwise-correct non-transient classification.

**Why:** A blocking-task panic can be hidden by provider fallback, and both the client and structured proxy warning lose `cursor request framing: ...`; this makes a deterministic local bug look like an upstream outage. Collapsing acquire and join failures to `io::Error` is not independently silent because their display/source remains preserved, and the process-static semaphore has no production close path.

**How to apply:** Make `map_client_error` mark `BeforeHeaders` only for `RetryableError::is_transient()`, preserve the concrete error string in `AdapterError.message`, and leave deterministic `CursorError::internal` failures at `failure: None`. Continue propagating image/gzip offload errors, and reassess a typed offload error if semaphore closure becomes possible or callers need mode-specific recovery.
