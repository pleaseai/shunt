---
name: shunt-gateway-claims-scope-rot
description: In src/proxy/failover.rs, `InboundContext.gateway_claims` is populated before the auth gate's early return, so comments scoping a gateway-JWT behavior to "a gated chain" over-narrow what the code does; also `injects_credential` means different things in check_inbound_auth (chain-level) vs headers_for_route (per-route).
metadata:
  type: project
---

Two recurring comment-rot traps in `src/proxy/failover.rs` (seen in PR #355, `fix/gateway-jwt-credential-slots`):

1. `check_inbound_auth` computes `gateway_claims` *before* the `if !injects_credential || (no auth configured)` early return. So `inbound.gateway_claims.is_some()` is true for any caller holding a valid gateway JWT — including an all-passthrough chain where nothing was gated. Any comment or doc sentence phrased as "whenever a gateway JWT **authenticated** the request" or "on the passthrough attempt of a **gated** chain" is narrower than the code.

2. `injects_credential` is a local name used twice with different meanings: chain-level (`routes.iter().any(...)`) in `check_inbound_auth`, per-route (`!is_passthrough_route(state, route)`) in `headers_for_route`. Comments inside `headers_for_route` that assert "`injects_credential` is chain-level" contradict the binding 20 lines above them.

**Why:** both produce comments that read correct in isolation but assert the wrong scope for the reader standing in that function.

**How to apply:** when reviewing comments about gateway-JWT credential stripping, re-read `check_inbound_auth` top-to-bottom for the early return, and check which `injects_credential` binding is in scope. Related: [[unfalsifiable-doc-claims]] style over-scoped claims.
