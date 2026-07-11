---
name: shunt-codex-ws-error-handling
description: Known error-handling gaps in shunt's Codex WebSocket v2 transport (src/adapters/codex_ws.rs, codex_continuation.rs, responses.rs) found reviewing PR #39
metadata:
  type: project
---

PR #39 (branch amondnet/32) added the Codex Responses WebSocket v2 transport to shunt (a Rust/axum Anthropic-Messages proxy). Reviewed the diff for silent failures; two real, in-scope bugs found (as of the PR #39 diff, commit at review time):

1. **Silent truncation on unexpected close**: `codex_ws.rs::stream_events()` — the `Some(Ok(Message::Close(_))) | None` arm (around line 497) returns `Outcome::Failed` without sending any `Err` over `tx` and without logging. Downstream (`responses.rs::stream_events_response` / `json_events_response`), a channel that closes with no buffered error is treated identically to a normal graceful stream end (`machine.finish()` / `machine.final_json()` runs as if the turn completed). This violates the PR's own stated design intent that mid-stream failures surface as an Anthropic `error` SSE event. Same root cause would also hide a reader-task panic (tx drops silently either way).

2. **Generic-only fallback log**: `responses.rs::forward()`'s websocket-failure fallback log (`error = %error.message`, around line 103) always prints the literal string `"responses adapter failed"` for any pure-transport `CodexWsError` (timeout, DNS/TLS, frame encode failure, bad URL scheme), because `own_error()` (line ~602) hardcodes `AdapterError.message` to that constant and puts the real diagnostic text only in the (never-returned, since this error is swallowed for fallback) response body. Every websocket transport failure is indistinguishable in logs. Note: `build_upstream_error` (handshake rejected with an HTTP status, e.g. 429) does NOT have this problem — it logs the real status/body already.

3. **Minor**: `Turn::stream()` in codex_ws.rs (line ~262-268) returns `Err` on frame-encode/send failure without calling `invalidate_pool_key` for a *reused* connection, unlike the analogous failure paths in `begin()` and `run_reader()`. Self-heals on the next `begin()` call's liveness ping in most cases, so impact is low (one extra wasted attempt), but it's an inconsistency worth flagging.

**How to apply**: If asked to re-review this file or a follow-up PR on the same module, check whether these were fixed before treating them as still-open. Grep for `Message::Close` handling and `own_error(` call sites to verify.
