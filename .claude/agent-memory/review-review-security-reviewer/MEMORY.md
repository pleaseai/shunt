# Memory index

- [Unauth endpoints invariant](project_unauth-endpoints-invariant.md) — GET / and GET /health bypass auth by design; must expose only status + crate version.
- [Sentry PII egress](project_sentry-pii-egress.md) — before_send only strips server_name; warn!/info! breadcrumbs (upstream_error_body, client names) leak request data on panic.
