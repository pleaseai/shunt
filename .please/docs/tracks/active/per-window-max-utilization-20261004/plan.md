# Plan: Per-window hard utilization caps for pool accounts

> Track: per-window-max-utilization
> Spec: [requirements](../../../brainstorms/2026-10-04-per-window-max-utilization-requirements.md)

## Overview

- **Source**: .please/docs/brainstorms/2026-10-04-per-window-max-utilization-requirements.md
- **Track**: per-window-max-utilization
- **Issue**: TBD
- **Created**: 2026-10-04
- **Approach**: Pragmatic — exclude capped accounts from the selection rotation, add one cap-exhaustion query for callers
- **Plan Depth**: standard
- **Planned At**: 3bde0a8

## Purpose

Operators can bound how much of each account a quota window may consume. Today's per-window thresholds only reorder accounts, and nothing stops Fable traffic from draining a pool the team needs for Opus. After this change, an account at or past its `max_utilization_*` cap is no longer selected for requests that window governs. A fully capped pool fails over to the next upstream or returns a rate-limit error.

## Context

### Problem

`select_order_inner` (`src/accounts.rs:800`) removes only `disabled` and paused accounts from the rotation (`src/accounts.rs:888-891`). Every quota signal — soft thresholds, `hard_threshold`, cooldowns — only sorts accounts into buckets that the adapters still try in order. No pool path produces a gateway-owned quota error today. When a pool runs out, the last upstream 429 is relayed.

### Requirements Summary

The requirements doc's R1–R17 and AE1–AE6 are the contract. In short:
- **Keys.** Accounts get `max_utilization{,_5h,_7d,_fable}`, and `[server.pool]` gets `default_max_utilization{,_5h,_7d,_fable}`. Each resolves like the soft thresholds but ends in "no cap". Values must be in `[0.0, 1.0]`.
- **Exclusion.** An account is excluded when observed utilization ≥ cap in a governing window. 5h and 7d caps govern every request. The `7d_oi` cap governs Fable requests only, once observed. An unobserved window never excludes.
- **Exhaustion.** An empty order with at least one capped account gives a 429 that advances failover.
- **Scope.** This takes effect on Claude OAuth and Codex/ChatGPT pools. Kimi and Antigravity have no `utilization_*` data, so caps are inert there.
- **Status.** Status surfaces show the capped state distinctly.

### Constraints

- New `Option<f64>` config fields need `#[serde(default, skip_serializing_if = "Option::is_none")]`. `Config::load` round-trips defaults through figment, and a bare `null` breaks it. `AccountConfig` is `deny_unknown_fields`. Both structs have explicit `Default` impls that must list the new fields.
- The client usage endpoints (`/usage`, `/api/oauth/usage`) must not expose per-account names or settings (`docs/m12-client-usage-endpoint.md:29`, `docs/m14-oauth-usage-endpoint.md:193`). A capped signal there must be coarse and account-free.
- Docs must ship in the same PR: the English page plus its ko/ja/zh-cn copies (AGENTS.md). Do not edit `wiki/`.

### Non-Goals

Out of scope per the spec:
- Auto-downgrading Fable to Opus.
- Runtime cap editing from the admin API.
- Per-user or per-key budgets.
- Reserving shared headroom for non-Fable models.
- Burn-rate-projected exclusion.
- Pool-aggregate caps.
- Bypassing failover.

Considered and not built:
- **Cross-validating a cap against the soft threshold or `hard_threshold`.** A cap below the soft threshold is a legitimate configuration: the soft threshold simply never fires. Revisit if operators report confusion.
- **Per-account caps for store-scanned accounts.** These accounts are built with `..Default::default()` (`src/auth/shared.rs:291`), so they get pool defaults only. Soft thresholds behave the same way today.
- **Cap-specific errors on the Kimi and Antigravity paths.** Caps cannot trigger there (no `utilization_*`), so their empty-order paths stay unchanged.
- **A capped state field on the `/api/oauth/usage` wire.** That wire mirrors Anthropic's `fetchUtilization` shape and has no state slot. R17 is met there by computing the bars consistently (T005).

### STOP Conditions

- If `expire_stale_quota` no longer runs before `assess_quota` in `select_order_inner`, stop. R9's automatic recovery depends on it (`src/accounts.rs:870`).

## Architecture Decision

The cap is evaluated in the per-account loop of `select_order_inner`, right after `expire_stale_quota` and `assess_quota`. A capped account is filtered out of `rotation` in the same `.filter` that drops disabled and paused accounts. Filtering at `rotation` is required, not a style choice. The sticky fast path returns the whole `rotation` (`src/accounts.rs:1011-1016`), and the reprobe candidate is drawn from it, with `promote` asserting that the candidate is present (`src/accounts.rs:995`). A filter applied in the later bucket stage would let a capped sticky account through and could trip that assertion.

The cap check cannot reuse the threshold loop in `assess_quota`. For Fable requests, that loop swaps the shared `7d` window for `7d_oi` (`src/accounts.rs:3019-3027`), but R7 requires the 5h and 7d caps to apply to every request, Fable included. A separate resolver mirrors `resolved_threshold`, works with `pool = None`, and ends in "no cap" instead of the hard backstop. A separate check evaluates 5h, 7d, and (for Fable) `7d_oi` independently.

`select_order` returns `Vec<usize>`, and an empty vector already means "all disabled" or "all paused". Widening that return type would touch about 40 call sites and tests. Instead, a sibling query on the pool reports whether caps excluded anyone, and with what earliest eligibility time. Callers ask it only when the order comes back empty. The adapters turn a positive answer into a 429 `rate_limit_error` with `failure: Some(AdapterFailure::UpstreamStatus(429))` and a `retry-after` when the time is known. `run_chain` advances on that failure and remembers it at the top priority (`src/proxy/failover/chain.rs:248-260, 342`). The existing all-disabled error is a 401 with `failure: None`, which stops the chain, so it is the wrong template. The inbound Codex endpoint already re-shapes any `AdapterError` into the OpenAI shape (`src/codex_endpoint.rs:126`), so it needs no shape-specific work.

A cooldown-based design was rejected during brainstorming. It would make a cap change or reset wait on a stored expiry, and it would mix policy exclusion with failure cooldowns on every status surface.

## Tasks

- [x] T001 Add max_utilization keys to account and pool config (file: src/config.rs)
  Validation: TOML with all eight keys parses and round-trips; a value outside `[0.0, 1.0]` or NaN fails `Config::validate` naming the offending key; a config with none of the keys loads unchanged; `shunt.toml.example` documents the pool defaults in comments and still parses.
  Method: `cargo test --all-features config::` plus the example-file load test
- [ ] T002 Exclude capped accounts from pool selection (depends on T001, file: src/accounts.rs)
  Validation: requirements-doc examples AE1 and AE3–AE6 hold at `select_order` (a capped account is absent from the order for exactly the models its capped windows govern, and it returns once the window resets); a capped sticky account is not returned via the fast path.
  Method: `cargo test --all-features accounts::` (new unit tests beside `threshold_resolution_prefers_most_specific_and_caps_at_hard` and `account_reenters_selection_after_reset_passes`)
  STOP: if excluding at `rotation` breaks the reprobe reservation (a probe candidate chosen before the filter), stop and report instead of moving the filter into the bucket stage.
- [ ] T003 Return a cap-exhaustion 429 from the Claude OAuth pool (depends on T002, file: src/adapters/anthropic/mod.rs)
  Validation: when every non-disabled account is capped for the request, `/v1/messages` returns 429 with an Anthropic `rate_limit_error` body naming the cap, plus `retry-after` when the earliest eligibility time is known; with a second `[[upstreams]]` entry, the same request is served by that upstream; a pool exhausted only by pause or upstream failures keeps its current response; the `exhausted` rotation metric gains a distinct `capped` reason.
  Method: `cargo test --all-features --test multi_account --test failover`
- [ ] T004 Return the cap-exhaustion 429 from Codex pool paths (depends on T003, file: src/adapters/responses/pool.rs)
  Validation: an all-capped ChatGPT pool yields a 429 with `retry-after` on the outbound Responses paths (HTTP, committed-stream, and WebSocket-enabled ordering) and advances the failover chain; the inbound Codex endpoint returns the same 429 in the OpenAI error shape.
  Method: `cargo test --all-features --test codex_multi_account --test inbound_codex_endpoint --test codex_websocket_fallback`
- [ ] T005 Report capped state on snapshot and usage surfaces (depends on T002, file: src/accounts.rs)
  Validation: the admin pool JSON marks a capped account with a shared-cap and a Fable-cap flag and `available: false`; `GET /usage` reports an account-free `capped` pool status when no account is available and at least one is capped; `/api/oauth/usage` Fable and weekly bars still compute from the routing-aware set, with the fallback comment and behavior consistent with capped accounts no longer being routed.
  Method: `cargo test --all-features usage:: oauth_usage:: --test admin_surface`
- [ ] T006 [P] Show capped and Fable-capped states on the admin dashboard (depends on T005, file: ui/src/accounts.ts)
  Validation: a capped account row reads "Capped" and a Fable-only capped row reads "Capped (Fable)" in both the accounts table and the pool health panel; a stale observation error does not mask either state.
  Method: `cd ui && bun run test` (extend `coalescing.test.tsx` and `pool-health.test.tsx`) plus `Skill("please:test-browser")` on the admin pool page
- [ ] T007 [P] Document max_utilization caps across reference, guides, and milestone docs (depends on T003, file: site/src/content/docs/reference/configuration.md)
  Validation: the `[server.pool]` and account tables list all eight keys with the resolution order and the "excluded, not reordered" semantics; the multi-account and pool-account-controls guides and the `/usage` endpoint reference describe the capped state; ko/ja/zh-cn copies of every edited page carry the same content; `docs/m8-anthropic-multi-account.md` is updated; `site` builds.
  Method: `cd site && bun run build` and a grep across the four locale trees for `max_utilization`

## Dependencies

T001 → T002. T002 fans out to T003 and T005. T003 → T004 (T004 reuses T003's cap-exhaustion error helper). T005 → T006. T007 needs only the settled behavior from T003 and can run alongside T004–T006.

## Key Files

### Modify
- `src/config.rs` — `PoolConfig` (221), `AccountConfig` (1975), their `Default` impls, and the validation loops at 4298 and 4745 (reuse `InvalidPoolThreshold`/`InvalidAccountThreshold`)
- `src/accounts.rs` — `select_order_inner` per-account loop and `rotation` filter (866-891), `resolved_threshold` (2968, the template for the cap resolver), `AccountSnapshot` (443) and `snapshot` (2162)
- `src/adapters/anthropic/mod.rs` — empty-order exit of `forward_claude_oauth` (688-703)
- `src/adapters/responses/pool.rs`, `src/adapters/responses/mod.rs`, `src/adapters/responses/inbound.rs` — Codex empty-order exits
- `src/usage.rs` (`pool_status`, 268) and `src/oauth_usage.rs` (`routing_aware_window`)
- `ui/src/accounts.ts`, `ui/src/components/PoolHealth.tsx`, `ui/src/components/ObservedAccounts.tsx`, `ui/src/types.ts`
- `tests/responses_cache_affinity.rs:106` — the one full `AccountConfig` literal without `..Default::default()`
- `shunt.toml.example`, site pages and locale copies, `docs/m8-anthropic-multi-account.md`

### Reuse
- `src/accounts.rs` → `expire_stale_quota` — clears utilization once a window resets (R9)
- `src/proxy/failover/chain.rs` → `is_advance_status`, `failure_priority` — failover advance for the 429
- `src/error.rs` → `ShuntError::new` — Anthropic error body; `into_openai_error_shape` handles the inbound endpoint
- `src/accounts.rs` tests → `stamp_near_quota`, `quota_headers`, `accounts()` helpers

## Verification

### Automated Tests

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-features --workspace` (build `ui/dist` first)
- [ ] `cd ui && bun run test`

### Observable Outcomes

- A Claude OAuth pool with `max_utilization_fable = 0.5` and an account whose observed `7d_oi` is 0.55 routes Fable requests to other accounts and still routes Opus to it.
- With every account capped and no second upstream, `curl` against `/v1/messages` returns HTTP 429, a `rate_limit_error` body, and a `retry-after` header.
- The admin dashboard shows the capped account as "Capped (Fable)".

### Manual Testing

- [ ] Run shunt against a real pool with a low `max_utilization_fable`, confirm Fable traffic moves off the capped account, then raise the cap and confirm the account returns after config reload.

### Acceptance Criteria Check

- [ ] AE1–AE6 from the requirements doc each map to a passing test in T002–T004.

## Progress

- [x] (2026-10-04 19:52 KST) T001 Add max_utilization keys to account and pool config — 918ae2c8
  Evidence: `cargo test --all-features config::` → 415 passed, 0 failed; `example_config_files_still_load` passed
  Note: added `#[allow(clippy::large_enum_variant)]` on `AccountSelection` (src/config/upstreams.rs) — the inline `AccountConfig` variant grew past clippy's limit; parsed once at boot

## Decision Log

- Decision: Exclude at the `rotation` filter rather than in the ordering buckets.
  Rationale: The sticky fast path returns the whole rotation, and reprobe asserts its candidate is in the final order. Only a rotation-level filter keeps both correct.
  Date/Author: 2026-10-04 / Claude
- Decision: Add a sibling cap-exhaustion query instead of widening `select_order`'s return type.
  Rationale: An empty order is already ambiguous (all disabled, all paused). A query asked only on empty keeps about 40 call sites and tests untouched.
  Date/Author: 2026-10-04 / Claude
- Decision: The cap error is a 429 `rate_limit_error` with `failure: UpstreamStatus(429)`, not a copy of the all-disabled 401.
  Rationale: The user chose failover to the next upstream. `failure: None` would stop the chain.
  Date/Author: 2026-10-04 / Claude
- Decision: The architecture pass was done inline instead of by a separate architect agent.
  Rationale: The exploration report pinned the constraints (sticky fast path, reprobe assertion, Fable window swap, failover contract), and the brainstorm already settled the strategy, so no competing approach was open.
  Date/Author: 2026-10-04 / Claude

## Surprises & Discoveries

- `tests/codex_multi_account.rs::pool_http_dispatch_seeds_message_start_input_token_estimate` fails under `--all-features` (3/3 solo). It is a known feature-gated/load-dependent failure on main, not attributable to this track.
- `--all-features` builds need `PYO3_PYTHON=/usr/local/bin/python3` in this worktree.
