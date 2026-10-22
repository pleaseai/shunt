# Plan: Fail over on Anthropic account-level refusals

> Track: anthropic-account-refusal-20261022
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: anthropic-account-refusal-20261022
- **Issue**: #817
- **Created**: 2026-10-22
- **Approach**: a body-aware refusal classifier in the Anthropic adapter, which peeks the error body only for 400 and 402, tags a recognised refusal so both chain paths advance, and records a memory-only skip window per gateway-owned credential
- **Plan Depth**: standard
- **Planned At**: b199c6d

## Context

### Problem

The chain decides on the status alone. `relay_response` (`src/adapters/anthropic/mod.rs:1105`) returns `Ok((status, response))` with a lazy body for every upstream status, and `run_chain` (`src/proxy/failover/chain.rs:224`) advances only when `is_advance_status` matches. The committed streaming path (`chain_attempt`, `src/adapters/anthropic/mod.rs:1629`) makes the same status-only call before it reads the body. Nothing reads a 400 or 402 body before the decision, and no per-upstream skip state exists: `run_chain` walks `routes` in order (`chain.rs:147`). The only per-credential state is the Claude pool's account cooldown (`accounts.rs:1661` `cooldown_scoped`, skipped by `select_order_inner` at `accounts.rs:939`), and its `classify` (`accounts.rs:3771`) relays every 400 and 402.

The closest precedent is the Codex `model is not supported` rule: `is_codex_model_unsupported` (`accounts.rs:3910`) reads the 400 body through `buffer_error_body` (`src/adapters/responses/pool.rs:1806`), which reads under `ERROR_ENVELOPE_BUDGET`/`ERROR_ENVELOPE_BYTES` (`src/error.rs:103-107`) and rebuilds an intact response so the client still gets the upstream body verbatim.

### Non-Goals

Considered and not built:
- A configurable skip duration: the spec fixes 1 hour and the stated resume time. Make it configurable if an operator asks for a different window.
- Persisting the skip map across restarts: the spec makes it memory-only. Persist it if a restart storm against an exhausted key is observed.

### STOP Conditions

- Stop and report if recognising a refusal on the committed streaming path cannot keep the existing stalled-400 test (`src/adapters/anthropic/mod.rs:~2509`, 1 s timeout, asserts `!advance`) passing unchanged.

## Architecture Decision

**Classify in the adapter, signal through the response.** The Anthropic adapter is the only place that knows the protocol's error envelope, so a new `src/adapters/anthropic/account_refusal.rs` owns a pure classifier: status plus body text in, `Option<refusal kind>` out, with the spend-limit resume time parsed from `You will regain access on YYYY-MM-DD at HH:MM UTC`. For a 400 or 402 the adapter buffers the body under the existing envelope budget and cap, rebuilds the response, and attaches a marker in the response extensions when the classifier matches. `run_chain` advances when the status is an advance status or the marker is present. Every other status keeps today's header-time decision with no body read (NFR-1). A body that times out, exceeds the cap, or is not the Anthropic envelope stays terminal, so the failure mode of the peek is today's behaviour.

`buffer_error_body` moves from `responses/pool.rs` to a shared module so the Codex pool and the Anthropic adapter use one implementation. Without the move, the Sonar duplication gate (3%) is the likely failure.

**Committed stream uses a short peek.** `chain_attempt` must decide at header arrival. For 400 and 402 only, it peeks the body with a sub-second budget. On timeout it falls back to terminal, so a stalled body behaves as it does today and the stall test keeps its 1 s bound.

**Skip state lives beside the pool, keyed by upstream name.** A bounded, memory-only map on `AccountPool` (it already lives in `AppState` and owns cooldown clocks) records `upstream -> skip_until` for `api_key` upstreams only. Passthrough upstreams are never recorded (FR-8). Routes are filtered once, before either chain shape is built (`src/proxy/failover.rs:~536` and `~560`). If filtering would drop every route, the unfiltered list is used (FR-12). The Claude pool needs no new state: a refusal on one account maps to `cooldown_scoped` and rotation, the same as the Rotate arm, and runs before `mark_healthy_scoped` in the Relay arm (`anthropic/mod.rs:331`) so the refusal does not clear the cooldown it just earned.

**Rejected alternatives.**
- Adding 400 and 402 to `is_advance_status`: this resends every malformed request to every upstream, which is the reason the rule exists.
- Returning an `AdapterError` with a new `AdapterFailure` variant: `relay_response` returns `Ok` for every upstream status, and converting the refusal to an error would lose the verbatim body that FR-10 relays on exhaustion.
- A general per-upstream circuit breaker: the spec asks for three refusals, not a health system.

## Tasks

- [x] T001 Recognise the three account-level refusals and the spend-limit resume time (file: src/adapters/anthropic/account_refusal.rs) [FR-1, FR-2, FR-3, FR-5, AC-002, AC-004, AC-007, AC-012]
  Validation: the documented spend-limit (org and workspace), credit-balance (400 and 402), and any-402 `billing_error` bodies classify as refusals; a validation 400, a 400 whose message only contains `usage limits`, and a non-JSON or HTML body do not; the stated resume time parses to that UTC instant, and a missing, unreadable, or past time yields the 1-hour default.
  Method: `cargo test --all-features account_refusal`
- [x] T002 Advance the ordered chain on a refusal and keep every other 400 terminal (file: src/adapters/anthropic/mod.rs) (depends on T001) [FR-4, FR-5, FR-10, NFR-1, NFR-2, NFR-3, AC-001, AC-003, AC-005, AC-006, AC-016, AC-017]
  Validation: in a two-upstream mock chain, each refusal reaches the second upstream and the client gets its response; a validation 400 returns immediately with the second mock at `expect(0)`; a single-upstream refusal returns the upstream's own status and message; a 429 or 5xx still advances without its body being read; the Codex pool's model-refusal tests pass on the shared body-buffer helper.
  Method: `cargo test --all-features --test failover` plus `cargo test --all-features --test codex_multi_account`
- [x] T003 Advance a committed streaming chain on a refusal (file: src/adapters/anthropic/mod.rs) (depends on T001) [FR-4, AC-008]
  Validation: a committed stream whose current element answers with a refusal continues from the next element instead of emitting a terminal error event; a stalled 400 body still returns within the existing 1 s bound as terminal.
  Method: `cargo test --all-features chain_attempt` plus `cargo test --all-features --test failover`
  STOP: the stalled-400 test needs its timeout or assertion changed to pass.
- [x] T004 Skip a refusing API-key upstream until its window ends (file: src/accounts.rs) (depends on T002, T003) [FR-6, FR-8, FR-9, FR-12, NFR-3, AC-009, AC-010, AC-011, AC-012, AC-014, AC-015, AC-018]
  Validation: after a spend-limit refusal stating a future time, later requests skip that upstream without contacting it until the time passes, then reach it again; a credit-balance or billing refusal skips for 1 hour; a passthrough upstream is never skipped; a chain whose every element is skipped is still tried in configured order; a fresh gateway skips nothing; each refusal logs one warning naming the upstream, the refusal kind, and the expiry.
  Method: `cargo test --all-features --test failover` with tokio paused time for the window boundaries
- [x] T005 Rotate the Claude subscription pool on a refusal and cool the refusing account (file: src/adapters/anthropic/mod.rs) (depends on T001) [FR-7, AC-013]
  Validation: a refusal from one pool account sends the request to the next eligible account and cools the first for the refusal's window, on both the first attempt and the post-refresh retry; when every account refuses, the pool relays the last refusal and the chain advances past the upstream.
  Method: `cargo test --all-features --test multi_account`
- [x] T006 Document the account-level refusal rule in the failover contract and user docs (file: docs/upstreams-failover.md) [FR-11]
  Validation: §3, §4, and §6 of the contract name the three refusals, the skip windows, the passthrough exception, and that every other 400 stays terminal; the site configuration reference and troubleshooting page state the same in English and in every existing `ko`/`ja`/`zh-cn` copy, with no fragment link to an anchor the locale lacks.
  Method: `cd site && bun run build` succeeds, then grep the built `site/dist` pages for the new text in each locale

## Key Files

- Create: `src/adapters/anthropic/account_refusal.rs`, the refusal classifier and resume-time parser.
- Modify: `src/adapters/anthropic/mod.rs`, which covers the body peek in `relay_response`, `chain_attempt`, and the `forward_claude_oauth` Relay arm.
- Modify: `src/proxy/failover/chain.rs`, where the advance check honours the refusal marker.
- Modify: `src/proxy/failover.rs`, where routes are filtered before `chain_stream` or `run_chain` is built.
- Modify: `src/accounts.rs`, which holds the upstream skip map.
- Modify: `src/error.rs` (or a new sibling module), which receives the shared body buffer moved from `src/adapters/responses/pool.rs:1806`.
- Modify: `docs/upstreams-failover.md`, `site/src/content/docs/reference/configuration.md`, `site/src/content/docs/reference/troubleshooting.md`, and their `ko`/`ja`/`zh-cn` copies.
- Tests: `tests/failover.rs` (`connect_failure_advances_but_400_returns_immediately` at :486 stays as is), `tests/multi_account.rs`, `tests/codex_multi_account.rs`.

## Verification

### Observable Outcomes

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features --workspace` (the `ui/dist` prebuild is required for `--all-features`).
- [ ] Existing terminal-400 tests pass unmodified: `tests/failover.rs:486` and the stalled-400 test in `src/adapters/anthropic/mod.rs`.
- [ ] Run the gateway once against a wiremock that answers the documented spend-limit 400. Confirm the next upstream serves the request and that a second request skips the refusing upstream.

## Review Focus

- A 400 or 402 body that is not JSON, is HTML from an intermediary, or starts with a BOM must stay terminal and relay byte for byte (T001, T002).
- A refusal body larger than `ERROR_ENVELOPE_BYTES`, or one with a declared oversized `content-length`, must stay terminal and relay intact (T002).
- A stated resume time in the past, or one off by clock skew, falls back to the 1-hour window instead of a zero-length skip (T001).
- Concurrent requests refused by the same upstream at once leave one skip entry and one warning per refusal, with no lost update (T004).
- A pool refusal on the post-refresh retry path cools the account exactly like the first attempt (T005).

## Progress

- [x] (2026-10-22 KST) T001 Recognise the three account-level refusals and the spend-limit resume time
  Evidence: `cargo test --all-features --lib account_refusal` → 8 passed, 0 failed (exit 0); fmt and clippy -D warnings clean
- [x] (2026-10-22 19:20 KST) T002 Advance the ordered chain on a refusal and keep every other 400 terminal
  Evidence: `cargo test --all-features --test failover` → 25 passed, 0 failed (exit 0; 5 new); `cargo test --all-features --test codex_multi_account` → 34 passed, 0 failed (exit 0); mutations: dropping the marker check fails 2 new tests, buffering 429/5xx fails the stalled-body test, substring matching fails the terminal-400 test; fmt and clippy -D warnings clean
- [x] (2026-10-22 21:10 KST) T003 Advance a committed streaming chain on a refusal
  Evidence: `cargo test --all-features chain_attempt` → 8 passed, 0 failed (exit 0; 2 new, stalled-400 test unchanged and green); `cargo test --all-features --test failover` → 25 passed (exit 0); `--test codex_multi_account` → 34 passed (exit 0); `--lib` → 3609 passed (exit 0); `--test responses_chain_stream` → new `an_account_refusal_on_the_primary_advances_the_committed_chain` ok, 14 passed / 3 failed (the 3 pre-existing `refused_base_url` failures on this Mac); mutations: refusal arm `advance: is_advance_status` fails the unit advance test and the integration test, an always-true classifier fails the validation-400 twin, a 5 s peek budget fails the stalled-400 test; fmt and clippy -D warnings clean. Also touched: src/adapters/error_body.rs (budget parameter), src/adapters/mod.rs (re-export), src/proxy/failover{.rs,/refusal.rs} (`log_refusal_advance` made `pub(crate)`), tests/responses_chain_stream.rs
- [x] (2026-10-22 23:30 KST) T004 Skip a refusing API-key upstream until its window ends
  Evidence: `cargo test --all-features --test failover` → 28 passed, 0 failed (exit 0; 3 new: skip effect AC-009, passthrough AC-014, all-skipped order AC-018); `cargo test --all-features --lib` → 3618 passed, 0 failed (exit 0; first run hit the known `malformed_typed_limit` spend-persist flake, rerun green; new: 6 `accounts::upstream_skip` unit tests with paused tokio time for the window end AC-010 and fresh state AC-015, `credit_balance_and_billing_refusals_skip_for_one_hour` AC-011, two `record_refusal` log tests NFR-3, `chain_attempt_skips_an_api_key_upstream_that_refused`); `--test multi_account` → 37 passed (exit 0); `--test responses_chain_stream` → 14 passed / 3 failed (the 3 pre-existing `refused_base_url` failures); fmt and clippy -D warnings clean. Mutations (each red, then restored): no route filter, no passthrough guard, no all-skipped fallback, no forward-side record, no chain_attempt-side record, expiry ignored, later expiry overwrites, no update on a longer refusal, fresh state skips, reversed order, no expiry log field, an extra warn, a 2 h default window. AC-010 is split: the window boundary is tested on `UpstreamSkips` with paused time, the skip effect through a real wiremock gateway (paused time cannot drive reqwest/wiremock reliably, and the stated resume time has minute granularity). AC-012 stays covered by T001's `unreadable_resume_time_yields_default_window`. Also touched: src/accounts/upstream_skip.rs (new), src/adapters/anthropic/{account_refusal.rs,mod.rs} (record at classification), src/proxy/failover{.rs,/refusal.rs,/chain.rs} (filter, record+log, marker), src/routing/serve{.rs,/gated.rs} (filter on internal chain entries)
- [x] (2026-10-22 21:08 KST) T005 Rotate the Claude subscription pool on a refusal and cool the refusing account
  Evidence: `cargo test --all-features --test multi_account` → 41 passed, 0 failed (exit 0; 4 new: `an_account_refusal_rotates_and_cools_the_refusing_account` (first attempt, A cooled 3500-3600 s, a session pinned to A lands on B), `a_refusal_after_refresh_rotates_and_cools_the_refusing_account` (401 → refresh → 402 billing → B, A cooled), `a_pool_where_every_account_refuses_relays_the_last_refusal` (400 + B's body verbatim), `a_validation_400_is_relayed_without_rotating_or_cooling` (B `expect(0)`, no cooldown)); `cargo test --all-features --test failover` → 29 passed (exit 0; new `a_claude_pool_whose_accounts_all_refuse_advances_to_the_next_upstream`); `--lib` → 3618 passed (exit 0); fmt and clippy -D warnings clean. Mutations (each red, then restored from cp backup + touch): first-arm refusal relayed (3 tests red), retry-arm refusal relayed (post-refresh test red), 30 s cooldown instead of the refusal window (both cooled tests red), exhaustion marker dropped (chain test red), every 400 treated as a refusal (validation twin red). Also touched: src/adapters/anthropic/account_refusal.rs (helper `judge_pool_answer`, `PoolAnswer`, `POOL_REFUSAL_REASON`), tests/failover.rs (the chain test, which needs that file's upstream-chain helpers)
  Revision (2026-10-22 21:18 KST, commit c70eea92): a pool exhausted on refusals now records its upstream in the skip map. `cargo test --all-features --test failover` → 29 passed (exit 0; `a_claude_pool_whose_accounts_all_refuse_advances_to_the_next_upstream` now sends two requests, pool `expect(2)` across both, fallback `expect(2)`); `--test multi_account` → 41 passed (exit 0); `--lib` → 3619 passed (exit 0; new `earliest_reselect_is_the_shortest_cooldown_among_selectable_accounts`); fmt and clippy -D warnings clean. Mutations, each red and then restored: drop the upstream record (chain test red), `max` instead of `min` (unit test red), an uncooled account ignored (unit test red). Also touched: src/accounts/upstream_skip.rs (`AccountPool::earliest_reselect`)
- [x] (2026-10-22 KST) T006 Document the account-level refusal rule in the failover contract and user docs
  Evidence: `cd site && bun run build` → exit 0, 209 pages; built `site/dist` en/ko/ja/zh-cn `reference/configuration` and `reference/troubleshooting` each contain `regain access on` and the new text (commit e41c0494). README*.md state no advance statuses, so left unchanged.

## Decision Log

- 2026-10-22: the architecture pass ran inline instead of through a separate architect agent, because exploration settled the hook points and the spec left no competing strategy.
- Decision: the `AccountRefused` marker lives in `src/proxy/failover/refusal.rs` (re-exported as `crate::proxy::failover::AccountRefused`) and the shared body buffer in `src/adapters/error_body.rs`, rather than both in `src/adapters/mod.rs`.
  Rationale: the chain owns the contract the marker expresses, and both `src/adapters/mod.rs` and `chain.rs` were already at or past 500 lines. The refusal-log unit test sits in `chain.rs`'s test module because `store::boot`'s guard forbids the proxy path from referencing `crate::store` (its log-capture helper).
  Date/Author: 2026-10-22 / implement-executor

- Decision: the skip map is `UpstreamSkips` in a new `src/accounts/upstream_skip.rs`, held as a field on `AccountPool` (accessor `upstream_skips()`), instead of inside `src/accounts.rs` itself; it uses `tokio::time::Instant`.
  Rationale: `accounts.rs` is already ~11.5k lines; a field on the `#[derive(Default)]` pool touches no construction site, and the pool is the process-lifetime state every `AppState` snapshot shares across reloads. Tokio's clock makes the window boundary testable with paused time.
  Date/Author: 2026-10-22 / implement-executor
- Decision: the refusal is recorded and logged once at classification in the adapter (`account_refusal::note` → `proxy::failover::record_refusal`, on both `forward` and `chain_attempt`), and `AccountRefused` became a unit marker; the ordered chain no longer logs a refusal advance, and `log_refusal_advance` was replaced by `record_refusal`. The expiry is logged as `skip_until_epoch_secs`; a passthrough refusal logs once without it.
  Rationale: NFR-3 asks for one warning per refusal naming the expiry, which only the recording site knows; logging in both the adapter and the chain would emit two. Recording at classification also covers the `count_tokens` path, which reaches the adapter without the chain.
  Date/Author: 2026-10-22 / implement-executor
- Decision: routes are filtered by one helper, `skip_refused_routes`, at three entries: `failover::forward` just before the `count_tokens` branch, `routing::serve`'s judge call, and `routing::serve::gated::capture`.
  Rationale: the judge and gated-capture calls resolve their own chains with `resolve_target_chain` and reach `run_chain` / the committed stream without passing `forward`'s filter point (the gated exit returns before it). `count_tokens` was already truncated to its first route, so the filter never changes it.
  Date/Author: 2026-10-22 / implement-executor
- Decision: the pool refusal helper `judge_pool_answer` also clears a stale needs-re-login mark, and the chain-advance test for an all-refusing pool lives in `tests/failover.rs`, not `tests/multi_account.rs`.
  Rationale: before this task a refusal was a relayed 4xx, which cleared the mark (`mark_healthy_scoped` on the first attempt, `clear_needs_relogin` after a refresh); skipping those calls on a refusal would have dropped that signal, and a refusal proves the bearer authenticated just as well. Only the cooldown clear is withheld. The chain test needs the `[[upstreams]]` chain helpers (`chain_config`, `upstream`, `passthrough`) that only `tests/failover.rs` has; the four pool-only tests are in `tests/multi_account.rs` as planned.
  Date/Author: 2026-10-22 / implement-executor
- Decision: a Claude pool whose last response is a refusal records its upstream in `UpstreamSkips` (`account_refusal::skip_refused_pool`). The window comes from a new `AccountPool::earliest_reselect`: the shortest remaining governing cooldown among the pool's enabled, unpaused accounts for the route's model. It falls back to `REFUSAL_DEFAULT_SKIP` when no account is selectable. When an account is selectable right now (zero), nothing is recorded. This revises T005's "pool never in the skip map" stance.
  Rationale: per the coordinator's revision, re-contacting a fully refused pool on every request defeats FR-6. No existing query fit. `snapshot` ignores per-model cooldowns and builds a full dashboard row, and the gateway 429 `retry-after` covers caps, not cooldowns. A zero window would skip a pool that holds a usable account, for example one that was only storm-control saturated, so the pool is not skipped in that case rather than given the default window. Logging stays one warn per refusal (per account), plus one distinct "pool exhausted on account refusals" warn carrying `skip_until_epoch_secs`. It does not go through `record_refusal`, which would log the same refusal a second time.
  Date/Author: 2026-10-22 / implement-executor

## Surprises & Discoveries

- Observation: on an alias route (`route.model != route.upstream_model`, every `[[models]]` mapping to a different id), `relay_response` already reads every non-SSE body whole, error statuses included, before `run_chain` decides, so a 429 or 5xx body is read there today regardless of this track. NFR-1 holds only for what this track adds; the no-read guarantee is observable only on non-alias routes, which the stalled-body test uses.
  Evidence: `src/adapters/anthropic/mod.rs` `relay_response` branch `else if alias.is_some() || wrap_buffered` calls `collect_upstream_body` without a status check.
- Observation: a pool whose every account is cooling is still tried, cooled accounts last (`select_order_inner` appends `cooled` to the order). Without a chain-level record, a pool cooled by refusals was therefore re-contacted on every later request, one refused round trip per account. Resolved in the T005 revision: the exhausted pool's upstream is skipped until its first account can serve again (see Decision Log). A pool-only chain still tries the pool through FR-12's all-skipped fallback.
  Evidence: the first draft of `a_claude_pool_whose_accounts_all_refuse_advances_to_the_next_upstream` sent a second request and wiremock recorded 4 pool requests (a, b, a, b). After the revision the same two-request test passes with the pool at `expect(2)`, and dropping the upstream record turns it red.
