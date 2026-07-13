---
name: shunt-codex-ws-peek-fallback-issue46
description: shunt repo — issue #46/PR #111 "peek first WS event before committing" design in src/adapters/responses/mod.rs; what it fixes, the one real tradeoff, and a pre-existing non-bug to stop re-flagging.
metadata:
  type: project
---

Issue #46 extended the Codex WS→HTTP fallback safety net to cover the
"send→first-token" window, not just pre-handshake failures. Reviewed the
diff (`git diff origin/main...HEAD` touching only `src/adapters/responses/mod.rs`
plus docs) on 2026-07-14; see [[shunt-codex-websocket-v2]] for the broader
transport architecture this builds on.

**Mechanism:** `open_ws_turn` (mod.rs ~L359) now ALWAYS calls
`peek_first_event` (recv the first `CodexWsEvents` item) before returning,
replacing the old fast-path that skipped peeking entirely for non-continuation
turns. `commit_or_fallback` then maps `Some(Ok(event))` → commit (buffer +
stream/json as before), `Some(Err(transport_error))` → `Err(AdapterError)`
(triggers `forward()`'s existing HTTP-fallback catch), `None` (channel closed,
nothing ever sent) → `Err` too. Verified correct against all 4 angles a
reviewer would worry about here:
1. Continuation-retry path (`previous_response_missing`) re-peeks the *second*
   turn's stream cleanly, no double-peek, no dropped receiver.
2. No mis-routing either direction — see the `Ok`-vs-`Err` invariant in
   [[shunt-codex-websocket-v2]] (backend business errors are `Ok`, only
   transport failures are `Err`), which is exactly what makes
   `commit_or_fallback`'s split safe.
3. `json_events_response`/`stream_events_response` function bodies are
   UNCHANGED by this diff (only doc comments touched) — no non-streaming
   behavior change.
4. No infinite hang: the peek's `events.recv().await` is bounded by
   `codex_ws.rs`'s `IDLE_TIMEOUT` (300s) in the pathological case, but the
   common case is one small RTT on an already-open pooled connection.

**The one real, legitimate tradeoff (not a bug):** streaming turns that
previously returned immediately (no continuation reuse) now block response
commit on one `events.recv()` with zero keepalive active (axum hasn't
committed the response yet, so `crate::keepalive::with_pings` can't help).
Bounded by `IDLE_TIMEOUT`=300s worst case, single-digit ms typical case on a
warm connection. Reported this at `minor` severity/confidence ~40-45 — a
sibling `review-performance-analyzer` pass on the same PR independently
reached the same conclusion at the same confidence/severity, which is good
corroboration this is genuinely low-stakes, not underrated.

**Pre-existing non-bug, do not treat as introduced by this PR:**
`json_events_response` (mod.rs, and identically `json_response` for the HTTP
path) does `let _ = machine.apply(event);` in its event loop — discards the
return value and never checks `AnthropicSseMachine::stopped`, so a
backend-sent `"error"`/`"response.failed"` event (arrives as `Ok`, see above)
can resolve to a misleading `200 OK` JSON response instead of a gateway error.
This is real but **entirely pre-existing and orthogonal to the peek/fallback
diff** — the function bodies are untouched, and the exposure existed for
every turn and every event position before this PR too (not just "first
events" or "peeked" turns). Confidence for flagging this against PR #111
specifically should stay low (<30) since it's out-of-diff-scope by the
review brief's own filtering rules. A sibling `review-comment-analyzer` pass
flagged the *doc comment* wording (added in this diff) as overclaiming that
backend errors are "always visibly surfaced" — that framing (comment
accuracy) is a fair independent finding even though the underlying behavior
itself isn't new.
