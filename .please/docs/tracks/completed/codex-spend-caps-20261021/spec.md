---
product_spec_domain: gateway/spend-limits
---

# Spend Caps on the Inbound Codex Endpoint

> Track: codex-spend-caps-20261021

## Overview

Spend-limit stage 2 (#728, PR #732) enforces per-user and organization spend
caps, and meters cost, on `/v1/messages` only. The inbound Codex endpoint
(`[server.codex_endpoint]`, OpenAI Responses protocol) authenticates the same
principals and spends the same gateway-owned upstream credentials, but it
neither checks caps nor meters cost, so a capped caller can keep spending
through it (#733).

This track closes that gap. Every Codex-endpoint turn — over HTTP (streamed or
not) and over the inbound Responses WebSocket — is checked against the
caller's effective caps before it reaches an upstream, and its cost is added
to the same period-to-date counters `/v1/messages` uses. One cap therefore
bounds a caller's combined spend across both surfaces. A refused caller gets an
error in the OpenAI Responses shape that the Codex CLI treats as a usage limit
rather than a transient failure to retry.

## Requirements

### Functional Requirements

- [ ] FR-1: resolve each Codex-endpoint request's principal exactly as `/v1/messages` does — the authenticated `[server.auth]` identity (static token name or verified JWT email), or the single anonymous principal when the endpoint runs without `[server.auth]`
- [ ] FR-2: check the principal's effective daily, weekly, and monthly caps before a Codex-endpoint turn reaches any upstream, over the HTTP routes and over the inbound Responses WebSocket, and refuse the turn when period-to-date spend has reached any of them
- [ ] FR-3: refuse an over-cap turn with HTTP `429` and an OpenAI Responses error envelope whose `type` and `code` are `usage_limit_reached`, whose `resets_at` is the Unix time the cap that resets last resets, whose message names that cap's period and UTC reset time (appended with `blocked_message` when configured), and which carries `retry-after` with the seconds until that reset
- [ ] FR-11: carry a configured `blocked_message` on the refusal in the `x-codex-promo-message` header as well, because the Codex CLI renders its own usage-limit text from `resets_at` and that header rather than from the error message
- [ ] FR-4: refuse an over-cap turn on an open inbound WebSocket by sending that turn a terminal error event carrying the same error as FR-3, and keep the socket open for later turns
- [ ] FR-5: refuse the Codex CLI's web-search calls (`/alpha/search` and its aliases) for an over-cap principal with the same error as FR-3, without metering them
- [ ] FR-6: meter each Codex-endpoint turn that completes from the upstream-reported Responses `usage` (input tokens, cached input tokens, output tokens), priced through the existing pricing table by the serving provider, the client-requested model, and the upstream model, into the same counters `/v1/messages` writes
- [ ] FR-12: price a Codex-endpoint model the pricing table cannot place — every GPT/Codex model unless the operator adds a `[[server.spend.pricing.overrides]]` row for it — at the existing unknown-model rate with the existing once-per-model warning, and add no built-in GPT/Codex prices
- [ ] FR-7: bill a turn that ends without the upstream's completion report — a dropped or closed WebSocket, a broken or cancelled stream — at a floor estimate made of the request's estimated input tokens plus the output text already delivered to the client
- [ ] FR-8: honor `[server.spend.enforcement] fail_closed_on_error` on this endpoint the same way `/v1/messages` does when a principal's spend state cannot be consulted
- [ ] FR-9: leave the endpoint unchanged — no cap check, no metering — when `[server.spend]` is not configured
- [ ] FR-10: document the Codex-endpoint enforcement, its refusal shape and what the Codex CLI shows for it, the WebSocket and web-search behavior, the floor-estimate rule, and the need for pricing overrides on GPT/Codex models in `docs/gateway-spend-limits.md`, `docs/m11-inbound-codex-endpoint.md`, the README locales, and the site reference pages with their `ko`/`ja`/`zh-cn` copies

### Non-functional Requirements

- [ ] NFR-1: preserve streaming semantics on the Codex endpoint — metering observes usage as the relayed stream or WebSocket frames pass and never buffers or alters the bytes sent to the client
- [ ] NFR-2: keep a metering or persistence failure from failing the turn it meters
- [ ] NFR-3: add no perceptible latency to Codex-endpoint turns, including for principals without caps

## Acceptance Criteria

- [ ] AC-001 (FR-1, FR-6): When an authenticated client spends through `/v1/messages` and then through the Codex endpoint in the same period, the system shall add both costs to that client's one period-to-date counter.
- [ ] AC-002 (FR-3): If a principal whose monthly spend has reached its cap posts a turn to `/v1/responses`, then the system shall return `429` with `error.type` and `error.code` `usage_limit_reached`.
- [ ] AC-012 (FR-2): If a principal whose monthly spend has reached its cap posts a turn to `/v1/responses`, then the system shall send nothing upstream.
- [ ] AC-003 (FR-3): When several of a principal's caps are reached, the system shall name the cap that resets last in the refusal message, in `resets_at`, and in `retry-after`.
- [ ] AC-013 (FR-11): Where `blocked_message` is configured, the system shall send its text in the refusal's `x-codex-promo-message` header.
- [ ] AC-004 (FR-4): When an over-cap principal sends `response.create` on an open inbound WebSocket, the system shall answer that turn with a terminal error event.
- [ ] AC-014 (FR-4): When the system refuses a turn on an open inbound WebSocket for spend, the system shall keep the socket open.
- [ ] AC-005 (FR-4): While a principal's cap is reached, the system shall refuse each further turn on the same WebSocket.
- [ ] AC-015 (FR-4): When the period of every cap that refused the principal has reset, the system shall accept turns again on a WebSocket that stayed open across the reset.
- [ ] AC-006 (FR-5): If an over-cap principal calls `/alpha/search`, then the system shall refuse the call with the FR-3 error.
- [ ] AC-016 (FR-12): If a Codex turn's model has no pricing override, then the system shall meter it at the unknown-model rate.
- [ ] AC-007 (FR-6): When a streamed Codex turn completes with `usage` reporting cached input tokens, the system shall price the cached portion at the cache-read rate and the rest of the input at the input rate.
- [ ] AC-008 (FR-6): When a turn routed through `[[server.codex_endpoint.routes]]` to a third-party Responses upstream completes, the system shall price it by that route's provider and upstream model.
- [ ] AC-009 (FR-7): If the client closes the WebSocket after output text has been delivered but before `response.completed`, then the system shall record a cost no lower than the estimated input plus the delivered output.
- [ ] AC-010 (FR-8): While `fail_closed_on_error` is on and a capped principal's spend state is unavailable, the system shall refuse that principal's Codex-endpoint turns.

## Out of Scope

- Rate-limit or usage headers on allowed Codex-endpoint responses (the `/v1/messages` path's `anthropic-ratelimit-unified-*` headers have no Codex-protocol counterpart that shunt owns).
- Metering web-search calls, which report no token usage.
- Metering other non-`/v1/messages` surfaces (e.g. Cursor, Antigravity, admin traffic).
- Changes to the caps Admin API, `/v1/organizations/spend_limits/effective`, counter persistence, or the pricing table format — this track reuses them as stage 2 left them.
- Built-in list prices for GPT/Codex models; operators price them with `[[server.spend.pricing.overrides]]`.
- The other Codex-internal rate-limit headers (`x-codex-rate-limit-reached-type`, `x-codex-primary-*`); only `x-codex-promo-message` is sent.

## Assumptions

- Codex CLI maps an error whose type is `usage_limit_reached` — an HTTP `429` body or a wrapped WebSocket `error` event — to its usage-limit error (`codex-rs/codex-api/src/api_bridge.rs`, `responses_websocket.rs`). That error renders its own text from `plan_type`, `resets_at`, and the `x-codex-promo-message` header, ignoring the server message (`codex-rs/protocol/src/error.rs`). At the pinned `rust-v0.159.3` the error is not retried: its retry policy returns no delay for `UsageLimitReached`, and only an HTTP `429` reaches that mapping.
- A Responses `usage.output_tokens` already includes reasoning tokens, so reasoning is priced at the output rate and not added separately.
- A Responses `usage.input_tokens` already includes `input_tokens_details.cached_tokens`, so the uncached input is the difference of the two.
- Responses usage reports no cache-write tokens; cache writes are billed as zero on this endpoint.
- The anonymous principal applies here as on `/v1/messages`: every Codex-endpoint route injects a gateway-owned credential, so no route on this endpoint is a passthrough the caller pays for.
- The endpoint accepts static tokens and external-IdP JWTs but not gateway-login sessions, so FR-1's shared principal covers the identities the endpoint admits.
- `/v1/messages` has no input-token estimator for its floor (upstreams report input at stream start); this endpoint needs a new estimate over the Responses request body, because Responses reports input only at completion.
