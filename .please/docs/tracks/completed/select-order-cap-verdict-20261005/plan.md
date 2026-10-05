# Plan: Report cap exhaustion from account selection

> Track: select-order-cap-verdict-20261005
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: select-order-cap-verdict-20261005
- **Issue**: #739
- **Created**: 2026-10-05
- **Approach**: Have `select_order_inner` keep each account's cap verdict, already computed under its single `entries` lock, and return the folded `CapExhaustion` alongside the order. Adapters then read that value instead of calling `cap_exhaustion`.
- **Plan Depth**: lightweight
- **Planned At**: d40beed

## Context

### Problem

`select_order_inner` (src/accounts.rs) holds one `entries` lock while it computes `excluded[index] = paused || capped` and builds `rotation`. The cap verdict is reduced to a bool there and then discarded. `AccountPool::cap_exhaustion` re-derives the verdict under a second lock, and `adapters::cap_exhausted` (src/adapters/mod.rs) calls it from five exits:

- `src/adapters/anthropic/mod.rs`: Claude OAuth and Kimi
- `src/adapters/responses/pool.rs`: the HTTP loop in `forward_chatgpt_oauth` and `pool_events_stream`
- `src/adapters/responses/inbound.rs`

`src/adapters/gemini/mod.rs` calls `select_order` and has no cap exit.

### Requirements Summary

The spec sets five success criteria:

- SC-1: the cap verdict comes from the same selection pass.
- SC-2: observable behavior is unchanged apart from the race.
- SC-3: existing cap tests are unmodified.
- SC-4: each selectability rule lives in one place.
- SC-5: the Gemini/Antigravity pool gets a cap exit.

### Non-Goals

Per the spec: changing what counts as capped or how `eligible_at` is computed, feeding Antigravity quota into caps, and per-alias cap evaluation on `/usage` (TD-002).

Considered and not built:
- **Changing the tuple returned by `select_order_deferred`.** It is destructured as `(order, reprobe)` in more than ten places, most of them tests in `src/adapters/responses/pool.rs`. Adding `_with_cap` siblings keeps every existing caller and test untouched. Revisit if a later change touches all of those call sites anyway.

## Architecture Decision

- **Compute the verdict on every selection and consume it only on an empty order.** The existing unit test `cap_exhaustion_reports_earliest_eligibility_and_ignores_uncapped_pools` expects a verdict while an uncapped account is still selectable. The fold therefore cannot be gated on `rotation.is_empty()` if the test is to stay unchanged.
  - The fold covers the non-disabled, unpaused representatives (`ident_reps`) that a cap excluded, and takes their earliest `eligible_at`.
  - The fold runs inside the existing lock and loop. It costs one `cap_exclusion` call per capped account, replacing the `cap_flags` bool at the same site.
- **Return path.** `select_order_inner` returns the verdict as a third element.
  - The public wrappers `select_order`, `select_order_deferred`, and `select_order_without_reprobe` keep their signatures.
  - New `_with_cap` siblings return the verdict. Only the adapter exits use them.
- **`adapters::cap_exhausted` takes the verdict** (`Option<CapExhaustion>`) instead of `(state, accounts)`. It no longer touches the pool, and its metric and error construction stay the same.
- **`cap_exhaustion` becomes `#[cfg(test)]`.** It is a thin shim that runs a selection and returns that selection's verdict. Its three unit tests keep their assertions, and no production rule set remains outside `select_order_inner`.

Rejected: keep `cap_exhaustion` and take both locks in one critical section. That still duplicates the selectability rules, which is the drift PR #738 hit twice.

## Tasks

- [x] T001 Return the cap verdict from the selection pass (file: src/accounts.rs)
  Validation: The `_with_cap` selection returns `Some(CapExhaustion)` with the earliest `eligible_at` exactly when an unpaused, non-disabled representative is capped for that request. Once returned, the value is unaffected by later quota changes. `cap_exhaustion` is test-only and all existing `accounts::` cap tests pass unchanged.
  Method: `cargo test --all-features --lib accounts::`, plus a new unit test that clears the capping quota after selection returns and asserts the returned verdict still matches the empty order.
- [x] T002 Switch the five adapter exits to the returned verdict (file: src/adapters/mod.rs) (depends on T001)
  Validation: No production path calls `cap_exhaustion`. The Claude OAuth, Kimi, Codex HTTP, pooled-stream, and inbound exits read the verdict from the selection call that produced their order. The existing cap integration tests pass unmodified. The per-window track's Known Issue "Cap re-check after selection" and tech-debt TD-001 are marked resolved, citing #739.
  Method: `cargo test --all-features --test multi_account --test failover --test codex_multi_account --test inbound_codex_endpoint --test kimi_multi_account --test codex_websocket_fallback`, and `git grep -n cap_exhaustion -- src` shows only `#[cfg(test)]` uses.
  STOP: an exit's empty order comes from a different selection call than any one you can thread the verdict from (for example a retry loop that re-selects). Report it instead of re-checking the pool.
- [x] T003 Give the Gemini/Antigravity pool the cap exit (file: src/adapters/gemini/mod.rs) (depends on T002)
  Validation: When injected utilization caps every Antigravity account, the request gets the gateway cap 429 with `retry-after`, and the rotation metric reason is `capped`. Uncapped behavior is unchanged.
  Method: A new test that seeds Antigravity account quota through the pool's existing test hooks and asserts status, error type, and `retry-after`. Then run `cargo test --all-features` for the Gemini/Antigravity suites.

## Key Files

- `src/accounts.rs`: `select_order_inner`, the new `_with_cap` wrappers, and the `cap_exhaustion` test shim
- `src/adapters/mod.rs`: `cap_exhausted` signature
- `src/adapters/anthropic/mod.rs`, `src/adapters/responses/pool.rs`, `src/adapters/responses/inbound.rs`: exit call sites
- `src/adapters/gemini/mod.rs`: new cap exit
- `.please/docs/tracks/completed/per-window-max-utilization-20261004/plan.md`, `.please/docs/tracks/tech-debt-tracker.md`: Known Issue and TD-001 closure

## Verification

### Automated Tests

- [x] `cargo fmt --all --check`
- [x] `PYO3_PYTHON=/usr/local/bin/python3 cargo clippy --all-targets --all-features -- -D warnings`
- [x] `PYO3_PYTHON=/usr/local/bin/python3 cargo test --all-features --workspace` (build `ui/dist` first)
- [x] `git grep -n "cap_exhaustion" -- src` lists only the test shim and its tests.

## Progress

- [x] (2026-10-05 KST) T001 Return the cap verdict from the selection pass
  Evidence: `cargo test --all-features --lib accounts::` -> 223 passed, 0 failed (includes new select_order_with_cap_verdict_is_a_value_from_the_selection_pass)
- [x] (2026-10-05 KST) T002 Switch the five adapter exits to the returned verdict
  Evidence: `cargo test --all-features --test multi_account --test failover --test codex_multi_account --test inbound_codex_endpoint --test kimi_multi_account --test codex_websocket_fallback` -> 34/20/34/33/10/16 passed, 0 failed; `cargo test --all-features --lib` -> 3033 passed; `git grep cap_exhaustion -- src` -> only cfg(test) use
  Note: test PoolStreamContext literal got `cap: None` (pure field addition); responses/mod.rs:636 also builds a PoolStreamContext in production and now threads the verdict.
- [x] (2026-10-05 KST) T003 Give the Gemini/Antigravity pool the cap exit
  Evidence: `cargo test --all-features --test antigravity_multi_account ...` -> 7 passed incl. capped_pool_returns_the_cap_429_and_uncapped_serves; with the guard disabled that test FAILED (panicked at tests/antigravity_multi_account.rs:606); gemini lib tests 20 passed
  Deferred: rotation-metric `capped` reason not asserted — no cheap metrics reader in the test harness
- [x] (2026-10-05 KST) Review-stage commits (SHA: `73c4e17d`)
  Note: ensemble review (gpt, ocr, cubic; greptile skipped, local CLI broken) found 0 Critical/Important. The two validated Minor findings (hot-path allocation plus extra representative pass, and a vacuous test assertion) were fixed in this commit, and the test was renamed `select_order_with_cap_reports_cap_exhaustion_only_while_capped`. The engines have not re-reviewed it.

## Decision Log

- Decision: `cap_exhaustion` stays non-test as a shim over `select_order_inner(..).2`
  Rationale: `adapters::cap_exhausted` (src/adapters/mod.rs:103) still calls it, so `#[cfg(test)]` broke the non-test build; T002 makes it `#[cfg(test)]` once the last production caller is gone. Until then each call also advances the round-robin counter.
  Date/Author: 2026-10-05 / luna-implementer

## Surprises & Discoveries

## Outcomes & Retrospective

### What Was Shipped
- `select_order_inner` produces the hard-cap verdict in the same locked pass that builds the order, and returns it through new `select_order*_with_cap` siblings.
- All five existing pool exits (Claude OAuth, Kimi, Codex HTTP, pooled stream, inbound Codex) read that verdict, plus a new Gemini/Antigravity exit. `cap_exhaustion` is a `#[cfg(test)]` shim, so the selectability rules exist in one place.
- The race between selection and the cap check is closed. The Known Issue from the per-window cap track and TD-001 are resolved.

### What Went Well
- Computing the verdict on every selection kept every existing cap test unchanged.
- The `_with_cap` siblings avoided touching more than ten `(order, reprobe)` destructurings.
- The mutation check (guard disabled → FAILED) made the Antigravity test's value concrete.

### What Could Improve
- The first cut added a per-call allocation and an extra representative pass on the hot path, which contradicted the spec constraint. Review caught it, and it was fixed by folding into the existing rotation loop. Hot-path constraints should be checked against the diff before the executor reports done.
- The SC-4 regression test first asserted a `Copy` value after mutating the pool, which cannot fail. "Same pass, no re-query" is a structural property (enforced by `cap_exhaustion` being test-only), not a runtime one.

### Tech Debt Created
- None. The `capped` rotation metric is not asserted for the Gemini exit because no test harness metrics reader exists. It shares `cap_exhausted` with the other adapters.

