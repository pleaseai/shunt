//! Per-request span tagging and upstream-failure signal for Sentry (#281).
//!
//! Goal: an upstream failure (5xx, or 429/529 quota/overload) is root-causeable
//! from Sentry alone — which model, which provider, what status — without
//! cross-referencing local logs.
//!
//! Two independent mechanisms, because they reach Sentry through different
//! paths:
//!
//! - **Span tagging** ([`record_requested_model`], [`record_span_outcome`]):
//!   fills in `tracing::field::Empty` fields declared on the `proxy_request` /
//!   `codex_endpoint_request` span at creation time. The `sentry` tracing layer
//!   only turns a `tracing` span into a Sentry span/transaction when
//!   `[sentry] traces_sample_rate > 0` (gated by
//!   [`crate::telemetry::sentry_span_export_enabled`]); the OTel trace bridge
//!   mirrors the same fields when `[otel] traces` is enabled. Sentry's own
//!   span/transaction *status* (as opposed to its `data`/`tags`) has no
//!   tracing-field convention — unlike `tracing-opentelemetry`'s
//!   `otel.status_code` — so it is set directly via
//!   `sentry::configure_scope(..).get_span()` in [`mark_sentry_span_status`].
//!   This is a no-op whenever no Sentry span is active (client absent, or span
//!   export disabled), so it costs nothing beyond the existing opt-in.
//! - **Event capture** ([`capture_upstream_outcome`]): a Sentry error/warning
//!   *event* for 5xx and 429/529 responses, built with `sentry::capture_message`
//!   directly rather than a `tracing::error!`/`warn!` macro. Sentry events are
//!   captured unconditionally once a client is bound (independent of
//!   `traces_sample_rate` — that gate is span-only), so routing through
//!   `tracing::warn!` would additionally require the layer's default
//!   `EventFilter` (WARN → breadcrumb, not an event) to be overridden. Calling
//!   the Sentry API directly avoids depending on that filter and keeps this
//!   change from touching `main.rs`'s subscriber wiring.
//!
//! Both mechanisms carry only the requested model id, resolved provider name,
//! and the numeric upstream status — never request/response bodies, headers,
//! or credentials.
//!
//! A config flag to gate this tagging was considered and deferred: the existing
//! `[sentry] traces_sample_rate` / `[otel] traces` opt-ins already gate span
//! export, and event capture only ever emits a handful of tag/status fields (no
//! new privacy surface), so a second flag would add configuration surface
//! without a corresponding new risk to guard.

use axum::http::StatusCode;
use sentry::protocol::SpanStatus;
use sentry::Level;

/// Record the requested model on the current span's `gen_ai.request.model`
/// field (OTel GenAI semantic convention: `gen_ai.request.model`). The field
/// must be declared as `tracing::field::Empty` on the span at creation time
/// (see `proxy::post` / `codex_endpoint::post`) for this to have any effect —
/// recording an undeclared field is silently ignored by `tracing`.
pub(crate) fn record_requested_model(model: &str) {
    tracing::Span::current().record("gen_ai.request.model", model);
}

/// Record the resolved upstream/provider and the upstream HTTP status on the
/// current span, and mark the span as errored for a 4xx/5xx response — both via
/// the `otel.status_code` convention (picked up by `tracing-opentelemetry` for
/// OTLP export) and, directly, Sentry's own span/transaction status (see the
/// module docs for why that needs a separate call). Call once the final
/// upstream status for the request is known; never buffers a streamed
/// response to learn it, since the status is available at response-header
/// time.
pub(crate) fn record_span_outcome(provider: &str, status: StatusCode) {
    let span = tracing::Span::current();
    span.record("shunt.provider", provider);
    span.record("http.response.status_code", status.as_u16());
    span.record(
        "otel.status_code",
        if status.is_client_error() || status.is_server_error() {
            "error"
        } else {
            "ok"
        },
    );
    mark_sentry_span_status(status);
}

/// Set the *active Sentry span/transaction's* own status (searchable as
/// Sentry's "span.status" field) directly, since `sentry-tracing` has no
/// tracing-field convention for it. A no-op when no Sentry span is active —
/// e.g. `[sentry] traces_sample_rate` is 0/unset, or no client is bound — which
/// keeps this piggybacking on the existing span-export opt-in rather than
/// requiring a new one.
fn mark_sentry_span_status(status: StatusCode) {
    if let Some(span) = sentry::configure_scope(|scope| scope.get_span()) {
        span.set_status(sentry_span_status(status));
    }
}

/// Map an upstream HTTP status to a Sentry `SpanStatus`. Pure so it is
/// unit-testable without a bound Sentry client or an active span.
fn sentry_span_status(status: StatusCode) -> SpanStatus {
    match status.as_u16() {
        200..=399 => SpanStatus::Ok,
        401 => SpanStatus::Unauthenticated,
        403 => SpanStatus::PermissionDenied,
        404 => SpanStatus::NotFound,
        429 | 529 => SpanStatus::ResourceExhausted,
        400..=499 => SpanStatus::InvalidArgument,
        501 => SpanStatus::Unimplemented,
        503 => SpanStatus::Unavailable,
        500..=599 => SpanStatus::InternalError,
        _ => SpanStatus::UnknownError,
    }
}

/// Whether an upstream status warrants a Sentry error/warning event, and at
/// what level. Pure — no Sentry client or span required — so it is
/// unit-testable in isolation; [`capture_upstream_outcome`] is the only
/// caller. `5xx` is an operational upstream failure (`Level::Error`); `429`
/// (rate limited) and the non-standard `529` (several upstreams' "overloaded")
/// are expected-but-actionable quota pressure (`Level::Warning`); every other
/// status (2xx/3xx, and 4xx other than 429) needs no event — the request
/// either succeeded or failed for a reason the client, not the operator, must
/// act on.
pub(crate) fn should_capture_upstream_status(status: StatusCode) -> Option<Level> {
    // Check 429/529 first: 529 is a non-standard "overloaded" status that
    // nonetheless falls inside `is_server_error()`'s 500-599 range, so it must
    // be special-cased ahead of the generic 5xx check or it is misclassified
    // as a generic error rather than the intended quota/overload warning.
    if matches!(status.as_u16(), 429 | 529) {
        Some(Level::Warning)
    } else if status.is_server_error() {
        Some(Level::Error)
    } else {
        None
    }
}

/// Emit a Sentry error/warning event for a 5xx or 429/529 upstream response,
/// tagged with the model, provider, and numeric status for triage — no
/// request/response body, header, or credential ever reaches this event. Runs
/// unconditionally whenever a Sentry client is bound (event capture does not
/// depend on `[sentry] traces_sample_rate`, unlike span export); a no-op with
/// no client configured. Uses `sentry::capture_message` directly rather than a
/// `tracing::error!`/`warn!` macro so this does not depend on overriding the
/// `sentry` tracing layer's default `EventFilter` (which otherwise downgrades
/// `WARN` to a breadcrumb, not an event) — see the module docs.
pub(crate) fn capture_upstream_outcome(provider: &str, model: &str, status: StatusCode) {
    let Some(level) = should_capture_upstream_status(status) else {
        return;
    };
    let message = match level {
        Level::Error => "upstream returned an error response",
        _ => "upstream returned a quota/overload response",
    };
    let status_code = status.as_u16();
    sentry::with_scope(
        |scope| {
            scope.set_tag("model", model);
            scope.set_tag("provider", provider);
            scope.set_tag("upstream_status", status_code);
        },
        || {
            sentry::capture_message(message, level);
        },
    );
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::http::StatusCode;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    use super::{
        record_requested_model, record_span_outcome, sentry_span_status,
        should_capture_upstream_status,
    };

    #[test]
    fn should_capture_upstream_status_covers_all_bands() {
        // 5xx is always an error-level event.
        assert_eq!(
            should_capture_upstream_status(StatusCode::INTERNAL_SERVER_ERROR),
            Some(sentry::Level::Error)
        );
        assert_eq!(
            should_capture_upstream_status(StatusCode::BAD_GATEWAY),
            Some(sentry::Level::Error)
        );
        assert_eq!(
            should_capture_upstream_status(StatusCode::SERVICE_UNAVAILABLE),
            Some(sentry::Level::Error)
        );
        // 429 and the non-standard 529 ("overloaded") are warning-level.
        assert_eq!(
            should_capture_upstream_status(StatusCode::TOO_MANY_REQUESTS),
            Some(sentry::Level::Warning)
        );
        let overloaded = StatusCode::from_u16(529).unwrap();
        assert_eq!(
            should_capture_upstream_status(overloaded),
            Some(sentry::Level::Warning)
        );
        // Everything else — success, redirects, and other 4xx — needs no event.
        assert_eq!(should_capture_upstream_status(StatusCode::OK), None);
        assert_eq!(
            should_capture_upstream_status(StatusCode::NOT_MODIFIED),
            None
        );
        assert_eq!(
            should_capture_upstream_status(StatusCode::BAD_REQUEST),
            None
        );
        assert_eq!(
            should_capture_upstream_status(StatusCode::UNAUTHORIZED),
            None
        );
        assert_eq!(should_capture_upstream_status(StatusCode::NOT_FOUND), None);
    }

    #[test]
    fn sentry_span_status_maps_status_bands() {
        assert_eq!(
            sentry_span_status(StatusCode::OK),
            sentry::protocol::SpanStatus::Ok
        );
        assert_eq!(
            sentry_span_status(StatusCode::UNAUTHORIZED),
            sentry::protocol::SpanStatus::Unauthenticated
        );
        assert_eq!(
            sentry_span_status(StatusCode::FORBIDDEN),
            sentry::protocol::SpanStatus::PermissionDenied
        );
        assert_eq!(
            sentry_span_status(StatusCode::NOT_FOUND),
            sentry::protocol::SpanStatus::NotFound
        );
        assert_eq!(
            sentry_span_status(StatusCode::TOO_MANY_REQUESTS),
            sentry::protocol::SpanStatus::ResourceExhausted
        );
        assert_eq!(
            sentry_span_status(StatusCode::from_u16(529).unwrap()),
            sentry::protocol::SpanStatus::ResourceExhausted
        );
        assert_eq!(
            sentry_span_status(StatusCode::BAD_REQUEST),
            sentry::protocol::SpanStatus::InvalidArgument
        );
        assert_eq!(
            sentry_span_status(StatusCode::NOT_IMPLEMENTED),
            sentry::protocol::SpanStatus::Unimplemented
        );
        assert_eq!(
            sentry_span_status(StatusCode::SERVICE_UNAVAILABLE),
            sentry::protocol::SpanStatus::Unavailable
        );
        assert_eq!(
            sentry_span_status(StatusCode::INTERNAL_SERVER_ERROR),
            sentry::protocol::SpanStatus::InternalError
        );
        assert_eq!(
            sentry_span_status(StatusCode::from_u16(599).unwrap()),
            sentry::protocol::SpanStatus::InternalError
        );
    }

    /// A `Visit` that stringifies every recorded field into a shared map, so
    /// the test can assert on exactly what `tracing::Span::record` sent
    /// through, independent of any particular exporter.
    struct CapturingVisitor<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for CapturingVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    /// A minimal `Layer` that records every `Span::record(...)` call into a
    /// shared map — this is what a real exporter (the Sentry tracing layer's
    /// `on_record`, or `tracing-opentelemetry`'s) observes, so asserting on it
    /// proves the empty-field-then-record pattern actually reaches a
    /// subscriber rather than merely compiling.
    #[derive(Clone, Default)]
    struct CapturingLayer(Arc<Mutex<HashMap<String, String>>>);

    impl<S: tracing::Subscriber> Layer<S> for CapturingLayer {
        fn on_record(
            &self,
            _id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: Context<'_, S>,
        ) {
            let mut map = self.0.lock().expect("capturing layer mutex poisoned");
            values.record(&mut CapturingVisitor(&mut map));
        }
    }

    #[test]
    fn records_requested_model_and_upstream_outcome_on_the_current_span() {
        let captured = CapturingLayer::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "test_request",
                gen_ai.request.model = tracing::field::Empty,
                shunt.provider = tracing::field::Empty,
                http.response.status_code = tracing::field::Empty,
                otel.status_code = tracing::field::Empty
            );
            let _entered = span.enter();
            record_requested_model("claude-opus-5");
            record_span_outcome("anthropic", StatusCode::INTERNAL_SERVER_ERROR);
        });

        let map = captured.0.lock().unwrap();
        assert_eq!(
            map.get("gen_ai.request.model").map(String::as_str),
            Some("claude-opus-5")
        );
        assert_eq!(
            map.get("shunt.provider").map(String::as_str),
            Some("anthropic")
        );
        assert_eq!(
            map.get("http.response.status_code").map(String::as_str),
            Some("500")
        );
        assert_eq!(
            map.get("otel.status_code").map(String::as_str),
            Some("error")
        );
    }

    #[test]
    fn records_ok_status_for_a_successful_response() {
        let captured = CapturingLayer::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "test_request_ok",
                shunt.provider = tracing::field::Empty,
                otel.status_code = tracing::field::Empty
            );
            let _entered = span.enter();
            record_span_outcome("anthropic", StatusCode::OK);
        });

        let map = captured.0.lock().unwrap();
        assert_eq!(map.get("otel.status_code").map(String::as_str), Some("ok"));
    }
}
