---
name: codex-endpoint-reads-headers-before-credential-strip
description: The inbound Codex endpoint reads session-id headers before the credential strip and rewrites WebSocket handshake headers before per-turn re-auth, so an operator-named credential header must avoid both
type: gotcha
source: "PR #862 (track inbound-jwt-header-cookie-20261030), fix 7c2333c9"
---

# The Codex endpoint reads and rewrites headers outside the credential strip

Stripping a credential header at the single forward strip point
(`ShuntCredentials::from_state` / `strip_reserved_slots`) is not enough on the inbound
Codex endpoint, because two other code paths touch request headers first:

- **Session id extraction.** `extract_session_id` (`src/codex_endpoint.rs`) is called on the
  raw request headers in `serve` (`src/codex_endpoint.rs:103`) and at the WebSocket upgrade
  (`src/codex_endpoint/websocket.rs:98`), before any credential is stripped. It reads
  `session-id`, then `x-claude-code-session-id`. The extracted value goes into the request span
  and, on routed OpenAI api-key routes, into `session_affinity_headers`
  (`src/adapters/responses/request.rs`), which synthesizes upstream `session-id`, `thread-id`,
  `x-client-request-id` and `x-codex-window-id` from it.
- **WebSocket turn headers.** For each turn the bridge clones the handshake headers, overwrites
  `content-type`, removes `content-length`, `content-encoding`, `upgrade`, `connection` and the
  `sec-websocket-*` headers, and rewrites `openai-beta` (`src/codex_endpoint/websocket.rs`,
  around lines 244-255). `run_turn` then re-runs `authenticate_inbound` on that rewritten map.

Observed consequence: with `[server.auth] jwt_header = "session-id"` the IdP JWT became the
session id and reached telemetry and synthesized upstream headers, even though the header
itself was stripped. With `jwt_header = "openai-beta"` the WebSocket upgrade authenticated but
every turn failed with 401. Both were found by the ensemble review of PR #862 and fixed by
rejecting those names at startup (`CODEX_PATH_HEADERS` in `src/auth/jwt_slots.rs`).

When adding another operator-named header slot on this endpoint, check the name against these
reads and rewrites as well as against the credential slots. `CODEX_PATH_HEADERS` is maintained
by hand (TD-023), so a new read or rewrite here also has to be added to that list.

Related: [[codex-cli-sends-no-origin-header]]
