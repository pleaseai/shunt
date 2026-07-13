# Memory index

- [Unauth endpoints invariant](project_unauth-endpoints-invariant.md) — GET / and GET /health bypass auth by design; must expose only status + crate version.
- [Sentry PII egress](project_sentry-pii-egress.md) — before_send only strips server_name; warn!/info! breadcrumbs (upstream_error_body, client names) leak request data on panic.
- [OTel PII egress](project_otel-pii-egress.md) — OTel export has no Sentry-style scrubbing: dead include_session_id flag leaks session_id on spans; logs bridge exports upstream_error_body + client names.
- [Codex WS pool isolation](project_codex-ws-pool-isolation.md) — WS v2 conn pool keyed only on client-supplied x-claude-code-session-id, not the authenticated inbound client.
- [Token file writers](project_token-file-writers.md) — two credential-file writers; claude_auth's chmod-after-write leaves a world-readable window (vs codex_auth's born-private). Plus verified-safe list for the multi-account path.
- [Claude token URL egress](project_claude-token-url-egress.md) — SHUNT_CLAUDE_TOKEN_URL override sends refresh_token to any host with no anthropic/https guard (base_url IS guarded); env-gated, minor.
