---
name: sentry-pii-egress
description: Sentry integration egress surface — before_send only strips server_name, so warn!/info! breadcrumbs (upstream_error_body, client names) can leak request-derived data on panic.
metadata:
  type: project
---

Opt-in Sentry integration (branch amondnet/sentry-metrics, PR #12/#13). Egress model: sentry-tracing layer maps `error!`→event, `warn!`/`info!`→breadcrumb. There are NO `tracing::error!` calls in src/, so the only Sentry *events* are panics (panic feature enabled). Breadcrumbs attach to those panic events.

**Why it matters:** `before_send` in `src/main.rs` only nulls `event.server_name`. It does NOT scrub breadcrumb data, span fields, or panic payloads. Docs (shunt.toml.example, docs/running.md) promise "bodies/headers/credentials/client names never sent" — that promise holds for the metrics path but is violated for the error/panic path.

**Known leak sinks (as of this review):**
- `src/adapters/responses.rs:171` — `warn!(upstream_error_body = %text ...)` logs the FULL upstream error body; Responses-API 400/403 bodies echo request/prompt content. Strongest exfil path.
- `src/proxy.rs:201` — `info!(client = %client ...)` puts operator-configured client names into breadcrumbs (docs say no client names).
- `src/proxy.rs:35` span field `session_id` = client `x-claude-code-session-id` header (span-field→event attachment is sentry-tracing-version-dependent; traces_sample_rate default 0).
- `src/metrics.rs` — `model` attribute is the raw client-supplied model string (routing.rs default-provider fallthrough passes it verbatim) → unbounded metric cardinality. Gated behind `metrics=true` (default off).

**How to apply:** when reviewing changes to logging/tracing here, check any new `warn!`/`info!` field for request-derived data, and prefer `debug!` (ignored by the layer) for bodies. A `before_breadcrumb` scrub hook would close the class of issue rather than per-site fixes.
