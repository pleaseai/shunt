---
name: pr125-codex-passthrough-endpoint
description: PR #125 inbound Codex/Responses passthrough endpoint (src/codex_endpoint.rs, src/adapters/responses/mod.rs forward_codex_*) — top-level handler discards AdapterError.message and logs nothing on most failure paths, unlike the sibling proxy::post.
metadata:
  type: project
---

PR #125 added `[server.codex_endpoint]`: an inbound raw OpenAI-Responses passthrough (`src/codex_endpoint.rs`, plus `forward_codex_inbound`/`forward_codex_passthrough`/`forward_codex_passthrough_single`/`passthrough_send`/`relay_passthrough` in `src/adapters/responses/mod.rs`, `authenticate_bearer`/`bearer_token` in `src/auth/inbound.rs`). Reviewed for silent failures; findings:

1. **No server-side logging on most error exits** (highest-confidence finding, ~92): `codex_endpoint.rs::forward()` returns `Result<_, axum::response::Response>` (line 99) instead of a message-carrying error type. Its final line `result.map_err(|error| *error.response)` (line 172) discards `AdapterError.message` entirely, and the handler's catch (`post()` line 86: `Err(response) => response`) does not log at all. Compare `src/proxy.rs`'s `forward()`, which returns a `ForwardError { message, response }` specifically so `post()` can `tracing::warn!(error = %error.message, ...)` before returning — that pattern is NOT reused here. Concretely: `to_bytes` failures (line 138-140) and ALL of `forward_codex_inbound`'s top-level errors (unknown provider, `scan_accounts` failures) go completely unlogged. Worst case: `forward_codex_passthrough_single` (`src/adapters/responses/mod.rs` lines 688-698) — the single-account/no-pool fallback, likely the common default setup — has zero `tracing` calls anywhere in its `resolve_credential`/`passthrough_send` failure paths. Only `crate::metrics::record_proxied_request` fires (a bare status-code counter, no message). Multi-account pool path (`forward_codex_passthrough`) is NOT affected — it has per-attempt `tracing::warn!` calls in its loop.

2. **Error-shape mismatch** (~55): gateway-owned errors on this raw-passthrough endpoint (config missing, auth failure, body-too-large, `own_error` exhaustion) use `ShuntError`/`UpstreamError`, which hardcode an *Anthropic*-Messages-shaped JSON envelope (`{"type":"error","error":{"type":"api_error",...}}`). A Codex CLI client expects OpenAI-Responses-shaped errors. Genuine upstream errors ARE relayed byte-for-byte correctly via `relay_passthrough` — only shunt's own synthesized errors have the wrong shape.

3. Confirms the pre-existing `own_error()` "generic-message-only" bug from [[shunt-codex-ws-error-handling]] (PR #39) is inherited by ~6 new call sites in the codex-passthrough code, further reducing the value of `AdapterError.message` even if finding 1 were fixed.

**How to apply**: If re-reviewing this file or a follow-up PR, check whether `codex_endpoint.rs::forward()` was changed to preserve/log `AdapterError.message` (grep for `ForwardError`-style wrapping or a `tracing::warn!` in the `Err` arm of `post()`), and whether `own_error`'s generic-message issue was ever fixed globally.
