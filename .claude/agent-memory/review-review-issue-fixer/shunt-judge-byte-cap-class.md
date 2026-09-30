---
name: shunt-judge-byte-cap-class
description: shunt's response_byte_cap defect class — every adapter that materialises a reply must refuse with too_large_error, and "it streams" is the recurring false rationale
metadata:
  type: project
---

`Adapter::forward`'s `response_byte_cap: Option<usize>` is a recurring defect
class in shunt: adapters accept it as `_response_byte_cap` with a comment
claiming they only stream. The claim is true of the *streaming* branch only,
and an internal `[models.router]` (judge) call never takes it — `routing::serve`
strips `stream`, so every judge call lands on the buffered/aggregating branch.
Three adapters shipped the same wrong rationale: Responses (fixed in
`ab06afd5`), Cursor, Antigravity.

**Why:** the bound exists so an oversized judge reply fails open at
`judge_max_response_bytes` instead of allocating until `judge_timeout_ms` /
`HARD_TIMEOUT` (a wall clock, not a byte count). A judge target on these
providers passes validation because `route_is_passthrough` is false for them.

**How to apply:** when a fix must bound an adapter reply,
- one whole-body read → `crate::adapters::collect_upstream_body(upstream, cap)`;
- a reply accumulated from a translated event stream →
  `crate::adapters::over_cap(accumulated, cap)` (added for Cursor's
  `aggregate_turn` and Antigravity's `drain_non_streaming`);
- refuse with `crate::adapters::too_large_error(too_large)` — it is
  `failure: None` plus an `UpstreamBodyTooLarge` response extension, which is
  what `ForwardError::body_too_large()` reads back so `src/routing/serve.rs`
  resolves the judge as `JudgeFailure::Oversized` rather than `UpstreamStatus`.
  Anything else advances the failover chain and spends the cap again.

Test shape the reviewers expect: drive the layer that *receives* the cap (a
test one level below the wiring passes against the bug), make every individual
delta smaller than the cap so only the running total crosses it, and always add
the `None` positive twin — otherwise an unconditional refusal passes too.
