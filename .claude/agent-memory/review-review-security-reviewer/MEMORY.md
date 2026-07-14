# Memory index

- [Unauth endpoints invariant](project_unauth-endpoints-invariant.md) — GET / and GET /health bypass auth by design; must expose only status + crate version.
- [Sentry PII egress](project_sentry-pii-egress.md) — before_send only strips server_name; warn!/info! breadcrumbs (upstream_error_body, client names) leak request data on panic.
- [Sentry transaction hostname leak](project_sentry-transaction-hostname-leak.md) — perf-tracing transactions bypass before_send/scrub_event (no before_send_transaction in 0.48.4); fixed by pinning `server_name` to empty before context integration can auto-fill the hostname.
- [OTel PII egress](project_otel-pii-egress.md) — OTel export has no Sentry-style scrubbing: dead include_session_id flag leaks session_id on spans; logs bridge exports upstream_error_body + client names.
- [Codex WS pool isolation](project_codex-ws-pool-isolation.md) — WS v2 conn pool keyed only on client-supplied x-claude-code-session-id, not the authenticated inbound client.
- [Token file writers](project_token-file-writers.md) — two credential-file writers; claude_auth's chmod-after-write leaves a world-readable window (vs codex_auth's born-private). Plus verified-safe list for the multi-account path.
- [Claude token URL egress](project_claude-token-url-egress.md) — SHUNT_CLAUDE_TOKEN_URL override sends refresh_token to any host with no anthropic/https guard (base_url IS guarded); env-gated, minor.
- [M9 admin surface security](project_m9-admin-surface-security.md) — src/admin/ posture: CSRF triple-layered + traversal-safe + no secret leak; gaps = no /admin/login rate-limit, Host-derived Secure flag, no security headers.
- [Codex multi-account security](project_codex-multi-account-security.md) — PR #114 posture: bearer/WS-isolation/perms all verified safe; one gap = chatgpt_oauth missing the kind!=responses guard its siblings have.
- [Retry/backoff security](project_retry-backoff-security.md) — issue #48 src/retry.rs posture: storm-bound/Retry-After-cap/no-token-logs/no-wrong-host all verified safe; re-open a vector only if the cap, ExceedsBudget, log fields, or per-attempt base_url change.
