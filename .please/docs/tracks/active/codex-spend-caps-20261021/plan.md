# Plan: Spend Caps on the Inbound Codex Endpoint

> Track: codex-spend-caps-20261021
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: codex-spend-caps-20261021
- **Issue**: #733
- **Created**: 2026-10-21
- **Approach**: a Codex-endpoint spend gate that renders its own OpenAI-shaped refusal, plus a Responses mode for the existing spend tap
- **Execution**: code
- **Plan Depth**: standard
- **Planned At**: 5ebd404

## Context

### Problem

Every stage-2 primitive is already reachable from the endpoint: `AppState.gateway_stores.spend` (`assess`, `meter`), `spend_prices`, and the crate-visible `SpendTap::for_request` and `ServedTarget`. The gaps are in the transport and parsing layers:

- `into_openai_error_shape` keeps only `message`, `type` and `Retry-After`, so it drops `code`, `resets_at` and `x-codex-promo-message`.
- `run_turn` turns every `ForwardError` into an `api_error` frame.
- `StreamSpend` and `JsonSpendBody` parse only Anthropic usage. `observe_responses` parses Responses terminals, but only for metrics.
- `count_tokens` reads Anthropic `system`/`messages`, so there is no input estimator for a Responses body.

### Non-Goals

Considered and not built:
- Accepting gateway-login sessions on the endpoint. That is an auth change; revisit if the endpoint admits them.
- Eager input estimation on every turn. It costs tokenizer time for a value only the floor uses (NFR-3).

## Architecture Decision

**Admission is its own gate, not a `ForwardError`.**

- The new `src/codex_endpoint/spend.rs` assesses the principal via `SpendStore::assess` and reuses `spend_gate`'s period and reset wording (its helpers become `pub(crate)`).
- It returns either a metering tap or a refusal. The refusal renders as a direct OpenAI-shaped HTTP response, or as a WS frame with the same error object and the safe headers.
- The HTTP `forward` calls it before turn or search dispatch. `run_turn` calls it after its per-turn re-authentication.
- Each turn re-assesses, so an open socket recovers at the period reset without extra state.
- Search is refused but gets no tap. `generate:false` warmups make no upstream call and pass unchecked.

**The refusal body:**

- `type` and `code` are both `usage_limit_reached`.
- `resets_at` is the binding cap's `reset_at`.
- The message is the `/v1/messages` text.
- Headers are `retry-after` and `x-codex-promo-message`. The promo header is sent only when `blocked_message` is a valid header value; the text always stays in the message.
- A fail-closed refusal uses the same type with no reset, so the CLI stops rather than retries.

**Metering reuses the tap.**

- `forward_turn` takes the tap and `record_outcome` hands it to `observe_served(Protocol::Responses, …)`.
- `dispatch_routed` also returns the route's upstream model, which completes the target: provider, client model and upstream model.
- `StreamSpend` and `JsonSpendBody` gain a Responses mode. They read usage from the terminal `response.*` frame or the body's `usage`. The cached tokens are billed as cache-read and the input as `input_tokens − cached_tokens`, saturating. Output text comes from `response.output_text.delta` or the body's `output[]` text.
- A 2xx turn with no usage bills the floor: a lazy input estimate over the retained request `Bytes`, made at settle time, plus the delivered output.

**Rejected alternatives:**

- Gating via `ForwardError`, which both transports flatten.
- Widening `into_openai_error_shape` to smuggle spend fields through the Anthropic envelope.
- A separate Responses tap type, which would duplicate `settle` and `record`.

## Tasks

- [x] T001 Refuse over-cap HTTP turns with the Codex usage-limit error (file: src/codex_endpoint/spend.rs) [FR-1, FR-2, FR-3, FR-8, FR-9, FR-11, NFR-3, AC-002, AC-012, AC-003, AC-013, AC-010]
  Validation: An over-cap POST to each response path gets 429 with `type`/`code` `usage_limit_reached`, plus `resets_at` and `retry-after` for the cap that resets last. `x-codex-promo-message` is present when `blocked_message` is set. The upstream sees nothing. Fail-closed refuses a capped principal whose state is unavailable. Without `[server.spend]` the request is forwarded as before.
  Method: new `tests/codex_spend_enforcement.rs` mirroring `tests/spend_enforcement.rs`, plus unit tests in the new module
- [ ] T002 Refuse web search for over-cap principals without metering it (file: src/codex_endpoint/search.rs) (depends on T001) [FR-5, AC-006]
  Validation: An over-cap `/alpha/search` call, and its aliases, gets the T001 error and is not forwarded. An under-cap search leaves every counter unchanged.
  Method: `tests/codex_spend_enforcement.rs` search cases
- [ ] T003 Refuse over-cap WebSocket turns while keeping the socket open (file: src/codex_endpoint/websocket/turn.rs) (depends on T001) [FR-4, AC-004, AC-014, AC-005, AC-015]
  Validation: An over-cap `response.create` gets one error frame with `status` 429, `usage_limit_reached` type/code, `error.resets_at`, and the promo header. The upstream sees nothing, and the socket stays open and refuses each later turn. A turn after the cap's period resets (meter clock) is forwarded. A `generate:false` warmup is unchanged.
  Method: new `tests/codex_spend_websocket.rs` on the `tests/inbound_codex_websocket.rs` harness
- [ ] T004 Meter completed streamed Codex turns from Responses usage (file: src/stream_metrics/spend_tap/responses.rs) [FR-6, FR-12, NFR-1, NFR-2, AC-001, AC-007, AC-008, AC-016]
  Validation: `response.completed` usage reaches the principal's counter, with cached tokens at the cache-read rate and the rest of the input at the input rate. `cached_tokens > input_tokens` bills zero uncached input without panicking. `/v1/messages` and Codex spend land in one counter. A third-party route is priced by its provider and upstream model, and an un-overridden GPT model at the unknown-model rate. Relayed bytes are unchanged.
  Method: new `tests/codex_spend_metering.rs` mirroring `tests/spend_metering.rs` (byte-identity and decoy-row checks), plus `spend_tap` unit tests
- [ ] T005 Meter non-streamed Codex turns from the JSON body (file: src/stream_metrics/spend_tap/json.rs) (depends on T004) [FR-6]
  Validation: A 2xx JSON Responses body is billed from its `usage` with the T004 cached split, and its bytes reach the client unchanged.
  Method: `tests/codex_spend_metering.rs` JSON case
  STOP: if the endpoint never returns a non-SSE 2xx turn body, record it in Surprises and drop this task.
- [ ] T006 Bill incomplete Codex turns at the estimated-input floor (file: src/count_tokens.rs) (depends on T004) [FR-7, AC-009]
  Validation: A WS closed by the client after output deltas but before `response.completed`, and an HTTP stream broken mid-turn, each record at least estimated input plus delivered output. A turn that reports usage never runs the estimator.
  Method: floor cases in `tests/codex_spend_metering.rs` and `tests/codex_spend_websocket.rs`, using the `replacement_and_disconnect_drop_active_upstream_bodies` pattern
- [x] T007 Stop flagging Codex-endpoint pricing overrides as never pricing anything (file: src/config.rs) [FR-12]
  Validation: An override for a model the Codex endpoint's provider or routes serve loads without the "will never price anything" warning. An override no surface serves still warns.
  Method: `config` unit tests beside the existing `pricing_model_is_requestable` cases
- [ ] T008 Document Codex-endpoint spend enforcement across docs, README, and site (file: docs/gateway-spend-limits.md) (depends on T003, T006) [FR-10]
  Validation: `docs/gateway-spend-limits.md` and `docs/m11-inbound-codex-endpoint.md` cover the refusal shape, CLI display, WS and search behavior, the floor, and GPT pricing overrides, and the #733 "not yet implemented" bullet is gone. The four README spend rows and the site `reference/configuration`, `reference/endpoints` and `guides/inbound-codex-endpoint` pages, including their ko/ja/zh-cn copies, all agree. The `SpendEnforcementConfig` doc comment is no longer limited to `/v1/messages`.
  Method: `bun run build` in `site/` with no new dead anchors, plus a cross-surface grep for `usage_limit_reached` and `#733`

## Key Files

- Create: `src/codex_endpoint/spend.rs`, `src/stream_metrics/spend_tap/responses.rs`, and three `tests/codex_spend_*.rs` files.
- Modify:
  - `src/codex_endpoint.rs` (628 lines already, so the logic stays in `spend.rs`) and `websocket/turn.rs`, `search.rs`.
  - `src/stream_metrics.rs` (pass the protocol to `StreamSpend`), `spend_tap.rs`, `spend_tap/json.rs`.
  - `src/count_tokens.rs`, `src/proxy/spend_gate.rs`, `src/config.rs`, `src/config/spend.rs`.
- Reuse: `SpendStore::assess`, `SpendTap::for_request`/`set_target`, `ServedTarget`, `frame::safe_response_headers`, `observe_responses` (event-set reference).

## Verification

### Observable Outcomes

- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features --workspace` pass (build `ui/dist` first).
- `/verify` live run: shunt with `[server.spend]`, a cap, and the Codex endpoint against a mock Responses upstream. HTTP and WS turns under and over the cap show the refusal body, its headers, and moving counters.

## Review Focus

- A non-ASCII `blocked_message` or one containing a newline omits the promo header but keeps the text in the message (T001).
- An over-cap `generate:false` warmup is not refused (T003).
- `cached_tokens > input_tokens` saturates to zero (T004).
- A 2xx stream ending in `response.failed` or `error` bills its usage if reported, otherwise the floor (T006).

## Progress

- [x] (2026-10-21 KST) T001 Refuse over-cap HTTP turns with the Codex usage-limit error
  Evidence: `cargo test --all-features --test codex_spend_enforcement` → 7 passed (5 go red with `spend::admit` stubbed out); `--lib` 3542 passed, `inbound_codex_endpoint` 33 passed, `spend_enforcement` 13 passed

- [x] (2026-10-21 KST) T007 Stop flagging Codex-endpoint pricing overrides as never pricing anything
  Evidence: `cargo test --all-features --lib config` → 543 passed (incl. `pricing_override_is_requestable_through_the_codex_endpoint`, `pricing_warning_skips_codex_endpoint_models`); fmt and clippy clean

## Decision Log

- 2026-10-21: Designed inline from one exploration pass, with no architect agent. The existing tap and gate fixed the seams; rejected alternatives are listed above.

- 2026-10-21 (T007): Codex-route matching in the pricing predicate is case-insensitive on the trimmed, `[1m]`-stripped model, unlike `CodexEndpointConfig::route_for` (exact match), because `PriceTable::resolve` matches override rows case-insensitively.

## Surprises & Discoveries

- 2026-10-21: `/v1/messages` has no input estimator, contrary to the spec's assumption. The spec is corrected, and T006 builds the estimator.
- Observation: `HeaderValue::from_str` accepts non-ASCII UTF-8 (obs-text bytes `0x80..`), so "valid header value" alone would send a non-ASCII `blocked_message` as the promo header. `promo_header` in `src/codex_endpoint/spend.rs` also requires `is_ascii()`; T003 must reuse it.
  Evidence: the non-ASCII case in `tests/codex_spend_enforcement.rs` failed on the header until the ASCII check was added; unit test `only_visible_ascii_becomes_a_promo_header`.
