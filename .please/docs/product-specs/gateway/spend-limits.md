---
id: SPEC-001
level: V_M
domain: gateway
feature: spend-limits
depends: []
conflicts: []
traces: []
created_at: 2026-10-04T19:02:50.042Z
updated_at: 2026-10-21T05:48:24.604Z
source_tracks: ["spend-enforcement-20261003", "codex-spend-caps-20261021"]
---

# Spend limits Specification

## Purpose

Requirements for gateway spend limits.

## Requirements

### Requirement: resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim
<!-- req: REQ-001 tracks=spend-enforcement-20261003 -->

The system MUST resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim.

### Requirement: attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap
<!-- req: REQ-002 tracks=spend-enforcement-20261003 -->

The system MUST attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap.

### Requirement: resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool
<!-- req: REQ-003 tracks=spend-enforcement-20261003 -->

The system MUST resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool.

### Requirement: check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them
<!-- req: REQ-004 tracks=spend-enforcement-20261003 -->

The system MUST check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them.

### Requirement: refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured
<!-- req: REQ-005 tracks=spend-enforcement-20261003 -->

The system MUST refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured.

### Requirement: name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`
<!-- req: REQ-006 tracks=spend-enforcement-20261003 -->

The system MUST name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`.

### Requirement: meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters
<!-- req: REQ-007 tracks=spend-enforcement-20261003 -->

The system MUST meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters.

### Requirement: price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id
<!-- req: REQ-008 tracks=spend-enforcement-20261003 -->

The system MUST price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id.

### Requirement: bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client
<!-- req: REQ-009 tracks=spend-enforcement-20261003 -->

The system MUST bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client.

### Requirement: reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC
<!-- req: REQ-010 tracks=spend-enforcement-20261003 -->

The system MUST reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC.

### Requirement: persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`
<!-- req: REQ-011 tracks=spend-enforcement-20261003 -->

The system MUST persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`.

### Requirement: honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set
<!-- req: REQ-012 tracks=spend-enforcement-20261003 -->

The system MUST honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set.

### Requirement: add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request
<!-- req: REQ-013 tracks=spend-enforcement-20261003 -->

The system MUST add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request.

### Requirement: serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials
<!-- req: REQ-014 tracks=spend-enforcement-20261003 -->

The system MUST serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials.

### Requirement: never block or meter `/v1/messages/count_tokens`
<!-- req: REQ-015 tracks=spend-enforcement-20261003 -->

The system MUST never block or meter `/v1/messages/count_tokens`.

### Requirement: leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential
<!-- req: REQ-016 tracks=spend-enforcement-20261003 -->

The system MUST leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential.

### Requirement: document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference
<!-- req: REQ-017 tracks=spend-enforcement-20261003 -->

The system MUST document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference.

### Requirement: resolve each Codex-endpoint request's principal exactly as `/v1/messages` does — the authenticated `[server.auth]` identity (static token name or verified JWT email), or the single anonymous principal when the endpoint runs without `[server.auth]`
<!-- req: REQ-022 tracks=codex-spend-caps-20261021 -->

The system MUST resolve each Codex-endpoint request's principal exactly as `/v1/messages` does — the authenticated `[server.auth]` identity (static token name or verified JWT email), or the single anonymous principal when the endpoint runs without `[server.auth]`.

#### Scenario: When an authenticated client spends through `/v1/messages` and then through the Codex endpoint in the same period, the system shall add both costs to that client's one period-to-date counter.

- WHEN an authenticated client spends through `/v1/messages` and then through the Codex endpoint in the same period
- THEN add both costs to that client's one period-to-date counter

### Requirement: check the principal's effective daily, weekly, and monthly caps before a Codex-endpoint turn reaches any upstream, over the HTTP routes and over the inbound Responses WebSocket, and refuse the turn when period-to-date spend has reached any of them
<!-- req: REQ-023 tracks=codex-spend-caps-20261021 -->

The system MUST check the principal's effective daily, weekly, and monthly caps before a Codex-endpoint turn reaches any upstream, over the HTTP routes and over the inbound Responses WebSocket, and refuse the turn when period-to-date spend has reached any of them.

#### Scenario: If a principal whose monthly spend has reached its cap posts a turn to `/v1/responses`, then the system shall send nothing upstream.

- WHEN a principal whose monthly spend has reached its cap posts a turn to `/v1/responses`
- THEN send nothing upstream

### Requirement: refuse an over-cap turn with HTTP `429` and an OpenAI Responses error envelope whose `type` and `code` are `usage_limit_reached`, whose `resets_at` is the Unix time the cap that resets last resets, whose message names that cap's period and UTC reset time (appended with `blocked_message` when configured), and which carries `retry-after` with the seconds until that reset
<!-- req: REQ-024 tracks=codex-spend-caps-20261021 -->

The system MUST refuse an over-cap turn with HTTP `429` and an OpenAI Responses error envelope whose `type` and `code` are `usage_limit_reached`, whose `resets_at` is the Unix time the cap that resets last resets, whose message names that cap's period and UTC reset time (appended with `blocked_message` when configured), and which carries `retry-after` with the seconds until that reset.

#### Scenario: If a principal whose monthly spend has reached its cap posts a turn to `/v1/responses`, then the system shall return `429` with `error.type` and `error.code` `usage_limit_reached`.

- WHEN a principal whose monthly spend has reached its cap posts a turn to `/v1/responses`
- THEN return `429` with `error.type` and `error.code` `usage_limit_reached`

#### Scenario: When several of a principal's caps are reached, the system shall name the cap that resets last in the refusal message, in `resets_at`, and in `retry-after`.

- WHEN several of a principal's caps are reached
- THEN name the cap that resets last in the refusal message, in `resets_at`, and in `retry-after`

### Requirement: carry a configured `blocked_message` on the refusal in the `x-codex-promo-message` header as well, because the Codex CLI renders its own usage-limit text from `resets_at` and that header rather than from the error message
<!-- req: REQ-025 tracks=codex-spend-caps-20261021 -->

The system MUST carry a configured `blocked_message` on the refusal in the `x-codex-promo-message` header as well, because the Codex CLI renders its own usage-limit text from `resets_at` and that header rather than from the error message.

#### Scenario: Where `blocked_message` is configured, the system shall send its text in the refusal's `x-codex-promo-message` header.

- GIVEN `blocked_message` is configured
- THEN send its text in the refusal's `x-codex-promo-message` header

### Requirement: refuse an over-cap turn on an open inbound WebSocket by sending that turn a terminal error event carrying the same error as REQ-024, and keep the socket open for later turns
<!-- req: REQ-026 tracks=codex-spend-caps-20261021 -->

The system MUST refuse an over-cap turn on an open inbound WebSocket by sending that turn a terminal error event carrying the same error as REQ-024, and keep the socket open for later turns.

#### Scenario: When an over-cap principal sends `response.create` on an open inbound WebSocket, the system shall answer that turn with a terminal error event.

- WHEN an over-cap principal sends `response.create` on an open inbound WebSocket
- THEN answer that turn with a terminal error event

#### Scenario: When the system refuses a turn on an open inbound WebSocket for spend, the system shall keep the socket open.

- WHEN the system refuses a turn on an open inbound WebSocket for spend
- THEN keep the socket open

#### Scenario: While a principal's cap is reached, the system shall refuse each further turn on the same WebSocket.

- GIVEN a principal's cap is reached
- THEN refuse each further turn on the same WebSocket

#### Scenario: When the period of every cap that refused the principal has reset, the system shall accept turns again on a WebSocket that stayed open across the reset.

- WHEN the period of every cap that refused the principal has reset
- THEN accept turns again on a WebSocket that stayed open across the reset

### Requirement: refuse the Codex CLI's web-search calls (`/alpha/search` and its aliases) for an over-cap principal with the same error as REQ-024, without metering them
<!-- req: REQ-027 tracks=codex-spend-caps-20261021 -->

The system MUST refuse the Codex CLI's web-search calls (`/alpha/search` and its aliases) for an over-cap principal with the same error as REQ-024, without metering them.

#### Scenario: If an over-cap principal calls `/alpha/search`, then the system shall refuse the call with the REQ-024 error.

- WHEN an over-cap principal calls `/alpha/search`
- THEN refuse the call with the REQ-024 error

### Requirement: meter each Codex-endpoint turn that completes from the upstream-reported Responses `usage` (input tokens, cached input tokens, output tokens), priced through the existing pricing table by the serving provider, the client-requested model, and the upstream model, into the same counters `/v1/messages` writes
<!-- req: REQ-028 tracks=codex-spend-caps-20261021 -->

The system MUST meter each Codex-endpoint turn that completes from the upstream-reported Responses `usage` (input tokens, cached input tokens, output tokens), priced through the existing pricing table by the serving provider, the client-requested model, and the upstream model, into the same counters `/v1/messages` writes.

#### Scenario: When an authenticated client spends through `/v1/messages` and then through the Codex endpoint in the same period, the system shall add both costs to that client's one period-to-date counter.

- WHEN an authenticated client spends through `/v1/messages` and then through the Codex endpoint in the same period
- THEN add both costs to that client's one period-to-date counter

#### Scenario: When a streamed Codex turn completes with `usage` reporting cached input tokens, the system shall price the cached portion at the cache-read rate and the rest of the input at the input rate.

- WHEN a streamed Codex turn completes with `usage` reporting cached input tokens
- THEN price the cached portion at the cache-read rate and the rest of the input at the input rate

#### Scenario: When a turn routed through `[[server.codex_endpoint.routes]]` to a third-party Responses upstream completes, the system shall price it by that route's provider and upstream model.

- WHEN a turn routed through `[[server.codex_endpoint.routes]]` to a third-party Responses upstream completes
- THEN price it by that route's provider and upstream model

### Requirement: price a Codex-endpoint model the pricing table cannot place — every GPT/Codex model unless the operator adds a `[[server.spend.pricing.overrides]]` row for it — at the existing unknown-model rate with the existing once-per-model warning, and add no built-in GPT/Codex prices
<!-- req: REQ-029 tracks=codex-spend-caps-20261021 -->

The system MUST price a Codex-endpoint model the pricing table cannot place — every GPT/Codex model unless the operator adds a `[[server.spend.pricing.overrides]]` row for it — at the existing unknown-model rate with the existing once-per-model warning, and add no built-in GPT/Codex prices.

#### Scenario: If a Codex turn's model has no pricing override, then the system shall meter it at the unknown-model rate.

- WHEN a Codex turn's model has no pricing override
- THEN meter it at the unknown-model rate

### Requirement: bill a turn that ends without the upstream's completion report — a dropped or closed WebSocket, a broken or cancelled stream — at a floor estimate made of the request's estimated input tokens plus the output text already delivered to the client
<!-- req: REQ-030 tracks=codex-spend-caps-20261021 -->

The system MUST bill a turn that ends without the upstream's completion report — a dropped or closed WebSocket, a broken or cancelled stream — at a floor estimate made of the request's estimated input tokens plus the output text already delivered to the client.

#### Scenario: If the client closes the WebSocket after output text has been delivered but before `response.completed`, then the system shall record a cost no lower than the estimated input plus the delivered output.

- WHEN the client closes the WebSocket after output text has been delivered but before `response.completed`
- THEN record a cost no lower than the estimated input plus the delivered output

### Requirement: honor `[server.spend.enforcement] fail_closed_on_error` on this endpoint the same way `/v1/messages` does when a principal's spend state cannot be consulted
<!-- req: REQ-031 tracks=codex-spend-caps-20261021 -->

The system MUST honor `[server.spend.enforcement] fail_closed_on_error` on this endpoint the same way `/v1/messages` does when a principal's spend state cannot be consulted.

#### Scenario: While `fail_closed_on_error` is on and a capped principal's spend state is unavailable, the system shall refuse that principal's Codex-endpoint turns.

- GIVEN `fail_closed_on_error` is on and a capped principal's spend state is unavailable
- THEN refuse that principal's Codex-endpoint turns

### Requirement: leave the endpoint unchanged — no cap check, no metering — when `[server.spend]` is not configured
<!-- req: REQ-032 tracks=codex-spend-caps-20261021 -->

The system MUST leave the endpoint unchanged — no cap check, no metering — when `[server.spend]` is not configured.

### Requirement: document the Codex-endpoint enforcement, its refusal shape and what the Codex CLI shows for it, the WebSocket and web-search behavior, the floor-estimate rule, and the need for pricing overrides on GPT/Codex models in `docs/gateway-spend-limits.md`, `docs/m11-inbound-codex-endpoint.md`, the README locales, and the site reference pages with their `ko`/`ja`/`zh-cn` copies
<!-- req: REQ-033 tracks=codex-spend-caps-20261021 -->

The system MUST document the Codex-endpoint enforcement, its refusal shape and what the Codex CLI shows for it, the WebSocket and web-search behavior, the floor-estimate rule, and the need for pricing overrides on GPT/Codex models in `docs/gateway-spend-limits.md`, `docs/m11-inbound-codex-endpoint.md`, the README locales, and the site reference pages with their `ko`/`ja`/`zh-cn` copies.

## Non-functional Requirements

### Requirement: preserve streaming semantics — metering observes usage as a directly relayed stream passes and never buffers or alters bytes sent to the client; a gated capture, which the router retains before serving, is metered from its retained bytes
<!-- req: REQ-018 tracks=spend-enforcement-20261003 -->

The system SHOULD preserve streaming semantics — metering observes usage as a directly relayed stream passes and never buffers or alters bytes sent to the client; a gated capture, which the router retains before serving, is metered from its retained bytes.

### Requirement: keep a metering or persistence failure from failing the response it meters
<!-- req: REQ-019 tracks=spend-enforcement-20261003 -->

The system SHOULD keep a metering or persistence failure from failing the response it meters.

### Requirement: add no perceptible latency to requests, including for principals without caps
<!-- req: REQ-020 tracks=spend-enforcement-20261003 -->

The system SHOULD add no perceptible latency to requests, including for principals without caps.

### Requirement: keep concurrent updates to one principal's counters lossless
<!-- req: REQ-021 tracks=spend-enforcement-20261003 -->

The system SHOULD keep concurrent updates to one principal's counters lossless.

### Requirement: preserve streaming semantics on the Codex endpoint — metering observes usage as the relayed stream or WebSocket frames pass and never buffers or alters the bytes sent to the client
<!-- req: REQ-034 tracks=codex-spend-caps-20261021 -->

The system SHOULD preserve streaming semantics on the Codex endpoint — metering observes usage as the relayed stream or WebSocket frames pass and never buffers or alters the bytes sent to the client.

### Requirement: keep a metering or persistence failure from failing the turn it meters
<!-- req: REQ-035 tracks=codex-spend-caps-20261021 -->

The system SHOULD keep a metering or persistence failure from failing the turn it meters.

### Requirement: add no perceptible latency to Codex-endpoint turns, including for principals without caps
<!-- req: REQ-036 tracks=codex-spend-caps-20261021 -->

The system SHOULD add no perceptible latency to Codex-endpoint turns, including for principals without caps.
