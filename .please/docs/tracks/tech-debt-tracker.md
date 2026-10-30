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
| TD-024 | inbound-jwt-header-cookie-20261030 | `tests/inbound_codex_jwt_slots.rs` is 558 lines, over the 500-line guideline (#862) | Low | 2026-10-30 |

## Resolved

| ID | Source Track | Description | Resolved In | Date |
|----|------------|-------------|-------------|------|
| TD-001 | per-window-max-utilization-20261004 | Cap verdict re-checked after selection under a second lock (microsecond race, duplicated selectability rules); verdict now returned from the selection pass | #739 | 2026-10-05 |
| TD-006 | spend-enforcement-20261003 | Inbound Codex endpoint (`/v1/responses`, WebSocket) not spend-enforced or metered (#733) | #814 | 2026-10-21 |
| TD-014 | codex-spend-caps-20261021 | `response.refusal.delta` is not counted toward the streaming output floor | #814 | 2026-10-21 |
| TD-021 | anthropic-account-refusal-20261022 | `classify` did not require the top-level `"type": "error"`, so a third-party body without it advanced the chain (#825) | #825 | 2026-10-23 |
