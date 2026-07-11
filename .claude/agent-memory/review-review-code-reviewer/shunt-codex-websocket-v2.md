---
name: shunt-codex-websocket-v2
description: shunt repo (pleaseai/shunt) — Codex Responses WebSocket v2 transport (issue #32, PR #39) architecture and the unresolved silent-phantom-success bug found in review.
metadata:
  type: project
---

`src/adapters/codex_ws.rs` (transport: handshake, per-session connection pool,
reader task) + `src/adapters/codex_continuation.rs` (pure `decide()` prefix-match
decision layer) + `src/adapters/responses.rs` (`forward_websocket`/`open_ws_turn`
wiring) implement an opt-in (`provider.websocket = true`, ChatGPT/Codex backend
only, gated by `Config::codex_websocket_enabled`) websocket transport that reuses
a pooled connection and replays `previous_response_id` to cut per-turn upload.

**Unresolved bug found in review (2026-07-12, PR #39 diff):** in
`codex_ws.rs::stream_events()`, the `Some(Ok(Message::Close(_))) | None` arm
returns `Outcome::Failed` *without* sending anything into the `tx` channel — the
comment says "end the channel quietly" on the theory the machine's `finish()`
will produce a clean end. But `AnthropicSseMachine::finish()`
(`src/model/responses.rs`) unconditionally emits `message_delta` +
`message_stop` even when `started` is false (no `message_start` was ever sent),
and `final_json()` returns a fully-formed but empty/default message. Net effect:
if the websocket connects, the frame sends, but the socket closes before any
event is ever forwarded (idle backend hiccup, immediate close, etc.), the
non-streaming JSON path (`json_events_response`) returns **200 OK with an empty
fake-success body**, and the streaming path emits a protocol-invalid SSE stream
(message_delta/message_stop with no preceding message_start) — both silently
mask an upstream failure as a successful empty turn, contradicting the
`forward()` docstring's own claim that "a mid-stream failure is surfaced as an
Anthropic error event." No test in `tests/codex_websocket_fallback.rs` covers
this path (it only covers handshake-failure-before-connect, which correctly
falls back to HTTP). Worth re-checking whether a fix landed before reviewing
follow-up PRs in this area.

Secondary, lower-confidence finding: if `Turn::stream()`'s initial
`guard.send(Message::Text(frame))` fails (a stale pooled connection that passed
the liveness Ping — send-only probes can false-positive on a half-open TCP
connection — but fails on the real send), the reader task never spawns, so
`invalidate_pool_key` never runs; the dead entry stays in `POOL` and is retried
(and falls back to HTTP) on every subsequent turn for that session until the
30-minute `POOL_IDLE_TTL` expires.

Also: `src/adapters/codex_ws.rs` is 1011 lines and `codex_continuation.rs` is
508 — AGENTS.md says Rust files "preferably under 500 lines"; `responses.rs`
was already 646 lines pre-PR (pre-existing) and grew to 945.
