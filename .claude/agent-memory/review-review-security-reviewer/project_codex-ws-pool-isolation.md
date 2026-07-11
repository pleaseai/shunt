---
name: codex-ws-pool-isolation
description: Codex WS v2 connection pool (issue #32) is keyed only on client-supplied x-claude-code-session-id, not bound to the authenticated inbound client.
metadata:
  type: project
---

The Codex WebSocket v2 transport (`src/adapters/codex_ws.rs`, opt-in `websocket=true`, gated to `auth==ChatgptOauth`) pools live upstream connections in a process-global map keyed **only** on the client-supplied `x-claude-code-session-id` header (extracted in `src/adapters/responses.rs` `route`/`forward`). The pool key does NOT incorporate the authenticated inbound client name.

**Why it matters:** shunt supports multiple inbound identities via `[server.auth]` `name:token` pairs (`src/auth/inbound.rs`), all sharing ONE server-side upstream credential (`~/.codex/auth.json`). Two inbound clients that present the same session-id share the pooled connection AND its `StoredContinuation` (prior transcript + `previous_response_id`). Cross-client context extraction is throttled by `codex_continuation::decide()` requiring a normalized transcript-prefix match (attacker must already know the victim's transcript), so it is an isolation-boundary defect more than a turnkey exploit. No upstream-credential escalation (credential is shared by design). No token logging found; ws_url is config-derived (no SSRF).

**How to apply:** if re-reviewing this area, recommend namespacing the pool key by the authenticated inbound client name. Related: [[project_sentry-pii-egress]].
