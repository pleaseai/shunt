---
id: SPEC-002
level: V_M
domain: routing
feature: failover
depends: []
conflicts: []
traces: []
created_at: 2026-10-23T07:59:27.729Z
updated_at: 2026-10-23T07:59:27.729Z
source_tracks: ["anthropic-account-refusal-20261022"]
---

# Failover Specification

## Purpose

Failover Specification 관련 요구사항.

## Requirements

### Requirement: classify as an account-level refusal an Anthropic-protocol upstream response with HTTP 400 whose error message starts with `You have reached your specified API usage limits` or `You have reached your specified workspace API usage limits` (a spend limit the account owner set)
<!-- req: REQ-001 tracks=anthropic-account-refusal-20261022 -->

The system MUST classify as an account-level refusal an Anthropic-protocol upstream response with HTTP 400 whose error message starts with `You have reached your specified API usage limits` or `You have reached your specified workspace API usage limits` (a spend limit the account owner set).

#### Scenario: When the first upstream of a two-element chain answers 400 `invalid_request_error` with `You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.`, the system shall send the request to the second upstream and return its response.

- WHEN the first upstream of a two-element chain answers 400 `invalid_request_error` with `You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.`
- THEN send the request to the second upstream and return its response

#### Scenario: When an upstream answers 400 with `You have reached your specified workspace API usage limits`, the system shall classify it as an account-level refusal.

- WHEN an upstream answers 400 with `You have reached your specified workspace API usage limits`
- THEN classify it as an account-level refusal

### Requirement: classify as an account-level refusal an Anthropic-protocol upstream response with HTTP 400 or 402 whose error message starts with `Your credit balance is too low to access the Anthropic API` (prepaid or plan-included credit exhausted)
<!-- req: REQ-002 tracks=anthropic-account-refusal-20261022 -->

The system MUST classify as an account-level refusal an Anthropic-protocol upstream response with HTTP 400 or 402 whose error message starts with `Your credit balance is too low to access the Anthropic API` (prepaid or plan-included credit exhausted).

#### Scenario: When an upstream answers 400 with `Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.`, the system shall advance to the next chain element.

- WHEN an upstream answers 400 with `Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.`
- THEN advance to the next chain element

#### Scenario: When an upstream answers 402 `billing_error` with the credit-balance message, the system shall classify it as an account-level refusal.

- WHEN an upstream answers 402 `billing_error` with the credit-balance message
- THEN classify it as an account-level refusal

### Requirement: classify as an account-level refusal any Anthropic-protocol upstream response with HTTP 402 and error type `billing_error`
<!-- req: REQ-003 tracks=anthropic-account-refusal-20261022 -->

The system MUST classify as an account-level refusal any Anthropic-protocol upstream response with HTTP 402 and error type `billing_error`.

#### Scenario: When an upstream answers 402 `billing_error` with any other message, the system shall advance to the next chain element.

- WHEN an upstream answers 402 `billing_error` with any other message
- THEN advance to the next chain element

### Requirement: advance the failover chain to the next element on an account-level refusal, the same way it advances on a 429, both before response headers are committed to the client and inside an already committed streaming response
<!-- req: REQ-004 tracks=anthropic-account-refusal-20261022 -->

The system MUST advance the failover chain to the next element on an account-level refusal, the same way it advances on a 429, both before response headers are committed to the client and inside an already committed streaming response.

#### Scenario: When the first upstream of a two-element chain answers 400 `invalid_request_error` with `You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.`, the system shall send the request to the second upstream and return its response.

- WHEN the first upstream of a two-element chain answers 400 `invalid_request_error` with `You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.`
- THEN send the request to the second upstream and return its response

#### Scenario: When an upstream answers 400 with `Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.`, the system shall advance to the next chain element.

- WHEN an upstream answers 400 with `Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.`
- THEN advance to the next chain element

#### Scenario: When an upstream answers 402 `billing_error` with any other message, the system shall advance to the next chain element.

- WHEN an upstream answers 402 `billing_error` with any other message
- THEN advance to the next chain element

#### Scenario: When a committed streaming response's current chain element answers with an account-level refusal, the system shall continue the stream from the next chain element instead of emitting a terminal error event.

- WHEN a committed streaming response's current chain element answers with an account-level refusal
- THEN continue the stream from the next chain element instead of emitting a terminal error event

### Requirement: keep every other 400 and 402 response terminal: relay it to the client immediately and unchanged, without trying another upstream
<!-- req: REQ-005 tracks=anthropic-account-refusal-20261022 -->

The system MUST keep every other 400 and 402 response terminal: relay it to the client immediately and unchanged, without trying another upstream.

#### Scenario: If an upstream answers 400 `invalid_request_error` with a request-validation message such as `messages: Field required`, then the system shall return that 400 to the client without contacting any later chain element.

- WHEN an upstream answers 400 `invalid_request_error` with a request-validation message such as `messages: Field required`
- THEN return that 400 to the client without contacting any later chain element

#### Scenario: If an upstream answers 400 with a message that contains `usage limits` but does not start with one of the FR-1 phrases, then the system shall treat it as terminal.

- WHEN an upstream answers 400 with a message that contains `usage limits` but does not start with one of the FR-1 phrases
- THEN treat it as terminal

### Requirement: skip a credential the gateway owns (an API-key upstream, or one account of a Claude subscription pool) for new requests after it returns an account-level refusal, until the refusal's stated access-resume time for a spend-limit refusal, or for 1 hour for a refusal that states no time
<!-- req: REQ-006 tracks=anthropic-account-refusal-20261022 -->

The system MUST skip a credential the gateway owns (an API-key upstream, or one account of a Claude subscription pool) for new requests after it returns an account-level refusal, until the refusal's stated access-resume time for a spend-limit refusal, or for 1 hour for a refusal that states no time.

#### Scenario: While an API-key upstream is within the skip window set by a spend-limit refusal that stated `2026-11-01 at 00:00 UTC`, the system shall route new requests past it without contacting it.

- GIVEN an API-key upstream is within the skip window set by a spend-limit refusal that stated `2026-11-01 at 00:00 UTC`
- THEN route new requests past it without contacting it

#### Scenario: When the stated access-resume time has passed, the system shall send the next request to that upstream again.

- WHEN the stated access-resume time has passed
- THEN send the next request to that upstream again

#### Scenario: When an upstream returns a credit-balance or `billing_error` refusal, the system shall skip it for 1 hour.

- WHEN an upstream returns a credit-balance or `billing_error` refusal
- THEN skip it for 1 hour

#### Scenario: If a spend-limit refusal's access-resume time cannot be read, then the system shall skip the upstream for 1 hour.

- WHEN a spend-limit refusal's access-resume time cannot be read
- THEN skip the upstream for 1 hour

### Requirement: rotate a Claude subscription pool to its next account on an account-level refusal before the chain moves to the next upstream, consistent with how the pool already handles account-scoped failures
<!-- req: REQ-007 tracks=anthropic-account-refusal-20261022 -->

The system MUST rotate a Claude subscription pool to its next account on an account-level refusal before the chain moves to the next upstream, consistent with how the pool already handles account-scoped failures.

#### Scenario: When one account of a Claude subscription pool returns an account-level refusal, the system shall try the pool's next eligible account before advancing to the next upstream.

- WHEN one account of a Claude subscription pool returns an account-level refusal
- THEN try the pool's next eligible account before advancing to the next upstream

### Requirement: never put a client-supplied credential (passthrough authentication) into the skip state, because the refusal belongs to that client's key and not to the upstream
<!-- req: REQ-008 tracks=anthropic-account-refusal-20261022 -->

The system MUST never put a client-supplied credential (passthrough authentication) into the skip state, because the refusal belongs to that client's key and not to the upstream.

#### Scenario: When a passthrough-authenticated upstream returns an account-level refusal, the system shall advance the chain for that request and shall not skip the upstream for later requests.

- WHEN a passthrough-authenticated upstream returns an account-level refusal
- THEN advance the chain for that request and shall not skip the upstream for later requests

### Requirement: hold the skip state in memory only; a restart clears it
<!-- req: REQ-009 tracks=anthropic-account-refusal-20261022 -->

The system MUST hold the skip state in memory only; a restart clears it.

#### Scenario: When the gateway restarts, the system shall treat every previously skipped credential as eligible.

- WHEN the gateway restarts
- THEN treat every previously skipped credential as eligible

### Requirement: when every element of a chain has refused or is skipped, return an account-level refusal to the client with the upstream's own status and error message intact, unless another remembered failure takes precedence under the existing preference order
<!-- req: REQ-010 tracks=anthropic-account-refusal-20261022 -->

The system MUST when every element of a chain has refused or is skipped, return an account-level refusal to the client with the upstream's own status and error message intact, unless another remembered failure takes precedence under the existing preference order.

#### Scenario: When the only chain element returns a spend-limit refusal, the system shall return HTTP 400 with the upstream's original message.

- WHEN the only chain element returns a spend-limit refusal
- THEN return HTTP 400 with the upstream's original message

### Requirement: update the failover contract and the user-facing failover and troubleshooting documentation, including every maintained translation, to describe the three refusals, the skip durations, and that all other 400s stay terminal
<!-- req: REQ-011 tracks=anthropic-account-refusal-20261022 -->

The system MUST update the failover contract and the user-facing failover and troubleshooting documentation, including every maintained translation, to describe the three refusals, the skip durations, and that all other 400s stay terminal.

### Requirement: try a chain whose every element is in the skip window as configured instead of refusing locally, so a credential that was topped up or raised is noticed before its window ends
<!-- req: REQ-012 tracks=anthropic-account-refusal-20261022 -->

The system MUST try a chain whose every element is in the skip window as configured instead of refusing locally, so a credential that was topped up or raised is noticed before its window ends.

#### Scenario: While every element of a chain is in the skip window, the system shall send the request to the chain's elements in their configured order.

- GIVEN every element of a chain is in the skip window
- THEN send the request to the chain's elements in their configured order

## Non-functional Requirements

### Requirement: read an error body only for 400 and 402 responses, within a fixed size budget, so other statuses keep deciding failover from the status line alone
<!-- req: REQ-013 tracks=anthropic-account-refusal-20261022 -->

The system SHOULD read an error body only for 400 and 402 responses, within a fixed size budget, so other statuses keep deciding failover from the status line alone.

### Requirement: leave successful and non-refusal responses byte-for-byte unchanged, including streaming responses
<!-- req: REQ-014 tracks=anthropic-account-refusal-20261022 -->

The system SHOULD leave successful and non-refusal responses byte-for-byte unchanged, including streaming responses.

### Requirement: log each account-level refusal at warning level with the upstream name, the refusal kind, and the skip expiry, and count the advance in the existing failover metric
<!-- req: REQ-015 tracks=anthropic-account-refusal-20261022 -->

The system SHOULD log each account-level refusal at warning level with the upstream name, the refusal kind, and the skip expiry, and count the advance in the existing failover metric.
