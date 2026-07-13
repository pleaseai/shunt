---
name: shunt-codex-websocket-v2
description: shunt repo (pleaseai/shunt) — Codex Responses WebSocket v2 transport (issue #32, PR #39) architecture; the PR #39 silent-phantom-success bug was RESOLVED by the time of PR #111 (issue #46, verified 2026-07-14) — see [[shunt-codex-ws-peek-fallback-issue46]] for the current design.
metadata:
  type: project
---

`src/adapters/codex_ws.rs` (transport: handshake, per-session connection pool,
reader task) + `src/adapters/codex_continuation.rs` (pure `decide()` prefix-match
decision layer) + `src/adapters/responses/mod.rs` (`forward_websocket`/`open_ws_turn`
wiring — note: `responses.rs` became a `responses/` directory module at some point
after PR #39; the file path in older notes below is stale) implement an opt-in
(`provider.websocket = true`, ChatGPT/Codex backend only, gated by
`Config::codex_websocket_enabled`) websocket transport that reuses a pooled
connection and replays `previous_response_id` to cut per-turn upload.

**RESOLVED as of PR #111 / issue #46 (verified 2026-07-14):** the bug described
below (`stream_events()`'s `Close(_) | None` arm sending nothing into `tx`) no
longer exists in the current `codex_ws.rs`. The reader loop is now named
`run_turn`, and EVERY transport-failure exit (idle timeout via `IDLE_TIMEOUT`,
`Message::Close` before a terminal event, unexpected `Message::Binary`, a raw
stream error, or a send failure) explicitly sends
`Err(CodexWsError::transport(...))` into the channel before tearing down —
confirmed by reading `run_turn` end-to-end and cross-checked by a sibling
`review-silent-failure-hunter` pass on the same PR ("no leak, no silent
swallow"). Separately, business-logic failures reported *by the backend itself*
(`event.event == "error"` or `"response.failed"`) are always forwarded as
`Ok(ResponseEvent)`, never `Err(CodexWsError)` — only genuine transport/socket
failures use the `Err` variant. This `Ok`-vs-`Err` split is the invariant the
new `commit_or_fallback` helper (issue #46) depends on to correctly route
transport errors to HTTP-fallback while letting backend-reported errors stream
through as a clean Anthropic `error` SSE event. **Do not re-flag the old PR #39
description below as a live bug** — kept only as historical context for what
issue #46's fix built on top of.

<details>
<summary>Original PR #39 finding (2026-07-12), since fixed</summary>

in `codex_ws.rs::stream_events()`, the `Some(Ok(Message::Close(_))) | None` arm
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
Anthropic error event."

</details>

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
