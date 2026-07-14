---
name: issue48-retry-verification
description: Verification outcome for shunt issue #48 (bounded upstream retry/backoff) vs PR #122 — all 5 ACs + 3 comment-suggested extras confirmed implemented and tested.
metadata:
  type: project
---

PR #122 (`amondnet/48`, head `d8a048f`) closes issue #48 in `pleaseai/shunt`. Verified 2026-07-14 by reading `src/retry.rs`, its three call sites (`src/adapters/anthropic/mod.rs`, `src/adapters/responses/mod.rs`, `src/adapters/cursor/mod.rs`), `src/config.rs` (`RetryConfig`), `src/proxy.rs` (`is_count_tokens`), and running `cargo test --lib retry:: / config:: `, `cargo test --test retry`, `cargo clippy -D warnings` — all green.

All 5 issue acceptance criteria met:
1. Pre-stream-only retry: `send_with_retry` wraps only `.send()`/`run_agent()` (obtaining `reqwest::Response` headers+status); streaming (`stream_response`/`json_response`/`relay_response`) always happens strictly after, in code that never re-enters the retry loop — structurally impossible to retry mid-stream.
2. Bounded attempts + backoff: default `max_retries=2` (3 total attempts, within "2-3"); exponential (`initial_backoff * multiplier^attempt`, capped at `max_backoff`) with **true randomized full jitter** (`rand::rng().random_range(0.0..=ceiling)`, not a fixed fraction); `Retry-After` honored via `accounts::retry_after` (pre-existing, integer-seconds only — not this issue's scope), capped by `max_backoff` else `Backoff::ExceedsBudget` (give-up-cleanly, matches issue-comment suggestion).
3. Scope: `is_retryable_status` = exactly 429/502/503/504 (not 500); transient error = `reqwest::Error::is_connect()||is_timeout()` and a per-adapter `RetryableError` (Cursor's own `transient` flag on `CursorError`, set only from `from_reqwest` connect/timeout). Never any other 4xx — asserted directly in unit test `only_transient_statuses_are_retryable`.
4. Configurable per provider (`[providers.<name>.retry]`, `RetryConfig` with validate() capping `max_retries<=10` and `multiplier>=1.0 finite`), default conservative (2/500ms/8s/2.0), OFF for count_tokens — for Responses/Cursor this is moot (count_tokens is intercepted in `proxy.rs` before reaching the adapter at all), for Anthropic it's an explicit `RetryPolicy::DISABLED` when `is_count_tokens(uri)`.
5. Tests: transient→success (`transient_503_then_success_is_retried` + unit), non-transient→immediate (`non_transient_400_surfaces_without_retry` + unit), mid-stream→no-retry is covered only indirectly via `success_is_never_retried` (proves a 2xx is hit exactly once) — there's no test that injects a body-streaming failure and asserts no retry follows; flagged as a very-low-confidence/minor gap since the guarantee is structural (retry code path is never reached again after `Ok(response)`).

Issue-comment suggestions also landed: reusable `src/retry.rs` layer shared by all 3 adapters (comment's ask), true randomized jitter (comment's ask, vs. raine's fixed 0.75x), and the `exceeds_budget` give-up idea (comment's ask).

Scope note (design decision beyond issue's literal text, not a compliance gap): `claude_oauth`/pooled `chatgpt_oauth` account-pool paths (`forward_claude_oauth`, `forward_chatgpt_oauth`) deliberately do NOT get this retry layer — they already have account-rotation failover. Confirmed by reading `responses/mod.rs::forward` branching logic (pooled iff `auth==ChatgptOauth && !accounts.is_empty()`).
