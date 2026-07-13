---
name: shunt-responses-adapter-stream-json-doc-generalization
description: src/adapters/responses/mod.rs doc comments tend to generalize "Anthropic error event" / "streamed through" language across both the SSE streaming path (stream_events_response) and the non-streaming JSON path (json_events_response), but the two paths render errors differently
metadata:
  type: project
---

In `src/adapters/responses/mod.rs` (Codex WebSocket v2 transport, issue #32/#46), doc comments describing error handling for websocket turns are written from the streaming (SSE) path's point of view and use language ("surfaced as an Anthropic error event", "streamed through") that reads as if it applies uniformly to both client modes. It does not:

- `stream_events_response` really does emit an SSE `event: error` line (via `ws_error_sse`/`map_error_value`) for a mid-stream transport error.
- `json_events_response` instead returns a distinct 502 JSON body via `ShuntError::bad_gateway(...).into_response()` for the same transport-error case — accurate but not literally an "event".
- More importantly: a backend-sent `Ok`-wrapped error/`response.failed` event (rate limit, content-policy refusal) is handled asymmetrically — `stream_events_response`'s `AnthropicSseMachine::apply` renders it as `event: error` and is used, but `json_events_response` does `let _ = machine.apply(event);` — the rendered error text is silently discarded, and `final_json()` (since `self.stopped` is already true) skips `finish()` and returns whatever partial content had accumulated with a normal 200-shaped success body. This looks like a real, pre-existing (not this-PR-introduced) silent-failure gap in the JSON/non-streaming path, worth flagging to the silent-failure-hunter or performance/correctness reviewer aspect, not just comments.

**Why:** Found while auditing PR #111 (issue #46, socket-drop-before-first-token fallback) doc comments in this file — the new/reworded comments on `forward()`, `open_ws_turn`, `commit_or_fallback`, `stream_events_response`, `json_events_response` were otherwise scrupulously accurate (verified against `codex_ws.rs`'s `Turn::stream`/`run_connection`/`run_turn` channel-queueing implementation and `model/responses.rs`'s `AnthropicSseMachine`), but this stream-vs-JSON asymmetry wasn't called out anywhere, so a comment claiming backend error events are simply "streamed through" understates that the JSON path swallows them.

**How to apply:** Next time this file's comments are reviewed (or `stream_events_response`/`json_events_response` are touched again), check whether any new doc comment implicitly claims parity between the two response modes for error handling, and check whether `json_events_response`'s handling of `Ok`-wrapped terminal error events (not just `Err` transport errors) has been fixed to actually surface the error in the final JSON body instead of discarding it.
