# Tech Debt Tracker

> Tracked across all tracks. Updated during implementation and retrospectives.

## Active

| ID | Source Track | Description | Priority | Created |
|----|------------|-------------|----------|---------|
| TD-002 | per-window-max-utilization-20261004 | `/usage` and admin snapshot evaluate caps per alias row while selection uses the identity representative; can disagree for aliases with different caps | P3 | 2026-10-04 |
| TD-003 | pluggable-store-20261001 | A reload that changes `[server.gateway].state_path` silently redirects `save_if_dirty` (it reads `state.refreshed().config`) with no restart-only warning; pre-existing | P3 | 2026-10-17 |
| TD-004 | pluggable-store-20261001 | `docs/dashboard-metrics.md` series count (18, "sixteen of eighteen write-only") is stale against `src/metrics.rs`; needs a recount | P3 | 2026-10-17 |
| TD-005 | spend-enforcement-20261003 | Spend counters saturate at u64::MAX femto-USD (~$18.4k per principal per window) (#736) | Medium | 2026-10-05 |
| TD-007 | spend-enforcement-20261003 | Data-only SSE frames (no `event:` line) are not metered | Low | 2026-10-05 |
| TD-008 | spend-enforcement-20261003 | `shunt:anonymous` not reserved against authenticated identities | Low | 2026-10-05 |
| TD-009 | spend-enforcement-20261003 | Caught meter panic leaves the counters mutex poisoned (admission `.expect`) | Low | 2026-10-05 |
| TD-010 | spend-enforcement-20261003 | Stale "injects a credential"/"all-passthrough" doc comments; `restore_from` doc; spend files over 500 lines | Low | 2026-10-05 |
| TD-011 | codex-spend-caps-20261021 | Routed WebSocket continuations build the full-transcript estimate body eagerly when `[server.spend]` is set, even for turns that report usage (#821) | Low | 2026-10-21 |
| TD-012 | codex-spend-caps-20261021 | Oversized Responses terminal recovery uses a substring marker in the first 1 KiB and the last `"usage"` object in the 16 KiB tail; it misses a `type` key that comes after the first 1 KiB (#820) | Low | 2026-10-21 |
| TD-013 | codex-spend-caps-20261021 | The usage-less input estimate does not go through the `decode_slots` concurrency bound (#291, #819) | Low | 2026-10-21 |
| TD-015 | codex-spend-caps-20261021 | `cached_tokens` without `input_tokens` is billed twice (the estimate already includes the cached prefix) | Low | 2026-10-21 |
| TD-016 | codex-spend-caps-20261021 | The refusal log line uses the field name `upstream_status` | Low | 2026-10-21 |
| TD-017 | codex-spend-caps-20261021 | `src/codex_endpoint.rs` (710 lines) and `src/stream_metrics.rs` (903 lines) exceed the 500-line guideline | Low | 2026-10-21 |
| TD-018 | anthropic-account-refusal-20261022 | A refusal body that sends part of its bytes and then stalls can hold the response up to 5 s past the caller's idle limit (#825) | Low | 2026-10-23 |
| TD-019 | anthropic-account-refusal-20261022 | The `judge` doc says a body stalled past the budget is not a refusal, but with an idle clock a zero-byte stall returns the idle error (#825) | Low | 2026-10-23 |
| TD-020 | anthropic-account-refusal-20261022 | No integration test checks that a stated `You will regain access on … UTC` time becomes the recorded skip or cooldown; only unit tests cover the parsed window (#825) | Low | 2026-10-23 |
| TD-022 | anthropic-account-refusal-20261022 | Stale "every account refused" comments in `account_refusal.rs` and `upstream_skip.rs`; `account_refusal.rs` (523 lines) and `failover/chain.rs` (566 lines) exceed the 500-line guideline (#825) | Low | 2026-10-23 |
| TD-023 | inbound-jwt-header-cookie-20261030 | `CODEX_PATH_HEADERS` (`src/auth/jwt_slots.rs`) duplicates the headers the Codex endpoint and WebSocket bridge read or rewrite; a new one there must be added to the `jwt_header` reject list by hand (#862) | Low | 2026-10-30 |
| TD-025 | chatgpt-plan-provider-20261030 | `src/adapters/responses/pool_plan_tests.rs` is 695 lines, over the 500-line guideline | Low | 2026-10-30 |
| TD-026 | chatgpt-plan-provider-20261030 | `pool_plan::error_hint` attaches the plan usage-limit hint to every 429 from a plan account, not only `subscription_sharing_usage_limit_exceeded` | Low | 2026-10-30 |
| TD-027 | chatgpt-plan-provider-20261030 | Plan refresh deadline handling, adapted from the xAI store, was kept as plausible-but-unproven in review | Low | 2026-10-30 |
| TD-028 | chatgpt-plan-provider-20261030 | A chatgpt-plan login can overwrite a newer sign-in after an intervening logout (ABA in `commit`) (#877) | Low | 2026-10-30 |
| TD-029 | chatgpt-plan-provider-20261030 | Sign-in without the plan scope drops refresh tokens without revoking them (#878) | Medium | 2026-10-30 |
| TD-030 | codex-discovery-jwt-slots-20261030 | `src/discovery.rs` is 918 lines, over the 500-line guideline | Low | 2026-11-02 |
| TD-031 | session-account-binding-20261031 | The soft-near debounce stamp is not reset by a quota observation between selections, and providers sharing one account share `soft_near_since` (#893) | Low | 2026-11-05 |
| TD-032 | session-account-binding-20261031 | `a_relayed_client_error_commits_nothing` does not pin the streaming turn's account; assert every received request's `authorization` equals `pool.bearer(0)` | Low | 2026-11-05 |
| TD-033 | wif-token-exchange-20261121 | Exchange fingerprints are hashed per request and derived from `Algorithm`'s `Debug` output; the middleware OAuth shape is not gated on the route being registered; 403 code and docs error tables (403/414/431/405) undecided; reload warning wording misleads on removal (#916) | Low | 2026-11-22 |
| TD-035 | wif-token-exchange-20261121 | NFR-2 partial: no test captures the exchange's log lines | Low | 2026-11-22 |
| TD-036 | wif-token-exchange-20261121 | `tests/inbound/inbound_exchange_endpoint.rs` (548) and `src/auth/exchange/tests.rs` (540) exceed the 500-line guideline | Low | 2026-11-22 |
| TD-037 | exchange-rate-limit-20261122 | The exchange failure limit keys on the full IPv6 address, so a client controlling a `/64` can rotate addresses for fresh budgets and fill the 4,096-key table | Low | 2026-11-23 |
| TD-038 | exchange-rate-limit-20261122 | `an_identity_is_exchanged_again_once_its_window_elapses` holds `test_env_lock()` across a 3.2 s real sleep (the handler reads real `Instant`s) | Low | 2026-11-23 |

## Resolved

| ID | Source Track | Description | Resolved In | Date |
|----|------------|-------------|-------------|------|
| TD-001 | per-window-max-utilization-20261004 | Cap verdict re-checked after selection under a second lock (microsecond race, duplicated selectability rules); verdict now returned from the selection pass | #739 | 2026-10-05 |
| TD-006 | spend-enforcement-20261003 | Inbound Codex endpoint (`/v1/responses`, WebSocket) not spend-enforced or metered (#733) | #814 | 2026-10-21 |
| TD-014 | codex-spend-caps-20261021 | `response.refusal.delta` is not counted toward the streaming output floor | #814 | 2026-10-21 |
| TD-021 | anthropic-account-refusal-20261022 | `classify` did not require the top-level `"type": "error"`, so a third-party body without it advanced the chain (#825) | #825 | 2026-10-23 |
| TD-024 | inbound-jwt-header-cookie-20261030 | `tests/inbound_codex_jwt_slots.rs` was 558 lines, over the 500-line guideline; the shared harness moved to `tests/common/codex_jwt.rs` (158 lines), leaving the test at 445 lines | #883 | 2026-11-02 |
| TD-034 | wif-token-exchange-20261121 | Exchange endpoint has no rate limiting (#914) | #917 | 2026-11-23 |
