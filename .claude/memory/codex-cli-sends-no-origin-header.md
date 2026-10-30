---
name: codex-cli-sends-no-origin-header
description: The Codex CLI sets no Origin header on its own HTTP or WebSocket requests, so an Origin-based guard does not block it
type: reference
source: "openai/codex main, grep.app search 2026-10-30; recorded in track inbound-jwt-header-cookie-20261030 (PR #862)"
---

# The Codex CLI sends no `Origin` header

In `openai/codex` (`main`, checked 2026-10-30), `ORIGIN` / `header::ORIGIN` appears only in
server-side middleware that rejects requests carrying it, and in tests. The CLI's own HTTP
and WebSocket clients (`connect_async` call sites included) never set it.

This is why shunt can gate an ambient credential, such as the `[server.auth] jwt_cookie` slot,
on "no `Origin`, or one matching `Host`" (FR-12 of the inbound JWT track) without blocking the
intended Codex client: a browser driven by another site sends `Origin`, the Codex CLI does not.

The evidence is source-level only. A real Codex CLI `gateway_oauth` run against shunt is
still the end-to-end confirmation; re-check this if a Codex release starts reporting 401 on
cookie delivery.

Related: [[codex-endpoint-reads-headers-before-credential-strip]]
