---
id: SPEC-001
level: V_M
domain: gateway
feature: spend-limits
depends: []
conflicts: []
traces: []
created_at: 2026-10-04T19:02:50.042Z
updated_at: 2026-10-04T19:02:50.042Z
source_tracks: ["spend-enforcement-20261003"]
---

# Spend limits Specification

## Purpose

Requirements for gateway spend limits.

## Requirements

### Requirement: resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim
<!-- req: REQ-001 tracks=spend-enforcement-20261003 -->

The system MUST resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim.

#### Scenario: resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim

- GIVEN the system is operating normally
- WHEN resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim
- THEN the behavior is carried out as specified

### Requirement: attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap
<!-- req: REQ-002 tracks=spend-enforcement-20261003 -->

The system MUST attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap.

#### Scenario: attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap

- GIVEN the system is operating normally
- WHEN attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap
- THEN the behavior is carried out as specified

### Requirement: resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool
<!-- req: REQ-003 tracks=spend-enforcement-20261003 -->

The system MUST resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool.

#### Scenario: resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool

- GIVEN the system is operating normally
- WHEN resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool
- THEN the behavior is carried out as specified

### Requirement: check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them
<!-- req: REQ-004 tracks=spend-enforcement-20261003 -->

The system MUST check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them.

#### Scenario: check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them

- GIVEN the system is operating normally
- WHEN check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them
- THEN the behavior is carried out as specified

### Requirement: refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured
<!-- req: REQ-005 tracks=spend-enforcement-20261003 -->

The system MUST refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured.

#### Scenario: refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured

- GIVEN the system is operating normally
- WHEN refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured
- THEN the behavior is carried out as specified

### Requirement: name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`
<!-- req: REQ-006 tracks=spend-enforcement-20261003 -->

The system MUST name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`.

#### Scenario: name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`

- GIVEN the system is operating normally
- WHEN name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`
- THEN the behavior is carried out as specified

### Requirement: meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters
<!-- req: REQ-007 tracks=spend-enforcement-20261003 -->

The system MUST meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters.

#### Scenario: meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters

- GIVEN the system is operating normally
- WHEN meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters
- THEN the behavior is carried out as specified

### Requirement: price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id
<!-- req: REQ-008 tracks=spend-enforcement-20261003 -->

The system MUST price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id.

#### Scenario: price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id

- GIVEN the system is operating normally
- WHEN price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id
- THEN the behavior is carried out as specified

### Requirement: bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client
<!-- req: REQ-009 tracks=spend-enforcement-20261003 -->

The system MUST bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client.

#### Scenario: bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client

- GIVEN the system is operating normally
- WHEN bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client
- THEN the behavior is carried out as specified

### Requirement: reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC
<!-- req: REQ-010 tracks=spend-enforcement-20261003 -->

The system MUST reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC.

#### Scenario: reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC

- GIVEN the system is operating normally
- WHEN reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC
- THEN the behavior is carried out as specified

### Requirement: persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`
<!-- req: REQ-011 tracks=spend-enforcement-20261003 -->

The system MUST persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`.

#### Scenario: persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`

- GIVEN the system is operating normally
- WHEN persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`
- THEN the behavior is carried out as specified

### Requirement: honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set
<!-- req: REQ-012 tracks=spend-enforcement-20261003 -->

The system MUST honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set.

#### Scenario: honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set

- GIVEN the system is operating normally
- WHEN honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set
- THEN the behavior is carried out as specified

### Requirement: add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request
<!-- req: REQ-013 tracks=spend-enforcement-20261003 -->

The system MUST add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request.

#### Scenario: add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request

- GIVEN the system is operating normally
- WHEN add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request
- THEN the behavior is carried out as specified

### Requirement: serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials
<!-- req: REQ-014 tracks=spend-enforcement-20261003 -->

The system MUST serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials.

#### Scenario: serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials

- GIVEN the system is operating normally
- WHEN serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials
- THEN the behavior is carried out as specified

### Requirement: never block or meter `/v1/messages/count_tokens`
<!-- req: REQ-015 tracks=spend-enforcement-20261003 -->

The system MUST never block or meter `/v1/messages/count_tokens`.

#### Scenario: never block or meter `/v1/messages/count_tokens`

- GIVEN the system is operating normally
- WHEN never block or meter `/v1/messages/count_tokens`
- THEN the behavior is carried out as specified

### Requirement: leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential
<!-- req: REQ-016 tracks=spend-enforcement-20261003 -->

The system MUST leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential.

#### Scenario: leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential

- GIVEN the system is operating normally
- WHEN leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential
- THEN the behavior is carried out as specified

### Requirement: document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference
<!-- req: REQ-017 tracks=spend-enforcement-20261003 -->

The system MUST document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference.

#### Scenario: document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference

- GIVEN the system is operating normally
- WHEN document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference
- THEN the behavior is carried out as specified

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
