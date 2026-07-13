---
name: sentry-transaction-hostname-leak
description: Sentry perf-tracing transactions bypass before_send/scrub_event, so the machine hostname (server_name) leaks despite the "hostname never sent" contract.
metadata:
  type: project
---

Sentry performance tracing (`[sentry] traces_sample_rate > 0`, added on branch amondnet/sentry-otel) leaks the machine **hostname** to the operator's Sentry project, violating the module privacy contract ("server_name scrubbed", "the host name are never sent").

**Why:** In `sentry` 0.48.4, `Transaction::finish_with_timestamp` (sentry-core performance.rs:880) copies `opts.server_name` onto the transaction and calls `send_envelope` **directly** — it does NOT run `prepare_event` and does NOT invoke the `before_send` callback. shunt's hostname scrubber `scrub_event` (src/main.rs) is registered only as `before_send`, which fires for events/panics but never for transactions. `opts.server_name` is auto-populated with `hostname::get()` by `ContextIntegration::setup` (contexts feature, a default integration) because shunt never sets `server_name` in `ClientOptions`. Crucially, sentry-core 0.48.4 has **no `before_send_transaction` hook** (only before_send / before_breadcrumb / before_send_log / before_send_metric), so no callback can strip transaction fields.

**How to apply:** Fix = set `ClientOptions.server_name` to an explicit non-hostname value (e.g. `Some("".into())` or a constant) so `ContextIntegration::setup` won't auto-fill the hostname (it only sets when `None`). Whenever reviewing the Sentry tracing path, remember: transactions bypass `scrub_event` entirely — any per-span field is sent as-is except session_id, which is separately gated by `telemetry::withhold_session_id()`. See [[sentry-pii-egress]].
