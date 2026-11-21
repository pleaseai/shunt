---
name: inbound-auth-architecture
description: How shunt's inbound credential gate, header stripping and WS re-auth fit together (verified 2026-10-30 for track inbound-jwt-header-cookie)
metadata:
  type: project
---

Inbound credential flow: `auth::gate::authenticate(auth, jwks, headers, Slots)` is the single gate. `Slots::Codex` is used only by the Codex endpoint (`codex_endpoint::authenticate_inbound`, `codex_analytics::post`); `Slots::Client` by discovery/usage/oauth_usage/failover.

Forward-side stripping is centralised in `auth::slots::ShuntCredentials::from_state(..).strip_reserved_slots` (RESERVED_SLOTS includes `cookie`). WS turns capture auth/admin header names at upgrade (`websocket.rs` get -> `TurnContext`) because a reload can rename them before the turn forwards.

WS per-turn re-auth reuses the handshake headers (clone), so a header/cookie credential is re-verified each turn and expires mid-connection like a bearer JWT.

No middleware (concurrency limiter, http_tuning) drops `cookie`; there is no TraceLayer/compression layer; no code Debug-logs a HeaderMap.

`InboundAuthConfig` implements `Default` (src/config.rs), but it is still built as a struct literal in many sites; clippy `needless_update` rejects `..Default::default()` on a literal that already names every field.

**Why:** saves re-deriving this when touching auth or header-forwarding.
**How to apply:** re-verify file:line before relying on it; code moves.
