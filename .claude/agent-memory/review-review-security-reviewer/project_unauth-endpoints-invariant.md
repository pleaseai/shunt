---
name: unauth-endpoints-invariant
description: shunt's GET / and GET /health are intentionally unauthenticated; invariant is they expose only status + crate version
metadata:
  type: project
---

`src/server.rs` `build_router` mounts `GET /` (`root_index`) and `GET /health` (`health`) BEFORE the protected `/v1/*` routes. Inbound `[server.auth]` is enforced inside the proxy/discovery handlers, not middleware, so `/` and `/health` bypass auth BY DESIGN.

**Why:** healthcheck/liveness tools usually cannot attach tokens.

**How to apply:** the documented invariant (docs/running.md) is these routes must expose nothing beyond status and crate version. When reviewing changes here, verify both handlers stay fully static — they build responses only from string literals + `env!("CARGO_PKG_VERSION")` (compile-time constant), with zero request input, config, or secrets interpolated. Flag only if a change starts leaking config/credentials/upstream details or reflects request input. The auth exemption itself is not a finding.
