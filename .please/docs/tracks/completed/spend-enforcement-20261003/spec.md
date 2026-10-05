---
product_spec_domain: gateway/spend-limits
---

# Spend Limit Enforcement (Stage 2)

> Track: spend-enforcement-20261003

## Overview

Stage 1 lets an operator store per-user and organization spend caps through the
`[server.spend]` Admin API, but nothing enforces them: every caller can keep
spending through the gateway's shared upstream credentials. Stage 2 makes the
caps real. The gateway meters what each caller spends on `/v1/messages`,
accumulates it per daily, weekly, and monthly window, and refuses a caller who
has reached a cap until the window resets or an admin raises the cap.

Behavior mirrors the reference Claude apps gateway
(<https://code.claude.com/docs/en/claude-apps-gateway-spend-limits>) so that
Claude Code's spend-limit UI and Admin-API clients written against that
contract work unchanged. The caller ("principal") is the identity the inbound
auth gate already resolves: a static `[server.auth]` token's name, a verified
external JWT's email, or a gateway-login session's email. Requests the gateway
cannot attribute to anyone share one anonymous principal.

Pricing comes from the `[server.spend.pricing]` table delivered by PR #472;
this track builds on it after it merges.

## Requirements

### Functional Requirements

- [ ] FR-1: resolve each `/v1/messages` request's principal from the authenticated inbound identity, matching a `user` cap's `user_id` verbatim
- [ ] FR-2: attribute credential-injecting requests with no authenticated identity to a single anonymous principal that is subject to the organization cap
- [ ] FR-3: resolve, per period, a principal's effective cap as the principal's `user` cap if one exists, otherwise the `organization` cap, otherwise unlimited — the organization cap is a per-principal default, not a shared pool
- [ ] FR-4: check every period's effective cap before forwarding a request and refuse the request when period-to-date spend has reached any of them
- [ ] FR-5: refuse an over-cap request with a `429` Anthropic-shaped `billing_error` whose message names the period and its UTC reset time, appended with `blocked_message` when configured
- [ ] FR-6: name the cap that resets last when a principal is over several caps, and send `retry-after` with the seconds until that reset and `x-should-retry: false`
- [ ] FR-7: meter the cost of every upstream call made on a principal's behalf — the served response plus any routing-judge, classifier, advisor, or escalation call — from the upstream-reported token usage (input, output, cache read, cache write) and server-side web-search requests, priced through the pricing table, and add it to the principal's daily, weekly, and monthly counters
- [ ] FR-8: price a model the pricing table cannot place at a fallback unknown-model rate so no request is metered as free, and warn once per unknown model id
- [ ] FR-9: bill a stream that ends without the upstream's final usage report at a floor estimate derived from the output already delivered to the client
- [ ] FR-10: reset counters on UTC calendar boundaries — daily at 00:00 UTC, weekly on Monday 00:00 UTC, monthly on the 1st 00:00 UTC
- [ ] FR-11: persist counters under the existing spend state configuration (`state_path`, memory-only when empty) so a restart keeps period-to-date spend, and prune elapsed windows older than `spend_retention_months`
- [ ] FR-12: honor `[server.spend.enforcement] fail_closed_on_error` — when a principal's spend state cannot be consulted (its persisted counter record failed to restore, or the cap check hit an internal error), forward the request and warn by default, or refuse it with `429 billing_error` "spend limit unavailable" when set
- [ ] FR-13: add `anthropic-ratelimit-unified-*` headers describing the principal's own most-consumed cap to successful `/v1/messages` responses and to the spend refusal (`429 billing_error`) for a principal who has a cap, stripping every upstream `anthropic-ratelimit-*` header on every status of a metered request
- [ ] FR-14: serve `GET /v1/organizations/spend_limits/effective` returning, per principal and period, the resolved cap, period-to-date spend, and actor details, with `user_ids[]`, `period[]`, `sort=spend_desc`, `q`, `limit`, and `page` query parameters, authorized by read or write admin credentials
- [ ] FR-15: never block or meter `/v1/messages/count_tokens`
- [ ] FR-16: leave passthrough routes unmetered and unenforced, because those callers pay with their own upstream credential
- [ ] FR-17: document the enforcement behavior, pricing interaction, headers, and `/effective` in `docs/gateway-spend-limits.md`, the README locales, and the site reference

### Non-functional Requirements

- [ ] NFR-1: preserve streaming semantics — metering observes usage as a directly relayed stream passes and never buffers or alters bytes sent to the client; a gated capture, which the router retains before serving, is metered from its retained bytes
- [ ] NFR-2: keep a metering or persistence failure from failing the response it meters
- [ ] NFR-3: add no perceptible latency to requests, including for principals without caps
- [ ] NFR-4: keep concurrent updates to one principal's counters lossless

## Acceptance Criteria

- [ ] AC-001: When a principal whose period-to-date spend has reached a cap sends `/v1/messages`, the system shall return `429` with `error.type` `billing_error`.
- [ ] AC-001b: The system shall not forward a request it refuses for a reached cap upstream.
- [ ] AC-002: When a request is refused for a daily cap, the system shall respond with a message of the form `spend limit reached (daily; resets YYYY-MM-DD 00:00 UTC)` followed by the configured `blocked_message`.
- [ ] AC-003: When a request is refused for a reached cap, the system shall send `x-should-retry: false`.
- [ ] AC-003b: When a request is refused for a reached cap, the system shall send a `retry-after` equal to the seconds until the reset of the cap that resets last.
- [ ] AC-004: When a principal has a `user` cap and an `organization` cap exists for the same period, the system shall enforce the `user` cap only.
- [ ] AC-005: When two principals without `user` caps each spend below the organization cap, the system shall forward both, even if their combined spend exceeds it.
- [ ] AC-006: When a streamed response completes with a final usage report, the system shall add its priced cost to the principal's daily, weekly, and monthly counters.
- [ ] AC-007: When a stream ends without a final usage report after delivering output, the system shall add a non-zero floor estimate for that output.
- [ ] AC-008: When a request uses a model absent from the pricing table, the system shall meter it at the unknown-model rate.
- [ ] AC-009: When the UTC day rolls over, the system shall forward a request from a principal who was blocked only by a daily cap.
- [ ] AC-010: When the gateway restarts, the system shall enforce caps against the period-to-date spend recorded before the restart.
- [ ] AC-011: When a credential-injecting request arrives with no authenticated identity and the anonymous principal has reached the organization cap, the system shall refuse it with `429 billing_error`.
- [ ] AC-012: The system shall not block or meter `/v1/messages/count_tokens`.
- [ ] AC-013: The system shall not meter or refuse requests on passthrough routes.
- [ ] AC-014: If spend state cannot be consulted and `fail_closed_on_error` is unset, then the system shall forward the request and log a warning.
- [ ] AC-015: If spend state cannot be consulted and `fail_closed_on_error` is `true`, then the system shall return `429 billing_error` with the message `spend limit unavailable` and no `retry-after`.
- [ ] AC-016: When a principal with a cap receives a successful response, the system shall include `anthropic-ratelimit-unified-*` headers that report that principal's own cap utilization and reset. When the spend check refuses that principal, the over-cap `429` shall carry the exceeded set (`status: rejected`, the period, and `overage-disabled-reason: org_spend_cap_reached`), and the fail-closed `429 spend limit unavailable` shall carry only `overage-disabled-reason: fetch_error`.
- [ ] AC-016b: The system shall not forward any upstream `anthropic-ratelimit-*` header (the unified family and the others, such as `anthropic-ratelimit-tokens-limit`) to a principal with a cap on a metered request, whatever the response status; a fail-open forward carries none, and a chain made only of passthrough or `noop` routes keeps the upstream headers unchanged.
- [ ] AC-017: When an admin calls `/effective` with a read credential, the system shall return one row per principal and period with the resolved cap and period-to-date spend.
- [ ] AC-018: The system shall not alter, delay, or truncate response bytes delivered to the client because of metering.
- [ ] AC-019: When a counter window ended more than `spend_retention_months` ago, the system shall drop it from the persisted spend state.
- [ ] AC-020: When a routed request triggers a routing-judge, classifier, advisor, or escalation upstream call, the system shall add that call's cost to the same principal's counters.

## Success Criteria

- [ ] SC-1: A capped caller cannot exceed their cap through `/v1/messages` by more than the cost of requests already in flight when the cap was reached.
- [ ] SC-2: Claude Code connected through the gateway shows the 75% and 95% spend warnings and the spend-limit message for a capped caller without client-side configuration.
- [ ] SC-3: An admin can identify the top spenders for the current month from a single `/effective` call.
- [ ] SC-4: Metered spend for a request matches the pricing table's cost for its reported usage exactly.

## Out of Scope

- `rbac_group`, `seat_tier`, and `organization_service` scopes, and `group_limit_mode` resolution
- `GET /v1/organizations/spend_limits/audit`
- Hourly retention sweeps of audit records and identity data (`audit_retention_days`, `identity_retention_days`)
- Reserving estimated cost before forwarding (enforcement is a pre-check; in-flight overshoot is accepted, matching the reference)
- The separate request Claude Code makes for dollar amounts in its `/usage` bar
- Pricing table changes (#472) and its follow-ups (#612, #613)
- Metering non-`/v1/messages` inference surfaces (the inbound Codex endpoint, `/v1/responses` and its WebSocket transport); a capped caller can still spend through them, which the docs state as a known gap, tracked in a follow-up issue

## Assumptions

- PR #472 merges before implementation starts; this track consumes its price resolver and adds only the unknown-model fallback rate.
- The unknown-model rate matches the reference gateway's $5 / $25 per million input/output tokens.
- The floor estimate for a stream without final usage matches the reference: about four characters of delivered text per output token.
- The anonymous principal is one shared bucket keyed by the reserved id `shunt:anonymous`. A static token name cannot contain `:` (the `name:token` parser splits on the first colon), and the id is not an email, so it cannot collide with another principal; it appears in `/effective` under that id.
- "Spend state cannot be consulted" covers counter records that failed to restore or a meter-side internal error, since the counter store has no network dependency.
- Actor details in `/effective` are the last-seen identity values: the email for JWT and gateway-login principals, the token name for static-token principals.
