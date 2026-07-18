---
name: codex-quota-stable-identity
description: Codex quota recording must receive AccountConfig and key health by account_identity
metadata:
  type: project
---
Codex quota headers are recorded through `AccountPool::note_codex_quota` with the full `AccountConfig`, so UUID-backed accounts use the same stable identity as snapshot/health. HTTP, inbound, pooled HTTP, and websocket handshake paths must preserve the AccountConfig; the account name remains only for response headers and websocket pool keys.

**Why:** Keying by account name orphaned quota state whenever a Codex account had a UUID different from its configured name.

**How to apply:** When adding or changing Codex quota call sites, pass the selected account config rather than deriving an identity string.
