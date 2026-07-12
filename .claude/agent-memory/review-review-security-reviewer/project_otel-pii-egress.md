---
name: otel-pii-egress
description: OTel export path (src/telemetry.rs) leaks session_id + upstream response bodies off-machine, unlike the scrubbed Sentry path.
metadata:
  type: project
---

The opt-in OTel/OTLP export (`src/telemetry.rs`, added on branch amondnet/otel) does NOT replicate the Sentry privacy scrubbing, so it egresses request-derived data the codebase's privacy stance forbids.

**Why:** shunt's invariant is that no request/response bodies, headers, credentials, or client session id leave the machine. Sentry enforces this via `scrub_event` + `span_filter(|_|false)` in `src/main.rs`. The OTel pipeline has no equivalent.

**How to apply:** when reviewing OTel changes, check two leaks:
- `include_session_id` (config.rs) is documented "default off" but is **never read**. `proxy.rs` `proxy_request` span sets `session_id` unconditionally (proxy.rs ~L43); the `tracing_opentelemetry` bridge exports span fields as attributes whenever `traces=true`, so session_id leaks under the default secure config.
- OTel logs bridge (`OpenTelemetryTracingBridge`, `logs=true` default) exports ALL `shunt`-target tracing events gated only by `EnvFilter shunt=info`. That includes `warn!(upstream_error_body = %text …)` at `adapters/responses.rs:591` (raw upstream response body) and `info!(client = %client …)` at `proxy.rs:212`. Parallel to but broader than the [[sentry-pii-egress]] issue (Sentry only leaks these as breadcrumbs on panic; OTel exports every matching event).
