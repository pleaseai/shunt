# Plan: Spend Limit Enforcement (Stage 2)

> Track: spend-enforcement-20261003
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: spend-enforcement-20261003
- **Issue**: #728
- **Created**: 2026-10-03
- **Approach**: A spend meter beside the stage-1 cap store — admission pre-check, passive usage sinks on every paid upstream call, UTC-windowed counters flushed to a sibling file.
- **Plan Depth**: deep
- **Planned At**: 59bc8aa

## Context

### Problem

Stage-1 caps are stored but never enforced. Every caller spends through shared upstream credentials without a ceiling.

### Requirements Summary

See spec FR-1–FR-17 / AC-001–AC-020. Scope is `/v1/messages` only; the Codex endpoint is a documented gap.

### Constraints

These are facts from exploration that the executor would otherwise rediscover:

- **Pricing.** PR #472 money is `u64` femto-USD (1 cent = 1e13). `resolve` returns `None` for unknown models, and nothing calls it yet.
- **Admission point.** In `forward` (`src/proxy/failover.rs`), the first upstream-capable step (:240, the classifier judge) follows `check_inbound_auth` (:227) and `enforce_managed_model_policy` (:230). The latter is the precedent for a gateway-owned refusal there.
  - `count_tokens` shares the handler (`is_count_tokens`, `src/proxy.rs:160`).
  - `client: None` means either all-passthrough or no inbound auth. Tell them apart by recomputing `injects_credential` over the admission envelope.
- **Usage visibility.**
  - Only streamed usage is observed today (`src/stream_metrics.rs`, `ObserverState::finish`), and only as a byte count, not delivered text.
  - Non-stream usage is read nowhere.
  - `ChainSuccess` and the committed-stream winner slot carry the alias, not `upstream_model`.
- **Side calls and gated turns.**
  - Judge calls run as `InboundContext::internal()` and discard `value["usage"]` (`src/routing/serve.rs:178,304`).
  - A gated turn discarded on REDO is never observed. A replayed turn is observed on replay.
- **Rate-limit headers.** The `Ok` arm of `proxy::post` sees every success path.

### Non-Goals

Considered and not built:

- **Config validation of the anonymous id.** `shunt:anonymous` cannot be a token name, because the parser splits on the first `:`, and it is not an email. Revisit if token-name syntax changes.
- **Carry-through in the stage-1 envelope.** Counters use a sibling file instead, so they reset on rollback. Revisit if rollback must keep spend.

### STOP Conditions

- PR #472 is not on `main` when implementation starts. Do not vendor its files.

## Architecture Decision

**Principal.**
- Use `InboundContext.client` verbatim.
- If `client` is `None` and the admission envelope injects a credential, use `shunt:anonymous`.
- If the envelope is all-passthrough, there is no principal, so the request is neither metered nor enforced.
- The resolved principal lives on `InboundContext`, so every sink reads one value.

**Meter** (`src/gateway/spend/meter.rs`, owned by `SpendStore`, separate from `SpendState`).
- Counters are keyed by `(principal, period, window_start)`. Each value is a `u64` femto amount updated under a short-held lock.
- Windows follow the UTC calendar: day, Monday-based ISO week, and month. All three are derived from one `now`.
- The cap applies in this order per period: the user cap, then the org cap, then unlimited.
- `check` returns one of:
  - allow;
  - blocked, naming the cap that resets last;
  - unavailable.

**Pricing.**
- Build `PriceTable` on `RuntimeState` so reloads re-apply it.
- When `resolve` returns `None`, charge per million tokens: $5 input, $25 output, $0.50 cache read, $6.25 cache write. This follows the 0.1× / 1.25× ratios. Apply the multiplier, and warn once per model id.
- Charge `web_search_requests` at the table's web-search price.

**Sinks.** Each paid call is metered exactly once.
- **Served stream.** Add a hook on `ObserverState::finish`, carrying the `upstream_model` slot. It counts text-delta characters so it can bill a 4-chars-per-token floor when final usage is missing.
- **Served non-stream.** Use a passive bounded tee on the JSON body. Parse `usage` when the body ends. If the body exceeds the bound, bill a byte-based floor instead.
- **Judge and classifier calls.** Parse `value["usage"]` in `serve.rs` and take the principal from `AdmittedContext`.
- **Gated turns.** Meter them at capture, and turn the replay observer's hook off so the turn is not counted twice.

**Enforcement.**
- Run the check after failover.rs:230, skipping `count_tokens`.
- A blocked request gets `ShuntError(429, "billing_error", "spend limit reached (<period>; resets <date time> UTC)" + blocked_message)`, plus `retry-after` and `x-should-retry: false`. This follows the precedent at `concurrency.rs:112`.
- An unavailable meter forwards the request with a warning, or refuses it with `spend limit unavailable` when `fail_closed_on_error` is set.

**Persistence.**
- Write counters to a sibling file, `<state_path stem>.counters.json`. It has its own version and carries through opaque per-record values, reusing `parse_records` and `interleave_records`.
- Flush in the background on a debounce, modeled on `spawn_state_persister`. Flush again on shutdown, and prune windows older than `spend_retention_months` at each flush.
- If a record names a readable principal but its value is malformed, mark that principal unavailable until its windows elapse.
- If the envelope is unreadable, abort startup, as stage 1 does.

**Headers.**
- `forward` returns the principal to `proxy::post`.
- On a 2xx response to a capped principal, strip the upstream `anthropic-ratelimit-unified-*` headers and write the principal's own values.

Rejected alternatives:
- **Counters inside `SpendState`.** Every read would clone the hot-path state, and every flush would rewrite the audit log.
- **Bumping `STATE_VERSION`.** Rollback boot would fail.
- **Metering only the served response.** Judge calls and discarded gated turns would go unmetered, so AC-020 would fail.
- **Metering at `proxy::post` from `x-gateway-upstream-model`.** That header is absent on committed streams.

## Architecture Diagram

```
request ─► check_inbound_auth ─► principal ─► meter.check ──blocked──► 429 billing_error
                                                  │allow
           judge/classifier ─usage─┐              ▼
           gated capture ───usage──┼──► meter.record ◄── stream/JSON observer (served response)
                                   ▼
                         counters ──debounced flush──► <state>.counters.json
proxy::post Ok ─► (capped principal) replace anthropic-ratelimit-unified-* ─► client
```

## Tasks

- [x] T001 Spend meter core: counters, UTC windows, effective-cap resolution, pricing-to-cost with the unknown-model rate (file: src/gateway/spend/meter.rs) [FR-3, FR-6, FR-8, FR-10, NFR-4, AC-004, AC-005, AC-008, AC-009, SC-4]
  Validation: Caps resolve user → org → unlimited per period. Windows roll at 00:00 UTC each day, on Monday, and on the 1st. `check` names the blocking cap that resets last. An unpriceable model costs the unknown-model rate times the multiplier and warns once per id. Concurrent records on one principal lose no spend.
  Method: `cargo test --all-features -- gateway::spend::meter`
  STOP: PR #472's merged `PriceTable`/`Rates` API differs from `resolve(upstream, client_model, upstream_model) -> Option<Rates>`.
- [x] T002 Refuse over-cap principals at `/v1/messages` admission (file: src/proxy/failover.rs) (depends on T001) [FR-1, FR-2, FR-4, FR-5, FR-12, FR-15, FR-16, NFR-3, AC-001, AC-001b, AC-002, AC-003, AC-003b, AC-011, AC-012, AC-013, AC-014, AC-015, SC-1]
  Validation: An over-cap principal gets `429 billing_error` with the period/reset message and `blocked_message`, plus `retry-after` and `x-should-retry: false`. No upstream receives the request, judge calls included. `count_tokens` and all-passthrough chains are never refused. Unauthenticated credential-injecting requests are enforced as `shunt:anonymous`. When the meter is unavailable, the request is forwarded with a warning by default; with `fail_closed_on_error = true` it is refused with `spend limit unavailable` and no `retry-after`.
  Method: integration tests in `tests/` driving `/v1/messages` against wiremock upstreams with seeded counters
- [ ] T003 Meter served `/v1/messages` responses, streamed and non-streamed, priced on the real upstream model (file: src/stream_metrics.rs) (depends on T001) [FR-7, FR-9, NFR-1, NFR-2, AC-006, AC-007, AC-018]
  Validation: A completed stream and a non-stream JSON response each add exactly their priced usage, priced on the upstream model rather than the alias, to all three counters. This holds on the committed-stream path and on translated adapters. A stream cut before final usage adds a non-zero floor from its delivered text. Client bytes are byte-identical to a meter-off run, and a meter failure never fails the response. `count_tokens` and all-passthrough chains add nothing.
  Method: `cargo test --all-features` stream-metrics and failover integration tests, including a byte-equality check
- [ ] T004 Meter routing side calls and gated turns exactly once (file: src/routing/serve.rs) (depends on T003) [FR-7, AC-020]
  Validation: Judge and classifier costs land on the requesting principal. A replayed gated turn is metered once. A gated turn discarded for escalation is metered once, and the escalated turn is metered separately.
  Method: integration tests with router configs and wiremock upstreams asserting counter totals equal the sum of mocked usages
- [ ] T005 Persist counters across restarts with background flush and retention pruning (file: src/gateway/spend/meter/persist.rs) (depends on T001) [FR-11, AC-010, AC-019]
  Validation: After a restart, enforcement uses pre-restart spend. Windows older than `spend_retention_months` are absent from the written file. A malformed counter record makes only its principal unavailable. `state_path = ""` keeps counters in memory. Counter flushes leave the stage-1 caps file byte-unchanged.
  Method: `cargo test --all-features -- gateway::spend` plus a restart test on a temp `state_path`
  STOP: shutdown offers no way to await a final flush within `shutdown_timeout_seconds`.
- [ ] T006 Report the principal's own cap in `anthropic-ratelimit-unified-*` headers (file: src/proxy.rs) (depends on T002) [FR-13, AC-016, AC-016b, SC-2]
  Validation: A 2xx response to a capped principal carries unified headers for that principal's most-consumed cap. No upstream unified value reaches a capped principal on any 2xx path: relay, committed stream, or gated replay. Responses to uncapped principals are unchanged.
  Method: integration tests over the three 2xx paths
  STOP: the header names and value formats Claude Code reads for spend caps cannot be confirmed from the reference gateway's `GET /protocol` (`run-claude-gateway-ref` skill) or a Claude Code binary.
- [ ] T007 [P] Serve `GET /v1/organizations/spend_limits/effective` (file: src/gateway/spend/api.rs) (depends on T001) [FR-14, AC-017, SC-3]
  Validation: A read or write admin credential gets SpendSummary rows (principal, period, resolved cap, period-to-date spend, actor) for principals with recorded spend. `user_ids[]`, `period[]`, `sort=spend_desc` (exactly one `period[]`), `q`, `limit`, and `page` filter and paginate. A bad credential gets 401, and an invalid query gets 400 in the stage-1 envelope.
  Method: `cargo test --all-features -- gateway::spend` API tests in the stage-1 style
  STOP: Anthropic's `SpendSummary` field names cannot be confirmed from the public Admin API reference.
- [ ] T008 Document enforcement, pricing interaction, headers, `/effective`, and the Codex-endpoint gap (file: docs/gateway-spend-limits.md) (depends on T006, T007) [FR-17]
  Validation: `docs/gateway-spend-limits.md` no longer lists the shipped items as "Not yet implemented" and states the Codex-endpoint gap. The four README locales and the four site configuration-reference locales describe the same behavior. Cross-locale anchors resolve in the built site.
  Method: `bun run build` in `site/` plus a grep of `site/dist` for the linked anchors

## Dependencies

T001 → {T002, T003, T005, T007}; T003 → T004; T002 → T006; {T006, T007} → T008.

## Key Files

- New: `src/gateway/spend/meter.rs`, `src/gateway/spend/meter/persist.rs`.
- Modify:
  - `src/gateway/spend/{store,mod,api}.rs`
  - `src/proxy/failover.rs`, `src/proxy.rs`
  - `src/stream_metrics.rs`
  - `src/proxy/failover/{chain,gated}.rs`, `src/proxy/chain_stream.rs`
  - `src/routing/serve.rs`, `src/routing/serve/gated.rs`
  - `src/server.rs`, `src/main.rs`
  - `docs/gateway-spend-limits.md`, `README*.md`
  - `site/src/content/docs/**/reference/configuration.md`

## Verification

### Automated Tests

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-features --workspace`

### Manual Testing

- [ ] Live run via `Skill("run-shunt")`: a token with a 1-cent daily cap is served once, then refused with `429 billing_error`; Claude Code shows the message; the block survives a restart.

## Test Scenarios

### T004

- An escalation router captures the weak turn, decides REDO, then serves the strong turn. Expect counters to equal weak usage + strong usage + judge usage, each counted once.

## Progress

- [x] (2026-10-04 12:00 KST) T001 Spend meter core
  Evidence: `cargo test --all-features -- gateway::spend::meter` → 13 passed, 0 failed; `gateway::spend` 65 passed; fmt and clippy -D warnings clean
- [x] (2026-10-04 KST) T002 Refuse over-cap principals at `/v1/messages` admission
  Evidence: `cargo test --all-features --test spend_enforcement` → 11 passed, 0 failed; mutation (enforce call removed) → 8 failed; fmt and clippy -D warnings clean; full suite: only pre-existing env failures (responses_chain_stream refused-port x3 and codex_multi_account pool_http_dispatch_seeds_*, both also red on the base commit)

## Decision Log

- 2026-10-03: Counters persist to a sibling file, not the stage-1 envelope. FR-11 was reworded to "under the existing spend state configuration". This keeps rollback safe for caps and audit, and counter flushes never rewrite the audit log.
- 2026-10-03: One inline architecture pass instead of competing architects. Spec and reference-gateway parity settle the strategy, and the rejected alternatives are recorded above.
- 2026-10-03: The anonymous principal id is `shunt:anonymous`, which cannot collide with another principal by construction.

## Surprises & Discoveries
