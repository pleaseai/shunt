# Memory index

- [Unauth endpoints invariant](project_unauth-endpoints-invariant.md) — GET / and GET /health bypass auth by design; must expose only status + crate version.
- [Sentry PII egress](project_sentry-pii-egress.md) — before_send only strips server_name; warn!/info! breadcrumbs (upstream_error_body, client names) leak request data on panic.
- [Sentry transaction hostname leak](project_sentry-transaction-hostname-leak.md) — perf-tracing transactions bypass before_send/scrub_event (no before_send_transaction in 0.48.4); fixed by pinning `server_name` to empty before context integration can auto-fill the hostname.
- [OTel PII egress](project_otel-pii-egress.md) — OTel export has no Sentry-style scrubbing: dead include_session_id flag leaks session_id on spans; logs bridge exports upstream_error_body + client names.
- [Codex WS pool isolation](project_codex-ws-pool-isolation.md) — WS v2 conn pool keyed only on client-supplied x-claude-code-session-id, not the authenticated inbound client.
