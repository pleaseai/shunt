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

**PR #122 (issue #48, bounded upstream retry/backoff) extends the same pattern into
`src/retry.rs`/`src/config.rs`:**

- `RetryPolicy` (`src/retry.rs`) — all 4 fields `pub`, no smart constructor besides the
  `DISABLED` const, no self-validation. The invariants the runtime retry loop actually
  relies on to stay *bounded* (`max_retries <= 10`, `multiplier` finite `>= 1.0`) are
  validated only on the sibling `RetryConfig` type (`src/config.rs`, `.validate()`), one
  hop removed via `.policy()`. Today every call site builds `RetryPolicy` only from an
  already-validated `RetryConfig` or the `DISABLED` const, so it's latent, not exploited —
  flagged at Important-ish confidence (~75) specifically because an unvalidated
  `max_retries`/`multiplier` would defeat the feature's core "bounded" premise (near
  livelock via `attempt as i32` wraparound in `powi`), not just cosmetic drift. The pure
  backoff math (`backoff_ceiling`) is otherwise defensively written (NaN/negative/overflow
  all clamp via `f64::min`/`.max(0.0)` without panicking).
- `RetryConfig::validate()` (`src/config.rs`) is a genuine improvement over the prior
  `CodexWsError`/`Turn` pattern — it's a real callable method, not just prose — but it's
  still opt-in (caller must remember to invoke it; `Config::validate()` does, at startup +
  hot-reload). It also doesn't enforce a minimum backoff, so `initial_backoff_ms = 0` /
  `max_backoff_ms = 0` passes validation and produces immediate zero-delay retries —
  flagged low-confidence (~35) as a permissive-but-plausibly-intentional gap.
- `CursorError.transient: bool` (`src/adapters/cursor/client.rs`) is a nice *counter*-example:
  a private field set only by 3 controlled constructors (`new`/`internal`/`from_reqwest`)
  correctly encodes "transient iff constructed from a connect/timeout `reqwest::Error`".
  Adding it also incidentally closes off external struct-literal construction of
  `CursorError` entirely (Rust requires all fields visible to use literal syntax), which
  is a real encapsulation win. Undermined only by the pre-existing sibling fields
  (`status`, `retry_after`, etc.) staying `pub`-mutable, so a hypothetical future
  post-construction mutation of `.status` could drift out of sync with `.transient` —
  flagged low-confidence (~40) since no such mutation exists yet in the diff.
