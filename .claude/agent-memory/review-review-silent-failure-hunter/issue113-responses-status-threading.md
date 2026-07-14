---
name: issue113-responses-status-threading
description: Issue #113 fix (backend-sent error events on non-streaming Responses JSON paths) + its iteration-2 status-threading follow-up in src/adapters/responses/mod.rs — both verified correct.
metadata:
  type: project
---

Issue #113: on the Codex/Responses adapter's non-streaming JSON paths (`json_response` for HTTP,
`json_events_response` for the websocket transport, both in `src/adapters/responses/mod.rs`), a
backend-sent `error`/`response.failed` event arrives as a normal `Ok` event on a `200 OK` stream
(rate-limit, content-policy refusal). Before the fix, `AnthropicSseMachine::apply` silently
discarded these (`let _ = machine.apply(event)`), and the collector returned `200 OK` with
partial/empty content — a silent failure indistinguishable from a truncated-but-successful turn.
Fixed (commit 89e022b) by having the machine record the mapped Anthropic error envelope in a new
`backend_error` field, checked after draining; both collectors now return a `502` via
`backend_error_response()` instead of `200`.

Iteration-2 follow-up (independently verified 2026-07-14, still uncommitted at review time): the
outer `(StatusCode, Response)` tuple at all FOUR non-streaming success call sites
(`forward_http`, `forward_websocket`, and `forward_chatgpt_oauth`'s two `relay_success` sites —
initial classify + refreshed-credential retry) hardcoded `StatusCode::OK` even when the inner
`response` carried a `502`. This didn't affect the client (proxy.rs returns the inner `response`
object directly, whose status was always correct) — it only broke `proxy.rs`'s access log
(`upstream_status`) and `record_proxied_request` metrics, which misreported backend failures as
200. Fix: thread `response.status()` into the tuple instead. Verified: all 4 sites fixed,
streaming branches (`stream_response`/`stream_events_response`) correctly left at hardcoded
`StatusCode::OK` (SSE is genuinely always 200), `with_account_header` only mutates headers not
status, build green.

**Why:** this is the same *class* of bug as [[pr112-message-start-estimate]] (a real signal
computed correctly but not threaded through to the place that reports it) — worth checking for
on future Responses-adapter reviews: is a locally-known-correct value (status, usage, error
detail) being silently dropped or hardcoded on its way out to logging/metrics/the client?

**How to apply:** when reviewing `src/adapters/responses/mod.rs` or `src/model/responses.rs`,
check that every non-streaming success tuple threads the real `response.status()` rather than a
hardcoded `StatusCode::OK`, and that streaming branches are NOT changed to match (SSE responses
are always 200 at the HTTP-status level; failures are surfaced inline as SSE `error` events).
