---
name: m4-inbound-client-auth-security
description: PR #130 M4 inbound client-auth (authenticate_client 3-slot gate) security posture — all verified safe; one residual = passthrough Bearer/x-api-key gate-token egress in mixed deployments.
metadata:
  type: project
---

PR #130 (branch amondnet/130) extends M4 inbound auth: gated `/v1/messages`
routes (provider `auth != Passthrough`) now accept the gate token via three
slots — `x-api-key`, `Authorization: Bearer`, and the dedicated header
(`x-shunt-token`) — not just the dedicated header. `authenticate_discovery` →
`authenticate_client` (src/auth/inbound.rs); `check_inbound_auth` (src/proxy.rs)
switched from `authenticate()` to `authenticate_client()`.

**Verified safe:**
- No auth bypass: `authenticate_values` has no early-exit, keeps last match; any
  valid token in any slot authenticates; gated routes require a match → else 401.
- Priority dedicated>Bearer>x-api-key via chain order + keep-last-match. Two
  valid tokens = both authorized, attribution to highest slot (not a bypass).
- Credential strip: on gated success `forwarded.remove("authorization")` +
  `remove("x-api-key")`; dedicated header always removed (both paths). Failure →
  401, no forward. Gate token cannot travel upstream on gated routes.
- Constant-time: all slots go through `constant_time_eq` (pre-existing, folds
  length), no per-slot early-exit.
- No header/log injection: `x-shunt-inbound-client` stripped from client input
  then set from config client *name*; logs use config values only; tokens never
  logged.
- Gate covers all injected modes: only `AuthMode::Passthrough` is ungated;
  ApiKey/ChatgptOauth/ClaudeOauth/XaiOauth/CursorOauth all gated.

**Residual (documented, operator-mitigated — not a code bug):** widening the
accepted slots means a client using the gate token as its `ANTHROPIC_AUTH_TOKEN`
(Bearer) or `x-api-key` will leak that token upstream on **passthrough** routes,
which forward those headers verbatim by design. Only bites mixed
passthrough+mapped deployments. docs/m4-inbound-auth.md §2 + shared-gateway.md
spell out the mitigation: hand out dedicated `x-shunt-token` values when mixing.
Consistent with the project's egress-residual tracking (see
[[project_claude-token-url-egress]]). Cannot be fixed in code without breaking
passthrough (gateway can't tell a gate token from a real upstream credential on
a passthrough route).
