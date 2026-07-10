---
name: sentry-pii-egress
description: Sentry integration egress surface. Iter-1 breadcrumb.data + span-field exfil paths CLOSED by commit 9b1d2a1 (before_breadcrumb clears .data, span_filter(false) drops all spans). before_send still only strips server_name.
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

**FIX STATUS (commit 9b1d2a1, verified iter-2 against sentry-core/-tracing 0.48.4 vendored source):**
- `before_breadcrumb: |mut b| { b.data.clear(); Some(b) }` — verified in hub.rs:241-262 to run on EVERY breadcrumb (incl. sentry-tracing's `add_breadcrumb` at layer/mod.rs:279) BEFORE it enters the scope ring buffer. Closes the breadcrumb.data path. Breadcrumb `.message` is NOT cleared, but all per-request `warn!`/`info!` call sites use STATIC message literals (request data lives only in fields → cleared). category=target (module path) and ty="log" are safe.
- `.span_filter(|_| false)` — verified in layer/mod.rs:296-340: on_new_span returns at line 302 before start_transaction/record_fields/SentrySpanData insert, so NO transaction, NO TraceContext.data, and converters.rs:100 `ext.get::<SentrySpanData>()` is always None. Layer also has `with_span_attributes=false` (default), so on_event passes span_ctx=None → no span data merged into breadcrumbs/events regardless. Closes the span-field→trace-context path.

**Feature note:** sentry `logs` feature is NOT enabled (Cargo.toml features: backtrace/contexts/metrics/panic/reqwest/rustls/tracing). So EventFilter::Log is compiled out (before_breadcrumb would NOT filter logs, but there are none). WARN/INFO→Breadcrumb only; ERROR→Event (no `error!` sites exist); panics→Event with static/opaque messages (no request data). Metrics attrs = provider/model/http.response.status_code only (model by-design per rejected-findings ledger).

**How to apply:** when reviewing changes here, the invariant is now: any `warn!`/`info!` must keep request-derived data in FIELDS (never interpolated into the format-string message), because before_breadcrumb clears fields but not the message. Never add a `tracing::error!` with request data (becomes a captured event, unfiltered). Do not enable the sentry `logs` feature without adding a log scrubber. Avoid `panic!`/`.expect(msg)` where msg or the Err Debug embeds request data.
