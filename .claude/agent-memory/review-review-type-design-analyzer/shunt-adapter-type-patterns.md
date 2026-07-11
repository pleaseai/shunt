---
name: shunt-adapter-type-patterns
description: Recurring type-design pattern in pleaseai/shunt's src/adapters/*.rs — internal transport/state structs use all-public fields with invariants documented only in doc comments, enforced by a single smart-constructor call site rather than the type itself.
metadata:
  type: project
---

In `pleaseai/shunt` (Rust/axum LLM gateway), the `src/adapters/` modules define internal
(non-crate-public-API) structs that carry real invariants but express them only in prose,
not in the type:

- `CodexWsError` (`src/adapters/codex_ws.rs`) has 5 `pub` fields where only two
  combinations are meaningful (`status: Some(..)` = handshake rejection immutable from
  `previous_response_missing`/`message`; `previous_response_missing: true` = a distinct
  case). Two smart constructors (`transport()`, `previous_response_missing()`) build the
  valid combinations, but since fields are `pub`, nothing stops a struct literal
  elsewhere from producing an incoherent combination. A tagged enum would be safer, but
  the team may prefer the flat-struct + smart-constructor idiom for ergonomics — flag
  this pattern at moderate (not critical) confidence.
- `Turn`/`ReaderCtx` pair `reused: bool` with `pool_key: Option<String>`, where
  `reused == true` implies `pool_key.is_some()` by construction (only set that way in
  `begin()`), but the type doesn't encode the correlation — a plain bool+Option pair
  instead of an enum like `Fresh(Option<String>)`/`Reused(String)`.
- `RecordPlan::none()` (doc: "records nothing") doesn't actually suppress recording in
  `run_reader` — `run_reader` unconditionally builds a `StoredContinuation` whenever
  `response_id` is `Some`, regardless of whether `record` was `RecordPlan::none()`. It's
  only safe today because `signature()` always emits at least `"{}"`, so the empty-string
  sentinel from `none()` can never equal a real signature — correctness rides on that
  incidental property rather than an explicit "should I record" flag.

None of these caused a shipped bug as of PR #39 (issue #32, WS v2 transport) — they were
reported as coverage-first findings with moderate confidence (45-70), not blocking
Critical issues, since the module is internal-only and single-call-site construction
currently keeps invariants intact by convention.

See [[shunt-project-status]] for the broader issue #32 WS v2 transport context.
