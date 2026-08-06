---
name: shunt-sentry-error-display-url-leak
description: reqwest::Error Display embeds the request URL (host, query, sometimes creds) — any place that forwards an error's to_string()/source() chain to Sentry as free text can leak it despite control-char/length sanitizers
metadata:
  type: project
---

#310 (`src/stream_metrics.rs::error_chain`, called from `ObservedStream::poll_next` around
line 571) renders a body-read `axum::Error`'s `Display` plus up to
`MAX_ERROR_SOURCES` `source()` messages into one string, which
`src/observability.rs::record_stream_failure` exports as the Sentry `upstream_error` extra
after only `sanitize_tag` (control-char strip + length cap — no URL/host redaction).

**Why it matters:** `reqwest::Error`'s `Display` includes `for url (...)`, so a transport
error's message chain can carry the full upstream request URL — host, path, query params,
and in some misconfigurations query-string credentials. `docs/running.md:139` explicitly
documents "credentials, and the host name are never sent" for Sentry mid-stream events, so
this is a doc/behavior contradiction, not just a style nit. Confirmed live in cubic review
(`P1`, run via `cubic review -j -b origin/main`, commit `2d67f01`) and by reading
`sanitize_tag` in `src/observability.rs` (only strips control chars + truncates).

**How to apply:** any future observability code that stringifies an upstream `reqwest`/`axum`
error chain for export (Sentry extras, OTEL log bodies, breadcrumbs) needs URL/host
redaction, not just the existing `sanitize_tag` control-char/length sanitizer, to actually
satisfy the "no host name" guarantee in `docs/running.md`. Left unfixed pending a maintainer
decision — redacting a URL substring is a design choice (strip whole `for url (...)` clause?
keep scheme+path, drop host/query?) rather than a mechanical fix, so this agent flagged it
instead of auto-patching.
