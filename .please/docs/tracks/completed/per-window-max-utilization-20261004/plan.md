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
- **Scope.** This takes effect on Claude OAuth and Codex/ChatGPT pools. Kimi OAuth pools share the same selection, so caps apply there whenever the Kimi upstream reports utilization (none observed so far) and the Kimi path returns the same cap-exhaustion 429 (added in review, `2b3b03ff`). Antigravity quota does not feed caps.
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
- **A cap-specific error on the Antigravity path.** Antigravity quota does not feed caps, so its empty-order path stays unchanged. (Kimi was originally listed here too; review showed Kimi pools can observe utilization, so the Kimi path got the cap exit in `2b3b03ff`.)
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
- [x] T002 Exclude capped accounts from pool selection (depends on T001, file: src/accounts.rs)
  Validation: requirements-doc examples AE1 and AE3–AE6 hold at `select_order` (a capped account is absent from the order for exactly the models its capped windows govern, and it returns once the window resets); a capped sticky account is not returned via the fast path.
  Method: `cargo test --all-features accounts::` (new unit tests beside `threshold_resolution_prefers_most_specific_and_caps_at_hard` and `account_reenters_selection_after_reset_passes`)
  STOP: if excluding at `rotation` breaks the reprobe reservation (a probe candidate chosen before the filter), stop and report instead of moving the filter into the bucket stage.
- [x] T003 Return a cap-exhaustion 429 from the Claude OAuth pool (depends on T002, file: src/adapters/anthropic/mod.rs)
  Validation: when every non-disabled account is capped for the request, `/v1/messages` returns 429 with an Anthropic `rate_limit_error` body naming the cap, plus `retry-after` when the earliest eligibility time is known; with a second `[[upstreams]]` entry, the same request is served by that upstream; a pool exhausted only by pause or upstream failures keeps its current response; the `exhausted` rotation metric gains a distinct `capped` reason.
  Method: `cargo test --all-features --test multi_account --test failover`
- [x] T004 Return the cap-exhaustion 429 from Codex pool paths (depends on T003, file: src/adapters/responses/pool.rs)
  Validation: an all-capped ChatGPT pool yields a 429 with `retry-after` on the outbound Responses paths (HTTP, committed-stream, and WebSocket-enabled ordering) and advances the failover chain; the inbound Codex endpoint returns the same 429 in the OpenAI error shape.
  Method: `cargo test --all-features --test codex_multi_account --test inbound_codex_endpoint --test codex_websocket_fallback`
- [x] T005 Report capped state on snapshot and usage surfaces (depends on T002, file: src/accounts.rs)
  Validation: the admin pool JSON marks a capped account with a shared-cap and a Fable-cap flag and `available: false`; `GET /usage` reports an account-free `capped` pool status when no account is available and at least one is capped; `/api/oauth/usage` Fable and weekly bars still compute from the routing-aware set, with the fallback comment and behavior consistent with capped accounts no longer being routed.
  Method: `cargo test --all-features usage:: oauth_usage:: --test admin_surface`
- [x] T006 [P] Show capped and Fable-capped states on the admin dashboard (depends on T005, file: ui/src/accounts.ts)
  Validation: a capped account row reads "Capped" and a Fable-only capped row reads "Capped (Fable)" in both the accounts table and the pool health panel; a stale observation error does not mask either state.
  Method: `cd ui && bun run test` (extend `coalescing.test.tsx` and `pool-health.test.tsx`) plus `Skill("please:test-browser")` on the admin pool page
- [x] T007 [P] Document max_utilization caps across reference, guides, and milestone docs (depends on T003, file: site/src/content/docs/reference/configuration.md)
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

- [x] `cargo fmt --all --check`
- [x] `cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cargo test --all-features --workspace` (build `ui/dist` first) — CI `fmt · clippy · test` green at `3023ebef`. Locally 3715 passed; the 3 failures in `tests/responses_chain_stream.rs` (`refused_port_is_deterministically_refused` and the two `a_body_error_*` relay tests) failed the same way on a `main` control build, so they are environmental
- [x] `cd ui && bun run test` — 97 passed

### Observable Outcomes

- A Claude OAuth pool with `max_utilization_fable = 0.5` and an account whose observed `7d_oi` is 0.55 routes Fable requests to other accounts and still routes Opus to it.
- With every account capped and no second upstream, `curl` against `/v1/messages` returns HTTP 429, a `rate_limit_error` body, and a `retry-after` header.
- The admin dashboard shows the capped account as "Capped (Fable)".

### Manual Testing

- [ ] Run shunt against a real pool with a low `max_utilization_fable`, confirm Fable traffic moves off the capped account, then raise the cap and confirm the account returns after config reload.

### Acceptance Criteria Check

- [x] AE1–AE6 from the requirements doc each map to a passing test in T002–T004:
  - AE1, AE3: `fable_cap_excludes_for_fable_only_and_needs_observation`
  - AE2: `a_capped_claude_pool_advances_to_the_next_upstream` (tests/failover.rs), `capped_pool_returns_a_gateway_429_naming_the_cap_with_retry_after` (tests/multi_account.rs)
  - AE4: `five_hour_cap_excludes_for_every_model_and_returns_after_reset`
  - AE5: `soft_threshold_deprioritizes_below_the_hard_cap`
  - AE6: `fable_cap_does_not_affect_codex_accounts`

## Progress

- [x] (2026-10-04 19:52 KST) T001 Add max_utilization keys to account and pool config — 918ae2c8
  Evidence: `cargo test --all-features config::` → 415 passed, 0 failed; `example_config_files_still_load` passed
  Note: added `#[allow(clippy::large_enum_variant)]` on `AccountSelection` (src/config/upstreams.rs) — the inline `AccountConfig` variant grew past clippy's limit; parsed once at boot
- [x] (2026-10-04 KST) T002 Exclude capped accounts from pool selection
  Evidence: `cargo test --all-features --lib accounts::` -> 218 passed, 0 failed (new AE1/AE3-AE6, sticky, reset re-entry, pool-level cap tests)
  Note: `cap_exclusion()` / `CapExclusion { eligible_at }` in src/accounts.rs are the reusable verdict for T003/T005 (eligible_at = max reset of capping windows, None if any capping window has no known reset); a pool-level sibling query is left to T003
- [x] (2026-10-04 KST) T003 Return a cap-exhaustion 429 from the Claude OAuth pool
  Evidence: `cargo test --all-features --test multi_account --test failover` -> 34 + 19 passed, 0 failed; `cargo test --all-features --lib accounts::` -> 219 passed
  Note: `AccountPool::cap_exhaustion` (src/accounts.rs) and `adapters::cap_exhausted_error` (src/adapters/mod.rs) are the reusable pieces for T004; the `capped` reason is a new `record_pool_rotation` argument, so src/metrics.rs needed no change
- [x] (2026-10-04 KST) T004 Return the cap-exhaustion 429 from Codex pool paths
  Evidence: `cargo test --all-features --test codex_multi_account --test inbound_codex_endpoint --test codex_websocket_fallback` -> 34 + 16 + 33 passed, 0 failed; `--test failover` -> 20 passed
  Note: `into_openai_error_shape` (src/error.rs) dropped every header, so `retry-after` did not survive the inbound Codex re-shape; it now carries `retry-after` over. The stream path (`pool_events_stream`) covers `pool_or_single_events` and the translated chain arm in responses/mod.rs
- [x] (2026-10-04 20:30 KST) T007 Document max_utilization caps across reference, guides, and milestone docs — 0014c116 (merged)
  Evidence: `cd site && bun run build` → exit 0, 209 pages built; grep for `max_utilization`/`capped` hits all 5 edited pages in en, ko, ja, zh-cn; locale anchors taken from built dist
  Note: locale `reference/configuration.md` copies have no account table, so only their `[server.pool]` table and resolution paragraph were extended
- [x] (2026-10-04 KST) T005 Report capped state on snapshot and usage surfaces
  Evidence: `cargo test --all-features --lib -- usage:: oauth_usage:: accounts::` -> 283 passed; `--test admin_surface` -> 54 passed (new `admin_pool_marks_a_capped_account_with_both_cap_flags_and_unavailable`)
  Note: `AccountSnapshot` gains `capped` (5h/7d) and `capped_fable` (7d_oi, evaluated as for a Fable request); `available` honors the shared cap always and the Fable cap only for a Fable model; `/api/oauth/usage` fallback skips capped accounts for the bar's scope
- [x] (2026-10-04 KST) T006 Show capped and Fable-capped states on the admin dashboard
  Evidence: `cd ui && npm run test` -> 97 passed (new capped/capped-fable cases in coalescing and pool-health); `npx tsc --noEmit` -> exit 0; `npm run build` -> built
  Deferred: `Skill("please:test-browser")` on the admin pool page — needs a running shunt with `[server.admin]` and a capped account; not run here
  Note: also added row notes for both states in ObservedAccounts.tsx; no per-state CSS (cooling siblings have none); the UI has no locale tables
- [x] (2026-10-04 23:34 KST) Review-stage commits (SHA: `490522ad`, `9115dcaa`, `2b3b03ff`, `c7835074`, `6c7dc30c`, `f6e8de04`)

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
- `tests/responses_chain_stream.rs` failures (`refused_port_is_deterministically_refused`, and intermittently the two `a_body_error_*` tests) reproduce on pristine main `3bde0a86` (control worktree, separate target dir, 3 runs: 2–3 failures each). They are environmental on this machine, not caused by this track.
- `into_openai_error_shape` (src/error.rs) dropped all response headers, so `retry-after` never reached clients of the inbound Codex endpoint for any re-shaped error; T004 now carries `retry-after` over.

### Known Issues (review, 2026-10-04)

The ensemble review of PR #738 (gpt, ocr, cubic; review commits 490522ad, 9115dcaa, 2b3b03ff, c7835074) left these validated-but-unproven minor findings unfixed by decision:

- **Cap re-check after selection.** `select_order` and the adapters' `cap_exhaustion` take the entries lock separately (src/adapters/anthropic/mod.rs and src/adapters/responses/{pool,inbound}.rs). If a cap clears in the microseconds between them, the request gets the generic exhaustion error instead of the cap 429. Both errors advance `[[upstreams]]` failover, so only the message and `retry-after` differ. Fix if it matters: return the cap verdict from `select_order_inner` alongside the order. Tracked in #739. **Resolved in #739:** the verdict is now returned from `select_order_inner`.
- **Per-alias caps on `/usage`.** `pool_status` (src/usage.rs) and the admin snapshot read every alias row, each with its own `max_utilization*`, while selection uses only the `collapse_representatives` representative. Aliases with different caps can make `/usage` disagree with selection. This follows the pre-existing per-alias status convention (`representative_positions` covers only the window aggregates) and needs an unusual config (same identity, different caps).
- **Coverage gap.** greptile never ran: the Homebrew node binary fails to load `libada.3.dylib` (ada-url 4.0.0 installed); `brew reinstall node ada-url` should restore it. ocr excludes `.md`/`.mdx` by configuration, so docs were reviewed by gpt and cubic only.

## Outcomes & Retrospective

### What Was Shipped

Per-account, per-window hard utilization caps for pool accounts. Accounts take `max_utilization`, `max_utilization_5h`, `max_utilization_7d`, and `max_utilization_fable`; `[server.pool]` takes the `default_max_utilization*` equivalents. An account at or over its cap is excluded at selection time. The 5h and 7d caps apply to every request, and the Fable (`7d_oi`) cap applies to Fable requests only.

A pool that caps empty returns a gateway 429 `rate_limit_error` with `retry-after`. The 429 advances the `[[upstreams]]` failover chain, and it applies to Claude OAuth, Kimi, Codex/ChatGPT, and the inbound Codex endpoint, which re-shapes it to the OpenAI error format. "Capped" appears on the admin dashboard, `GET /usage`, and `/api/oauth/usage`. The docs are updated in English and ko/ja/zh-cn. PR #738.

### What Went Well

- Filtering at the `rotation` step of `select_order_inner` kept the sticky fast path and re-probe correct for free.
- Making the error a 429 with `failure: UpstreamStatus(429)` reused the existing failover advance, so no new chain logic was needed.
- The ensemble review caught real defects the plan missed. Each fix got a mutation-checked test:
  - `retry-after` ignored observation-expiry deadlines.
  - Paused accounts counted as capped.
  - Aliases contributed deadlines that selection never uses.
  - Kimi lacked the cap exit.

### What Could Improve

- Two of the review-round defects, paused accounts counted as capped and aliases contributing deadlines, were the same class: `cap_exhaustion` re-deriving selection rules that `select_order_inner` already applies. Producing the cap verdict where selection computes it (#739) would have prevented those two. The `retry-after` deadline and the missing Kimi exit were separate defects that #739 would not have caught.
- Doc claims about provider applicability ("inert on Kimi") drifted as soon as the code changed. Grep for each doc claim whenever a provider path changes.
- The review environment was missing one engine for the whole run: greptile, because of a node/ada-url dylib mismatch. Fix the toolchain before the next review, rather than accepting `needs_human_read` on every round.

### Tech Debt Created

- TD-001: the cap verdict is re-checked after selection under a second lock. This causes a microsecond race and duplicated selectability rules. Tracked in #739.
- TD-002: `/usage` and the admin snapshot evaluate caps per alias row, while selection uses one representative per identity. They can disagree only with aliases that have different caps.
