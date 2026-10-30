---
name: inbound-auth-credential-slot-map
description: Where shunt accepts/strips inbound credentials (gate.rs, slots.rs, config.rs InboundAuthConfig) and the Codex endpoint auth path
metadata:
  type: project
---

Single strip wiring point: `ShuntCredentials::from_state` + `strip_reserved_slots` in src/auth/slots.rs; every forward site (Codex passthrough inbound.rs, routed inbound_routed.rs, proxy/failover.rs check_inbound_auth, routing/serve.rs judge_headers) calls it. discovery/upstream builds a fresh map (only shared slots), so never copies custom headers. `cookie` already in RESERVED_SLOTS.

Codex endpoint auth: all HTTP (Responses + alpha search via `serve`->`forward`), and WS upgrade + per-turn re-auth (websocket.rs:67, turn.rs:107) call `codex_endpoint::authenticate_inbound` (codex_endpoint.rs:333) -> `gate::authenticate(.., Slots::Codex)`. Analytics has its own copy (codex_analytics.rs:66).

`InboundAuthConfig` implements `Default` (src/config.rs), but many construction sites are struct literals; a new field needs `..Default::default()` or an explicit value at each of them.

Only cookie parser: `admin::session_cookie` (src/admin/mod.rs:527), first `cookie` field only, no dup detection.

Verify against current code before relying (snapshot 2026-10-30).
