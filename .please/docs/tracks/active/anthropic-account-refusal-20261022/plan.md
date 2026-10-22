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

- [ ] T001 Recognise the three account-level refusals and the spend-limit resume time (file: src/adapters/anthropic/account_refusal.rs)
  Validation: the documented spend-limit (org and workspace), credit-balance (400 and 402), and any-402 `billing_error` bodies classify as refusals; a validation 400, a 400 whose message only contains `usage limits`, and a non-JSON or HTML body do not; the stated resume time parses to that UTC instant, and a missing, unreadable, or past time yields the 1-hour default.
  Method: `cargo test --all-features account_refusal`
- [ ] T002 Advance the ordered chain on a refusal and keep every other 400 terminal (file: src/adapters/anthropic/mod.rs) (depends on T001)
  Validation: in a two-upstream mock chain, each refusal reaches the second upstream and the client gets its response; a validation 400 returns immediately with the second mock at `expect(0)`; a single-upstream refusal returns the upstream's own status and message; a 429 or 5xx still advances without its body being read; the Codex pool's model-refusal tests pass on the shared body-buffer helper.
  Method: `cargo test --all-features --test failover` plus `cargo test --all-features --test codex_multi_account`
- [ ] T003 Advance a committed streaming chain on a refusal (file: src/adapters/anthropic/mod.rs) (depends on T001)
  Validation: a committed stream whose current element answers with a refusal continues from the next element instead of emitting a terminal error event; a stalled 400 body still returns within the existing 1 s bound as terminal.
  Method: `cargo test --all-features chain_attempt` plus `cargo test --all-features --test failover`
  STOP: the stalled-400 test needs its timeout or assertion changed to pass.
- [ ] T004 Skip a refusing API-key upstream until its window ends (file: src/accounts.rs) (depends on T002, T003)
  Validation: after a spend-limit refusal stating a future time, later requests skip that upstream without contacting it until the time passes, then reach it again; a credit-balance or billing refusal skips for 1 hour; a passthrough upstream is never skipped; a chain whose every element is skipped is still tried in configured order; a fresh gateway skips nothing; each refusal logs one warning naming the upstream, the refusal kind, and the expiry.
  Method: `cargo test --all-features --test failover` with tokio paused time for the window boundaries
- [ ] T005 Rotate the Claude subscription pool on a refusal and cool the refusing account (file: src/adapters/anthropic/mod.rs) (depends on T001)
  Validation: a refusal from one pool account sends the request to the next eligible account and cools the first for the refusal's window, on both the first attempt and the post-refresh retry; when every account refuses, the pool relays the last refusal and the chain advances past the upstream.
  Method: `cargo test --all-features --test multi_account`
- [ ] T006 Document the account-level refusal rule in the failover contract and user docs (file: docs/upstreams-failover.md)
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

## Decision Log

- 2026-10-22: the architecture pass ran inline instead of through a separate architect agent, because exploration settled the hook points and the spec left no competing strategy.

## Surprises & Discoveries
