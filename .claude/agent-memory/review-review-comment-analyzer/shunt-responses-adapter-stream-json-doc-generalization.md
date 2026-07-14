---
name: shunt-responses-adapter-stream-json-doc-generalization
description: src/adapters/responses/mod.rs doc comments historically generalized "Anthropic error event" / "streamed through" language across both the SSE streaming path and the non-streaming JSON path; the JSON-path gap this caused was fixed in PR #120 (issue #113) — see resolution note below before re-flagging.
metadata:
  type: project
---

**RESOLVED by PR #120 (issue #113), merged/reviewed 2026-07-14.** The gap described below is fixed — do not re-flag it as an open bug. `AnthropicSseMachine` now has a `backend_error: Option<Value>` field + `pub fn backend_error(&self) -> Option<&Value>` accessor (`src/model/responses.rs`), set when `apply()` hits `"error" | "response.failed"`. Both non-streaming collectors check it after draining and return a `502` via the new `backend_error_response()` helper instead of silently swallowing the error into a `200 OK`:
- `json_response` (HTTP JSON path, `src/adapters/responses/mod.rs`)
- `json_events_response` (WebSocket JSON path, same file)

The old `json_events_response` doc's "Note the asymmetry: ... a pre-existing limitation ... tracked separately" caveat was removed and replaced with accurate language describing the fix. Verified via `cargo doc` (no broken/ambiguous intra-doc-link warnings) and by re-reading `map_error_value`/`anthropic_error_type` — the `502`-derived `error.type` is always `"api_error"` (BAD_GATEWAY isn't in `anthropic_error_type`'s explicit match arms), which the new doc comments correctly describe as "mapped against 502" rather than overclaiming a rate-limit-specific type.

Two minor (low-confidence) doc nits survived in the PR #120 diff, worth a light touch on any follow-up PR touching these spots but not worth blocking on:
- `AnthropicSseMachine::backend_error()`'s own doc comment (`src/model/responses.rs` ~line 161) ends with a self-referential intra-doc link `[`Self::backend_error`]` pointing at itself — meaningless, though it builds clean (no rustdoc warning; the private field of the same name never triggered ambiguity in `cargo doc --no-deps`).
- Two of the four new/touched doc comments list `` `response.failed` `` as if it were a third example reason alongside "rate-limit" and "content-policy refusal" (e.g. "e.g. rate-limit or content-policy refusal or `response.failed`"), when it's actually one of the two triggering *event names* already named earlier in the same sentence — mixes a reason-category list with an event-type name. Locations: the `backend_error` field doc and the `json_events_response` doc, both in the diff.

**Original pre-fix finding (historical, for context on what PR #120 addressed):** doc comments were written from the streaming (SSE) path's point of view and used language ("surfaced as an Anthropic error event", "streamed through") that read as if it applied uniformly to both client modes. `stream_events_response` really did emit an SSE `event: error` line for a mid-stream transport error; `json_events_response` returned a distinct 502 for the same transport-error case (fine); but a backend-sent `Ok`-wrapped error/`response.failed` event was handled asymmetrically — the streaming path rendered `event: error` and used it, while the JSON path did `let _ = machine.apply(event);`, discarding the rendered text and falling through to a normal 200-shaped success body with whatever partial content had accumulated.

**Why kept:** Found while auditing PR #111 (issue #46) doc comments, then confirmed fixed while auditing PR #120 (issue #113). This file's comments in this exact area (stream vs. JSON parity for error handling) are a recurring rot hotspot — worth re-checking generalized "streamed"/"surfaced" language here specifically whenever either path is touched again.

**How to apply:** When `stream_events_response`/`json_events_response`/`json_response`/`AnthropicSseMachine` are touched again, check whether new doc comments still accurately distinguish the streaming vs. non-streaming JSON behavior (don't assume parity without verifying both call sites), and check `anthropic_error_type`/`map_error_value` before trusting any doc claim about what `error.type` a mapped envelope carries.
