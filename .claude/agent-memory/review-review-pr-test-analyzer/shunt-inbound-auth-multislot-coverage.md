---
name: shunt-inbound-auth-multislot-coverage
description: PR #133 tests/inbound_auth.rs + src/auth/inbound.rs gap analysis for the multi-slot (x-shunt-token/Bearer/x-api-key) gate-token change; the recurring "priority test forgets the leak assertion" pattern.
metadata:
  type: project
---

PR #133 widened the `/v1/messages` inference gate (`check_inbound_auth` in
`src/proxy.rs`) to accept the gate token via three slots — the dedicated
header, `Authorization: Bearer`, or `x-api-key` — via a renamed
`InboundAuth::authenticate_client` (was `authenticate_discovery`), with
priority dedicated-header > Bearer > x-api-key. `check_inbound_auth` now also
explicitly strips `authorization`/`x-api-key` on gated routes as defense in
depth, on top of what the adapters already do.

**Why:** documented "gate tokens must never leak upstream" boundary
(docs/m4-inbound-auth.md) is meant to hold independent of adapter behavior —
`AnthropicAdapter::outbound_headers` (src/adapters/anthropic/mod.rs ~642) and
`ResponsesAdapter` (never forwards raw client headers at all) already
happen to guarantee this for every credential type currently wired up, so
the new strip lines in `check_inbound_auth` are currently unreachable via any
test's observable upstream request — testing at the HTTP-mock level can't
distinguish "check_inbound_auth stripped it" from "the adapter stripped it
anyway." Not itself a bug; just means the safety net is unverified in
isolation given current adapters.

**Recurring pattern found here:** the single-slot acceptance test
(`mapped_route_accepts_the_gate_token_via_bearer_and_x_api_key`) pairs
`NoHeader("x-shunt-token")` / `NoHeader("x-api-key")` / an exact-value
`header("authorization", "Bearer upstream-key")` with the success assertion —
strong. But the *priority/attribution* test added right after it
(`mapped_route_attributes_to_the_dedicated_header_over_bearer_and_api_key`,
tests/inbound_auth.rs:307) — which is the one scenario where TWO different
clients' live credentials coexist on the same request — only asserts
`header("x-shunt-inbound-client", "alice")` and drops the NoHeader/exact-value
checks entirely. Coverage gap: nothing asserts the losing credential (bob's
tok-b) doesn't leak upstream when a second valid-but-wrong credential is
present. **Check on future PRs:** whenever a PR adds a "priority among
multiple credential slots" test, verify it keeps the same NoHeader/exact-value
upstream assertions as the single-slot version — priority tests tend to
downgrade to attribution-only assertions and silently drop the leak check.
See [[shunt-multi-account-failover-coverage]] for the same
test-naming-vs-assertion-consistency family of issue.

Confirmed via `cargo test --test inbound_auth` — 13/13 pass on
`amondnet/130` at review time.
