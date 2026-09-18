# Weekly quota fallback for Messages

The optional `server.weekly_fallback` policy keeps a Messages request on its selected provider while any selected account has weekly quota.
It changes providers only when every enabled physical account has fresh evidence of shared weekly exhaustion.
The policy supports both directions between Claude OAuth and ChatGPT OAuth through `/v1/messages`.
The inbound Codex endpoint and `count_tokens` retain their existing behavior.

## Configuration

Bind the policy to existing provider names. Use `claude_oauth` for the Claude provider and `chatgpt_oauth` for the Codex provider.
The Codex provider must use the ChatGPT backend.
The table is optional. An absent table or `enabled = false` preserves the existing route behavior.

```toml
[server.weekly_fallback]
enabled = true
claude_provider = "anthropic"
codex_provider = "codex"

[[server.weekly_fallback.models]]
claude = "claude-fable-5"
codex = "gpt-6-astra"
claude_fallback = "claude-opus-5"

[[server.weekly_fallback.models]]
claude = "claude-opus-5"
codex = "gpt-5.6-sol"

[[server.weekly_fallback.models]]
claude = "claude-sonnet-5"
codex = "gpt-5.6-terra"

[[server.weekly_fallback.models]]
claude = "claude-haiku-4-5-20251001"
codex = "gpt-5.6-luna"
```

These are explicit backend IDs. Confirm that the selected accounts support them before activation.
Add another pair for an additional versioned ID. Use existing routes or model mappings for client aliases.
The existing routing layer removes a trailing `[1m]` or `[1M]` hint before the policy matches the route.
The policy matches the resolved provider and backend ID exactly.
It does not infer model families from names.

The policy applies only to a resolved chain with one route.
Configuration validation rejects generic model chains that contain either bound provider when this policy is enabled.
Unrelated chains retain the existing [ordered failover behavior](upstreams-failover.md).

## Evidence and recovery

The policy uses observations from the current process. It does not restore strict evidence from persisted quota state.
Each observation must contain a future reset and either shared rejection status or complete utilization.
The reset must belong to that observation and fall within its weekly window.
Header observations and usage observations expire after 300 seconds.

Anthropic headers use ratios. Codex headers use percentages. Usage snapshots contain normalized ratios.
Malformed, incomplete, contradictory, expired, or reset-less observations cannot prove exhaustion.

The policy uses the adapters' account scope, credential store, and physical identity rules.
An empty pool, an all-disabled pool, or an unknown account prevents a claim of complete exhaustion.
Five-hour limits, Fable-only limits, cooldowns, soft thresholds, and aggregate pool status do not prove shared weekly exhaustion.
A fresh observation with remaining quota removes prior exhaustion evidence.

The usage parsers retain strict validity metadata separately from ordinary quota snapshots.
The strict Claude usage path validates the complete reset timestamp, including fractional digits, calendar dates, and timezone offsets.
It treats leap-second resets as unknown.
An invalid reported weekly value replaces previous strict evidence, even when the ordinary snapshot is empty.
Codex reports with contradictory weekly windows or unknown durations also replace prior strict evidence with unknown evidence.
A Claude report without the shared weekly key leaves strict evidence unchanged.
A Codex report that authoritatively omits it clears strict evidence.
The legacy quota display, persistence, and account-poll selection retain their existing behavior.

Existing usage polling supplies observations for eligible accounts when the operator enables it.
The fallback policy does not start an additional poller.

## Request behavior

The request first uses its ordinary resolved route.
If that pool has complete exhaustion evidence, shunt checks the paired provider before dispatch.
Otherwise, the adapter retains its existing account selection and rotation.
A failed attempt can record new quota evidence, so shunt checks that evidence again within the same request.
If both pools have complete exhaustion evidence, shunt returns HTTP 429 with an Anthropic `rate_limit_error`.

Unknown destination quota permits an attempt. A destination credential error retains the adapter's existing error behavior.

For a pair with `claude_fallback`, eligible Claude failures use that backend on the same provider.
Eligible failures include upstream 4xx, upstream 5xx, and transport failure before response headers.
This includes an upstream 400 model rejection.

Request validation and authorization errors remain terminal.
A pool that never attempts an upstream request also remains terminal under this policy.
A quota update during local credential lookup does not change this terminal result.
Initial HTTP and WebSocket request construction failures do not count as upstream attempts.
An earlier connection attempt or response retains its evidence across a later local failure.
Redirect failures retain the evidence from the earlier HTTP request.
Failures after successful headers remain terminal.

The fallback uses a fresh route, so Opus does not inherit Fable's account scope for cooldowns.
If Opus later exposes shared weekly exhaustion, the original Fable pair still selects Astra.

The request permits at most one provider change and never returns to the original provider.
A Codex request that changes to Fable can also use the configured Opus fallback.

Shunt returns a successful response immediately. It does not replay a request after successful headers or partial output.
The existing adapters retain stream conversion and tool conversion.
A provider change uses the destination provider's defaults and preserves the original request body.
A same-provider fallback retains the route's effort and service tier.
The failed attempt releases admission before the destination acquires its own admission.

## Authentication and response metadata

Shunt authenticates all potential routes before account resolution or quota decisions.
The managed model guard checks the original requested alias.
The operator's fallback configuration defines alternative implementations of that alias, as existing `upstream_model` mappings already do.
The destination uses its own credentials through the existing adapter path.

| Header | Value |
| --- | --- |
| `x-gateway-upstream` | Final provider name |
| `x-gateway-model` | Raw requested model or alias, including its context hint |
| `x-gateway-upstream-model` | Actual backend model for the final attempt |

## Activation and rollback

Prepare the binary and configuration separately before activation.
Keep the previous binary and a copy of the active configuration for rollback.
Set `enabled = false` or remove the table to restore the existing policy.
This feature does not require a credential migration or a persisted state migration.
