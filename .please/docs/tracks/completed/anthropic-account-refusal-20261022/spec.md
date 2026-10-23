---
product_spec_domain: routing/failover
---

# Fail over on Anthropic account-level refusals

> Track: anthropic-account-refusal-20261022

## Overview

An operator who orders upstreams as "Anthropic API key first, then another API key, then the Claude subscription pool, then Codex" expects the chain to move on when the first key runs out of money. Today it does not. Anthropic answers an exhausted spend limit or credit balance with HTTP 400 (or 402), and the failover chain treats every 400 as a request error that no other upstream can fix, so the client receives the refusal while the later upstreams sit unused (#817).

These refusals describe the credential, not the request: another credential can serve the same body. This track makes the gateway recognise exactly three account-level refusals from Anthropic-protocol upstreams, advance the chain past them, and skip the refusing credential for a while so later requests do not pay for the same refusal again. Every other 400 stays terminal, which keeps the reason the rule exists: a malformed request is not resent to every upstream.

## Requirements

### Functional Requirements

- [ ] FR-1: classify as an account-level refusal an Anthropic-protocol upstream response with HTTP 400 whose error message starts with `You have reached your specified API usage limits` or `You have reached your specified workspace API usage limits` (a spend limit the account owner set)
- [ ] FR-2: classify as an account-level refusal an Anthropic-protocol upstream response with HTTP 400 or 402 whose error message starts with `Your credit balance is too low to access the Anthropic API` (prepaid or plan-included credit exhausted)
- [ ] FR-3: classify as an account-level refusal any Anthropic-protocol upstream response with HTTP 402 and error type `billing_error`
- [ ] FR-4: advance the failover chain to the next element on an account-level refusal, the same way it advances on a 429, both before response headers are committed to the client and inside an already committed streaming response
- [ ] FR-5: keep every other 400 and 402 response terminal: relay it to the client immediately and unchanged, without trying another upstream
- [ ] FR-6: skip a credential the gateway owns (an API-key upstream, or one account of a Claude subscription pool) for new requests after it returns an account-level refusal, until the refusal's stated access-resume time for a spend-limit refusal, or for 1 hour for a refusal that states no time
- [ ] FR-7: rotate a Claude subscription pool to its next account on an account-level refusal before the chain moves to the next upstream, consistent with how the pool already handles account-scoped failures
- [ ] FR-8: never put a client-supplied credential (passthrough authentication) into the skip state, because the refusal belongs to that client's key and not to the upstream
- [ ] FR-9: hold the skip state in memory only; a restart clears it
- [ ] FR-10: when every element of a chain has refused or is skipped, return an account-level refusal to the client with the upstream's own status and error message intact, unless another remembered failure takes precedence under the existing preference order
- [ ] FR-11: update the failover contract and the user-facing failover and troubleshooting documentation, including every maintained translation, to describe the three refusals, the skip durations, and that all other 400s stay terminal
- [ ] FR-12: try a chain whose every element is in the skip window as configured instead of refusing locally, so a credential that was topped up or raised is noticed before its window ends

### Non-functional Requirements

- [ ] NFR-1: read an error body only for 400 and 402 responses, within a fixed size budget, so other statuses keep deciding failover from the status line alone
- [ ] NFR-2: leave successful and non-refusal responses byte-for-byte unchanged, including streaming responses
- [ ] NFR-3: log each account-level refusal at warning level with the upstream name, the refusal kind, and the skip expiry, and count the advance in the existing failover metric

## Acceptance Criteria

- [ ] AC-001 (FR-1, FR-4): When the first upstream of a two-element chain answers 400 `invalid_request_error` with `You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.`, the system shall send the request to the second upstream and return its response.
- [ ] AC-002 (FR-1): When an upstream answers 400 with `You have reached your specified workspace API usage limits`, the system shall classify it as an account-level refusal.
- [ ] AC-003 (FR-2, FR-4): When an upstream answers 400 with `Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.`, the system shall advance to the next chain element.
- [ ] AC-004 (FR-2): When an upstream answers 402 `billing_error` with the credit-balance message, the system shall classify it as an account-level refusal.
- [ ] AC-005 (FR-3, FR-4): When an upstream answers 402 `billing_error` with any other message, the system shall advance to the next chain element.
- [ ] AC-006 (FR-5): If an upstream answers 400 `invalid_request_error` with a request-validation message such as `messages: Field required`, then the system shall return that 400 to the client without contacting any later chain element.
- [ ] AC-007 (FR-5): If an upstream answers 400 with a message that contains `usage limits` but does not start with one of the FR-1 phrases, then the system shall treat it as terminal.
- [ ] AC-008 (FR-4): When a committed streaming response's current chain element answers with an account-level refusal, the system shall continue the stream from the next chain element instead of emitting a terminal error event.
- [ ] AC-009 (FR-6): While an API-key upstream is within the skip window set by a spend-limit refusal that stated `2026-11-01 at 00:00 UTC`, the system shall route new requests past it without contacting it.
- [ ] AC-010 (FR-6): When the stated access-resume time has passed, the system shall send the next request to that upstream again.
- [ ] AC-011 (FR-6): When an upstream returns a credit-balance or `billing_error` refusal, the system shall skip it for 1 hour.
- [ ] AC-012 (FR-6): If a spend-limit refusal's access-resume time cannot be read, then the system shall skip the upstream for 1 hour.
- [ ] AC-013 (FR-7): When one account of a Claude subscription pool returns an account-level refusal, the system shall try the pool's next eligible account before advancing to the next upstream.
- [ ] AC-014 (FR-8): When a passthrough-authenticated upstream returns an account-level refusal, the system shall advance the chain for that request and shall not skip the upstream for later requests.
- [ ] AC-015 (FR-9): When the gateway restarts, the system shall treat every previously skipped credential as eligible.
- [ ] AC-016 (FR-10): When the only chain element returns a spend-limit refusal, the system shall return HTTP 400 with the upstream's original message.
- [ ] AC-017 (NFR-1): When an upstream answers 429 or 5xx, the system shall decide to advance without reading the error body.
- [ ] AC-018 (FR-12): While every element of a chain is in the skip window, the system shall send the request to the chain's elements in their configured order.

## Out of Scope

- Removing or rewriting the `anthropic-beta: claude-code-20250219` token, or any other change aimed at how Anthropic bills Claude Code traffic against plan-included API credits (decided in #817).
- Checking spend or credit ahead of a request. A plain API key cannot read usage, cost, or spend limits; the Usage and Cost API needs Admin API credentials.
- Persisting the skip state across restarts.
- Account-level refusals from non-Anthropic protocols (OpenAI Responses, Gemini, Cursor); the existing Codex `model is not supported` rule is unchanged.
- The tier monthly spend cap: it already arrives as a 429 and already fails over.
- Making the skip duration configurable.

## Assumptions

- The refusal phrases and statuses come from Anthropic's documentation as read on 2026-10-22 (rate limits, errors, and API credits pages). Third-party reports that the credit-balance refusal sometimes arrives as `billing_error` are the reason FR-2 accepts 402 too.
- Matching the start of the message (not a substring) is enough to separate these refusals from request-validation 400s; Anthropic's validation messages start with a field path or a parameter name.
- FR-12 mirrors the subscription pool's existing all-cooled fallback.
- The skip is per credential, which assumes a refusal applies to every request on that credential. A third-party report (decolua/9router#4739) says the credit-balance refusal can depend on the request: plan-included credit refuses Claude Code traffic while it still serves other clients. When one upstream serves both kinds of client, a Claude Code request can therefore put it in the 1-hour skip and send the other clients' requests elsewhere for that hour. This track accepts that trade-off; request-class-aware skipping is not attempted.
- An account-level refusal ranks below 429, 401/403, 404, and 5xx in the existing remembered-failure preference, so a more actionable failure from a later element wins when the chain is exhausted.
- "Anthropic-protocol upstream" covers every upstream served by the Anthropic adapter, including Anthropic-compatible third-party hosts; the exact Anthropic phrases make a false match on those hosts unlikely.
