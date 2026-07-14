---
name: m11-inbound-codex-endpoint-security
description: PR #125 inbound Codex /responses passthrough security posture — credential strip verified safe; one response-header denylist gap (set-cookie).
metadata:
  type: project
---

PR #125 (`amondnet/codex-endpoint`) adds an inbound OpenAI Responses "Codex" passthrough endpoint (`[server.codex_endpoint]`, 3 routes: `/backend-api/codex/responses`, `/responses`, `/v1/responses`) that injects a server-side ChatGPT/Codex OAuth pool bearer. Reviewed `src/adapters/responses/mod.rs`, `src/codex_endpoint.rs`, `src/auth/inbound.rs`, `src/config.rs`, `src/server.rs`.

**Verified SAFE:**
- **Upstream credential strip** (`passthrough_request_headers`): `PASSTHROUGH_STRIP_REQUEST_HEADERS` always strips `authorization` + `chatgpt-account-id` (lowercase consts; `http` HeaderName is lowercase-normalized so `name.as_str()` matches client casing). Then `passthrough_send` re-injects the pool account's bearer+account-id — since the client's copies are already stripped, no duplicate/override reaches upstream. The shunt client-token header is stripped case-insensitively (`token_header.eq_ignore_ascii_case`), and the Bearer form of the shunt token rides `authorization` which is also stripped. Client's own credential never reaches the ChatGPT backend.
- **Inbound auth** (`InboundAuth::authenticate_bearer`): constant-time compare over all tokens, no early exit (timing doesn't reveal which matched); `bearer_token` parsing is sound (no prefix bypass). Open-when-`[server.auth]`-absent is BY DESIGN and consistent with `proxy::check_inbound_auth` (both no-op when `inbound_auth` is None). Client cannot auth with the pool/ChatGPT credential (inbound tokens are a separate operator-configured set).
- **SSRF/path**: `responses_url` builds the URL from the operator-configured, config-validated provider `base_url` (chatgpt_oauth held to ChatGPT host over https); provider is pinned by `codex_endpoint.provider`, NOT client-controlled. Body `model` is a metrics label only, never routes. 3 routes are fixed paths.
- Inbound passthrough relays verbatim and never calls `mapped_upstream_error`/`build_upstream_error`, so it does NOT log upstream request/response bodies (unlike the outbound translating path at mod.rs:1721 / :116).

**GAP (low sev, defense-in-depth):** `relay_passthrough` forwards upstream response headers via a DENYLIST (`PASSTHROUGH_STRIP_RESPONSE_HEADERS` = framing/hop-by-hop + content-encoding only). `set-cookie` is NOT stripped, so Cloudflare (`__cf_bm`/`cf_clearance`) or any account/session-affinity cookie the chatgpt.com edge sets is relayed verbatim to the untrusted/multi-tenant inbound client. Recommend allowlist or explicit `set-cookie` strip.

**Note:** client-supplied `x-shunt-inbound-client` is NOT stripped on the inbound path (the main proxy path removes it at proxy.rs:220 to prevent identity spoofing), but the inbound codex path uses only `session-id` for the pool key, so a spoofed value has no security effect here — harmless junk header forwarded upstream.
