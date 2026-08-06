//! Per-request span tagging and upstream-failure signal for Sentry (#281, #287).
//!
//! Goal: an upstream failure is root-causeable from Sentry alone — which
//! model, which provider, what happened — without cross-referencing local
//! logs. That covers two distinct kinds of failure:
//!
//! - One known at response-header time: 5xx, or 429/529 quota/overload.
//! - One discovered only mid-stream, after the upstream already answered
//!   `200` and started an SSE stream: an `event: error` frame, or the
//!   connection cut before a terminal event (#287). Without this, such a
//!   request is recorded as a plain success forever — `stream_metrics`
//!   already detects it (`ObserverState::finish`, for the
//!   `shunt_stream_outcome_total` metric) but, before #287, never told
//!   `observability` about it.
//!
//! Three mechanisms, because they reach Sentry through different paths and,
//! for the mid-stream case, at a different point in the request lifecycle:
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
//!   export disabled), so it costs nothing beyond the existing opt-in. This
//!   only ever runs at response-header time, once, from the handler.
//! - **Event capture** ([`capture_upstream_outcome`]): a Sentry error/warning
//!   *event* for 5xx and 429/529 responses, built with `sentry::capture_message`
//!   directly rather than a `tracing::error!`/`warn!` macro. Sentry events are
//!   captured unconditionally once a client is bound (independent of
//!   `traces_sample_rate` — that gate is span-only), so routing through
//!   `tracing::warn!` would additionally require the layer's default
//!   `EventFilter` (WARN → breadcrumb, not an event) to be overridden. Calling
//!   the Sentry API directly avoids depending on that filter and keeps this
//!   change from touching `main.rs`'s subscriber wiring.
//! - **Mid-stream failure** ([`record_stream_failure`]): the same two ideas —
//!   span field + Sentry event — applied from *inside the response body's
//!   stream poll*, well after the handler that created the span has already
//!   returned. This split is why it cannot simply call
//!   [`record_span_outcome`]/[`mark_sentry_span_status`] again: by the time a
//!   streamed body is being polled, `sentry-tracing`'s `on_exit` has already
//!   popped the `HubSwitchGuard` that made the request's Sentry span reachable
//!   via `sentry::configure_scope(..).get_span()` — that accessor now returns
//!   `None` (or some unrelated span), and there is no public API to reach a
//!   *specific*, no-longer-current tracing span's Sentry span/transaction
//!   handle. So Sentry's own span-status enum genuinely cannot be set for a
//!   mid-stream failure; this is a real, structural limitation of
//!   `sentry-tracing` 0.48, not a shortcut taken here. What *does* still work:
//!   `crate::stream_metrics::ObserverState` holds a cloned `tracing::Span`
//!   handle for the request span from the moment the stream is wrapped
//!   (`stream_metrics::observe_response`) until the stream itself finishes —
//!   tracing spans are ref-counted, so this clone defers the span's `on_close`
//!   (and, transitively, `sentry_span.finish()` and the OTel bridge's export)
//!   until then. Re-recording `otel.status_code` on that held clone therefore
//!   still reaches every layer's `on_record` while the span is open, including
//!   `sentry-tracing`'s, which mirrors recorded fields onto the still-open
//!   Sentry span's `data` (not its `status` enum, but still visible/searchable
//!   there) — so the OTel export and Sentry's span `data` both correctly end
//!   up `error` for a stream that opened `ok`. The event capture, unlike the
//!   span-status call, was never scope-dependent (it sets its own tags via
//!   `sentry::with_scope` rather than reading ambient state) and works
//!   identically here.
//!
//! All three carry only the requested model id, resolved provider name, and
//! (for the header-time and mid-stream paths respectively) the numeric
//! upstream status or the stream outcome — never request/response bodies,
//! headers, or credentials. The model id is client-supplied free text (the
//! inbound Codex endpoint in particular forwards its request body verbatim and
//! only reads `model` for this label, see `codex_endpoint.rs`), so it is
//! length-bounded and stripped of control characters before export by
//! [`sanitize_model_tag`] — otherwise an oversized or control-character-laden
//! value would reach a Sentry tag/span field unchanged. The mid-stream event
//! additionally carries the observer's own counters, the last observed SSE
//! event type, and — when the body read itself failed — the upstream error's
//! message (#310). The counters are plain integers the gateway computed, so
//! there is nothing in them to sanitize; the event type and the error message
//! are upstream text and do go through [`sanitize_tag`], at their own limits.
//!
//! A config flag to gate this tagging was considered and deferred: the existing
//! `[sentry] traces_sample_rate` / `[otel] traces` opt-ins already gate span
//! export, and event capture only ever emits a handful of tag/status fields (no
//! new privacy surface), so a second flag would add configuration surface
//! without a corresponding new risk to guard.

pub(crate) mod throttle;

use std::borrow::Cow;
use std::time::Instant;

use axum::http::StatusCode;
use sentry::protocol::SpanStatus;
use sentry::Level;

use throttle::ThrottleDecision;

/// Upper bound (in `char`s) on the model id exported to a span field or
/// Sentry tag, enforced by [`sanitize_model_tag`]. Well under Sentry's
/// 200-character tag-value limit, generous for any real model id, and small
/// enough to keep tag cardinality/payload size bounded for a client-supplied
/// value.
const MAX_MODEL_TAG_LEN: usize = 128;

/// Upper bound (in `char`s) on the last observed SSE event type exported as an
/// event extra. Upstream-controlled text, so it is bounded like the model tag;
/// generous for the event names the two protocols actually use
/// (`content_block_delta`, `response.output_item.done`, and the like).
const MAX_EVENT_TYPE_LEN: usize = 64;

/// Upper bound (in `char`s) on the upstream error message exported as an event
/// extra. Larger than the tag limits because this is the one field meant to be
/// read as prose, but still bounded — the message is rendered from the
/// upstream connection's error chain, which this process does not control.
const MAX_UPSTREAM_ERROR_LEN: usize = 256;

/// Sanitize a client- or upstream-supplied string before it is attached to a
/// span field, Sentry tag, or event extra: strip control/non-printable
/// characters, then truncate to `max_len` characters. Falls back to
/// `"invalid"` when the sanitized value is empty (blank input, or a value that
/// is nothing but control characters). Pure so it is unit-testable in
/// isolation.
fn sanitize_tag(value: &str, max_len: usize) -> Cow<'_, str> {
    // Only the first `max_len` raw characters can ever change the output: if
    // none of them is a control character, the collect-and-take branch below
    // would keep exactly those characters unfiltered anyway (the first
    // `max_len` chars all pass the filter, so `take` stops right there without
    // looking further), which is the same result as the plain-truncate branch.
    // So a control character beyond this window can never affect the output,
    // and detection can stop there too.
    let needs_stripping = value.chars().take(max_len).any(char::is_control);
    let cleaned: Cow<'_, str> = if needs_stripping {
        // `Iterator::take` short-circuits the underlying `chars()` iterator
        // once `max_len` non-control characters have been produced, so this
        // never allocates more than the truncation limit — even for a
        // pathological multi-KB/MB input with a control character near the
        // start. Collecting the *entire* filtered string first (and truncating
        // after) would allocate proportional to the full input instead.
        Cow::Owned(
            value
                .chars()
                .filter(|c| !c.is_control())
                .take(max_len)
                .collect(),
        )
    } else if let Some((byte_idx, _)) = value.char_indices().nth(max_len) {
        // No control characters to strip, so the original slice can be
        // truncated in place — `nth` also short-circuits, so this only walks
        // the first `max_len` characters, not the whole string.
        Cow::Borrowed(&value[..byte_idx])
    } else {
        Cow::Borrowed(value)
    };
    if cleaned.is_empty() {
        Cow::Borrowed("invalid")
    } else {
        cleaned
    }
}

/// [`sanitize_tag`] at [`MAX_MODEL_TAG_LEN`], for the model id. The model id
/// reaching this module is unvalidated free text — most notably the inbound
/// Codex endpoint's `ModelView`, which parses it with no format constraint
/// since the request body forwards upstream byte-for-byte
/// (`codex_endpoint.rs`) — so without this an oversized or
/// control-character-laden value would reach telemetry unchanged, weakening
/// the "no request content" guarantee for this one field and inflating tag
/// cardinality/size.
fn sanitize_model_tag(model: &str) -> Cow<'_, str> {
    sanitize_tag(model, MAX_MODEL_TAG_LEN)
}

/// Record the requested model on the current span's `gen_ai.request.model`
/// field (OTel GenAI semantic convention: `gen_ai.request.model`). The field
/// must be declared as `tracing::field::Empty` on the span at creation time
/// (see `proxy::post` / `codex_endpoint::post`) for this to have any effect —
/// recording an undeclared field is silently ignored by `tracing`. `model` is
/// sanitized via [`sanitize_model_tag`] before export.
pub(crate) fn record_requested_model(model: &str) {
    tracing::Span::current().record("gen_ai.request.model", sanitize_model_tag(model).as_ref());
}

/// Record the resolved upstream/provider and the upstream HTTP status on the
/// current span, and mark the span as errored for a 5xx response — both via
/// the `otel.status_code` convention (picked up by `tracing-opentelemetry` for
/// OTLP export) and, directly, Sentry's own span/transaction status (see the
/// module docs for why that needs a separate call). A 4xx leaves
/// `otel.status_code` as `"ok"`: per OTel semantic conventions for HTTP server
/// spans, client errors are not span-level errors — only 5xx and
/// transport-level failures are (Sentry's own `SpanStatus` mapping below is a
/// separate convention and is unaffected). Call once the final upstream
/// status for the request is known; never buffers a streamed response to
/// learn it, since the status is available at response-header time.
pub(crate) fn record_span_outcome(provider: &str, status: StatusCode) {
    let span = tracing::Span::current();
    span.record("shunt.provider", provider);
    span.record("http.response.status_code", status.as_u16());
    span.record(
        "otel.status_code",
        if status.is_server_error() {
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
/// request/response body, header, or credential ever reaches this event
/// (`model` is sanitized via [`sanitize_model_tag`] before it becomes a tag).
/// Runs unconditionally whenever a Sentry client is bound (event capture does
/// not depend on `[sentry] traces_sample_rate`, unlike span export); a no-op
/// with no client configured. Uses `sentry::capture_message` directly rather
/// than a `tracing::error!`/`warn!` macro so this does not depend on
/// overriding the `sentry` tracing layer's default `EventFilter` (which
/// otherwise downgrades `WARN` to a breadcrumb, not an event) — see the
/// module docs.
pub(crate) fn capture_upstream_outcome(provider: &str, model: &str, status: StatusCode) {
    let Some(level) = should_capture_upstream_status(status) else {
        return;
    };
    let message = match level {
        Level::Error => "upstream returned an error response",
        _ => "upstream returned a quota/overload response",
    };
    let status_code = status.as_u16();
    let model = sanitize_model_tag(model);
    sentry::with_scope(
        |scope| {
            scope.set_tag("model", model.as_ref());
            scope.set_tag("provider", provider);
            scope.set_tag("upstream_status", status_code);
        },
        || {
            sentry::capture_message(message, level);
        },
    );
}

/// The two `stream_metrics::Outcome` variants that represent an upstream
/// *failure* discovered mid-stream — the only ones [`record_stream_failure`]
/// is ever called for (`stream_metrics::Outcome::Completed` and
/// `::ClientDisconnect` are not failures of the upstream: a natural end and a
/// client hangup are not root-causeable events). The label strings mirror
/// (without importing — `stream_metrics::Outcome` is private to that module)
/// the ones already used for the `shunt_stream_outcome_total` metric, so a
/// `stream_outcome="error_event"` in Prometheus and an `outcome=error_event`
/// tag on a Sentry event refer to the same thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamFailureKind {
    /// The upstream sent an `event: error` SSE frame before any terminal
    /// event — an operational failure the upstream itself reported.
    ErrorEvent,
    /// The connection was cut (by the upstream, or a network intermediary)
    /// before a terminal event or an error event ever arrived.
    UpstreamCut,
}

impl StreamFailureKind {
    /// The Sentry `outcome` tag for this failure. Must stay identical to
    /// `stream_metrics::Outcome::as_str` for the corresponding outcome, so
    /// the tag joins against the `outcome` attribute of the
    /// `shunt.stream_outcome` metric; the test
    /// `stream_metrics::tests::stream_failure_labels_match_outcome_labels`
    /// pins that.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ErrorEvent => "error_event",
            Self::UpstreamCut => "upstream_cut",
        }
    }
}

/// Which flavor of upstream cut ended a stream, exported as the `cut_kind`
/// Sentry tag on an [`StreamFailureKind::UpstreamCut`] event (#310). The three
/// are very different faults that the single `upstream_cut` outcome conflates:
/// a clean end with no terminal event, a failed body read, and a
/// gateway-synthesized completion after a cut the adapter already detected.
///
/// This is deliberately a *tag on the existing outcome* rather than a new
/// outcome: the `shunt.stream_outcome` metric's `outcome` label keeps its four
/// values, so no existing dashboard or alert changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CutKind {
    /// The upstream body ended cleanly without ever sending a terminal event.
    Eof,
    /// Reading the upstream body failed; [`StreamFailureContext::upstream_error`]
    /// carries the error's rendered message.
    TransportError,
    /// The adapter had already detected the cut and injected
    /// `stream_metrics::UPSTREAM_TRUNCATED_MARKER` ahead of a synthesized
    /// completion, so the client still saw a well-formed stream.
    Marker,
}

impl CutKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::TransportError => "transport_error",
            Self::Marker => "marker",
        }
    }
}

/// What the streaming observer saw before the stream failed, attached to the
/// Sentry event as extras so one event is diagnosable on its own (#310).
/// Before this, every mid-stream failure event was identical apart from the
/// `model` tag.
///
/// Built by `stream_metrics::ObserverState` on the failure path only, so a
/// healthy stream never pays for it.
#[derive(Debug, Default)]
pub(crate) struct StreamFailureContext {
    /// Which flavor of cut this was; `None` for
    /// [`StreamFailureKind::ErrorEvent`], which is not a cut.
    pub(crate) cut_kind: Option<CutKind>,
    /// The upstream body error's rendered message, when the body read is what
    /// failed. Sanitized and capped at [`MAX_UPSTREAM_ERROR_LEN`] on export.
    pub(crate) upstream_error: Option<String>,
    /// Complete SSE frames the observer parsed before the stream ended.
    pub(crate) sse_events: u64,
    /// Body bytes forwarded to the client before the stream ended.
    pub(crate) bytes_forwarded: u64,
    /// The `event:` name of the last non-keepalive frame parsed. Sanitized and
    /// capped at [`MAX_EVENT_TYPE_LEN`] on export.
    pub(crate) last_event_type: Option<String>,
    /// Milliseconds from request start to the failure.
    pub(crate) elapsed_ms: u64,
    /// Milliseconds from request start to the first body chunk, when one
    /// arrived at all.
    pub(crate) ttft_ms: Option<u64>,
}

/// The throttle key for one mid-stream failure event: the dimensions a
/// distinct event would actually be triaged along. The sanitized model is
/// already length-capped, and the table itself is size-capped
/// (`throttle::MAX_KEYS`), so a client-chosen model id cannot grow it without
/// bound (#296). `\u{1f}` (unit separator) is a safe delimiter: `model` is the
/// only component that can carry arbitrary text, and [`sanitize_tag`] has
/// already stripped its control characters. `outcome` and `cut_kind` are
/// `&'static str`s from a closed set, and `provider` comes from the operator's
/// own config.
fn stream_failure_throttle_key(
    provider: &str,
    model: &str,
    outcome: StreamFailureKind,
    cut_kind: Option<CutKind>,
) -> String {
    const SEP: char = '\u{1f}';
    let cut_kind = cut_kind.map_or("", CutKind::as_str);
    let outcome = outcome.as_str();
    format!("{provider}{SEP}{model}{SEP}{outcome}{SEP}{cut_kind}")
}

/// Mid-stream counterpart to [`record_span_outcome`] + [`capture_upstream_outcome`]:
/// an upstream that answered `200` and only failed *after* headers were
/// already sent — an `event: error` SSE frame, or the connection cut before a
/// terminal event — never reaches either of those, since both run at
/// response-header time and by definition see only the `200`
/// (`crate::stream_metrics::ObserverState::finish` is the sole caller, gated
/// to exactly the two [`StreamFailureKind`] variants, #287).
///
/// `span` must be the request's own span (`proxy_request` /
/// `codex_endpoint_request`), captured via `tracing::Span::current()` while
/// still inside the `.instrument(span)` future
/// (`stream_metrics::observe_response`) and held until the stream finishes —
/// see the module docs for why recording on it still works, and why that is
/// *not* true for Sentry's own span-status enum (only the standalone event
/// below reaches that reliably).
///
/// The span field is recorded unconditionally; only the Sentry event is
/// rate-limited, per (provider, model, outcome, cut kind), by [`throttle`]
/// (#310). A client retrying a cut stream would otherwise turn one underlying
/// fault into thousands of identical events.
pub(crate) fn record_stream_failure(
    span: &tracing::Span,
    provider: &str,
    model: &str,
    outcome: StreamFailureKind,
    context: &StreamFailureContext,
) {
    record_stream_failure_at(span, provider, model, outcome, context, Instant::now());
}

/// [`record_stream_failure`] with the throttle's clock as a parameter, so a
/// test can cross a window boundary without sleeping.
fn record_stream_failure_at(
    span: &tracing::Span,
    provider: &str,
    model: &str,
    outcome: StreamFailureKind,
    context: &StreamFailureContext,
    now: Instant,
) {
    // Overwrites whatever `record_span_outcome` already recorded at
    // response-header time (`"ok"`, since a stream failure by definition
    // starts with a `200`) — `tracing` permits re-recording a declared field,
    // and every layer that mirrors it (the OTel bridge, `sentry-tracing`'s
    // span-data mirror) takes the latest value observed before the span
    // closes, which this is: `ObserverState` holds the only extra clone of
    // `span`, `finish` runs at most once (`self.finished` guard), and that
    // clone is not dropped until after this call returns.
    span.record("otel.status_code", "error");

    let (level, message) = match outcome {
        StreamFailureKind::ErrorEvent => (
            Level::Error,
            "upstream SSE stream sent an error event mid-stream",
        ),
        StreamFailureKind::UpstreamCut => (
            Level::Warning,
            "upstream SSE stream was cut before a terminal event",
        ),
    };
    let model = sanitize_model_tag(model);
    let key = stream_failure_throttle_key(provider, &model, outcome, context.cut_kind);
    let suppressed = match throttle::admit(&key, now) {
        ThrottleDecision::Emit { suppressed } => suppressed,
        ThrottleDecision::Suppress => return,
    };

    sentry::with_scope(
        |scope| {
            scope.set_tag("model", model.as_ref());
            scope.set_tag("provider", provider);
            scope.set_tag("outcome", outcome.as_str());
            if let Some(cut_kind) = context.cut_kind {
                scope.set_tag("cut_kind", cut_kind.as_str());
            }
            scope.set_extra("sse_events", context.sse_events.into());
            scope.set_extra("bytes_forwarded", context.bytes_forwarded.into());
            scope.set_extra("elapsed_ms", context.elapsed_ms.into());
            if let Some(ttft_ms) = context.ttft_ms {
                scope.set_extra("ttft_ms", ttft_ms.into());
            }
            if let Some(last_event_type) = &context.last_event_type {
                scope.set_extra(
                    "last_event_type",
                    sanitize_tag(last_event_type, MAX_EVENT_TYPE_LEN)
                        .into_owned()
                        .into(),
                );
            }
            if let Some(upstream_error) = &context.upstream_error {
                scope.set_extra(
                    "upstream_error",
                    sanitize_tag(upstream_error, MAX_UPSTREAM_ERROR_LEN)
                        .into_owned()
                        .into(),
                );
            }
            if suppressed > 0 {
                // Non-zero only on the first event of a window that follows a
                // window whose budget ran out, so the gap in the signal is
                // visible in the signal itself.
                scope.set_extra("suppressed_count", suppressed.into());
            }
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
        capture_upstream_outcome, record_requested_model, record_span_outcome,
        record_stream_failure, record_stream_failure_at, sanitize_model_tag, sentry_span_status,
        should_capture_upstream_status, throttle, CutKind, StreamFailureContext, StreamFailureKind,
        MAX_EVENT_TYPE_LEN, MAX_MODEL_TAG_LEN, MAX_UPSTREAM_ERROR_LEN,
    };

    #[test]
    fn sanitize_model_tag_passes_through_an_ordinary_model_id() {
        assert_eq!(sanitize_model_tag("claude-opus-5"), "claude-opus-5");
    }

    #[test]
    fn sanitize_model_tag_keeps_a_value_exactly_at_the_limit() {
        let exactly_at_limit = "a".repeat(MAX_MODEL_TAG_LEN);
        assert_eq!(sanitize_model_tag(&exactly_at_limit), exactly_at_limit);
    }

    #[test]
    fn sanitize_model_tag_truncates_a_value_over_the_limit() {
        let over_limit = "a".repeat(MAX_MODEL_TAG_LEN + 1);
        let sanitized = sanitize_model_tag(&over_limit);
        assert_eq!(sanitized.chars().count(), MAX_MODEL_TAG_LEN);
        assert_eq!(sanitized, "a".repeat(MAX_MODEL_TAG_LEN));
    }

    #[test]
    fn sanitize_model_tag_strips_control_characters() {
        // Embedded control bytes (e.g. from a client trying to smuggle a
        // newline/escape sequence into a Sentry tag or span field) are
        // dropped, not merely truncated away.
        let with_control_chars = "claude\n-\topus\r-5\u{7}";
        assert_eq!(sanitize_model_tag(with_control_chars), "claude-opus-5");
    }

    #[test]
    fn sanitize_model_tag_falls_back_to_invalid_when_empty() {
        assert_eq!(sanitize_model_tag(""), "invalid");
    }

    #[test]
    fn sanitize_model_tag_falls_back_to_invalid_when_only_control_characters() {
        assert_eq!(sanitize_model_tag("\n\t\r\u{7}"), "invalid");
    }

    #[test]
    fn sanitize_model_tag_ignores_a_control_character_beyond_the_truncation_window() {
        // The control-character scan is bounded to the first
        // `MAX_MODEL_TAG_LEN` characters. A control character placed well
        // beyond that window must not change the output at all: the clean
        // prefix is already exactly what a plain truncation would produce.
        let clean_prefix = "a".repeat(MAX_MODEL_TAG_LEN);
        let with_late_control_char = format!("{clean_prefix}\u{7}{}", "b".repeat(1024));
        assert_eq!(sanitize_model_tag(&with_late_control_char), clean_prefix);
    }

    #[test]
    fn sanitize_model_tag_bounds_a_pathological_multi_kb_input_with_a_control_character() {
        // A control character forces the strip-then-collect path; without a
        // `take(MAX_MODEL_TAG_LEN)` bound during collection, this would
        // allocate a `String` proportional to the entire multi-KB input
        // before truncating it back down. Assert the result is still capped
        // at the limit, regardless of how large the raw input is.
        let pathological = format!("\u{7}{}", "a".repeat(64 * 1024));
        let sanitized = sanitize_model_tag(&pathological);
        assert_eq!(sanitized.chars().count(), MAX_MODEL_TAG_LEN);
        assert_eq!(sanitized, "a".repeat(MAX_MODEL_TAG_LEN));
    }

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
        // Outside every named band (600 is a valid `StatusCode` but matches
        // none of the 2xx/4xx/5xx arms above): falls through to the default.
        assert_eq!(
            sentry_span_status(StatusCode::from_u16(600).unwrap()),
            sentry::protocol::SpanStatus::UnknownError
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

    #[test]
    fn records_ok_otel_status_for_a_4xx_client_error() {
        // OTel semantic conventions for HTTP server spans mark a span `error`
        // only for 5xx/transport failures — a 4xx is a client error and must
        // leave `otel.status_code` as `"ok"` (unlike Sentry's own SpanStatus
        // mapping, which does distinguish 4xx bands; see `sentry_span_status`).
        let captured = CapturingLayer::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "test_request_4xx",
                shunt.provider = tracing::field::Empty,
                otel.status_code = tracing::field::Empty
            );
            let _entered = span.enter();
            record_span_outcome("anthropic", StatusCode::TOO_MANY_REQUESTS);
        });

        let map = captured.0.lock().unwrap();
        assert_eq!(map.get("otel.status_code").map(String::as_str), Some("ok"));
    }

    // `capture_upstream_outcome` is the only caller of `should_capture_upstream_status`
    // that actually reaches Sentry; the tests above only exercise the pure decision
    // function. These use `sentry::test::with_captured_events` — a real (test) Sentry
    // client bound to a scoped Hub for the closure's duration — to verify the event
    // capture path itself is wired up: the right event count, level, message, and
    // (importantly, since this is the no-PII guarantee) tag set actually get sent.

    #[test]
    fn capture_upstream_outcome_emits_error_event_for_5xx() {
        let events = sentry::test::with_captured_events(|| {
            capture_upstream_outcome(
                "anthropic",
                "claude-opus-5",
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, sentry::Level::Error);
        assert_eq!(
            events[0].message.as_deref(),
            Some("upstream returned an error response")
        );
    }

    #[test]
    fn capture_upstream_outcome_emits_warning_event_for_429_and_529() {
        let events = sentry::test::with_captured_events(|| {
            capture_upstream_outcome("anthropic", "claude-opus-5", StatusCode::TOO_MANY_REQUESTS);
            capture_upstream_outcome(
                "anthropic",
                "claude-opus-5",
                StatusCode::from_u16(529).unwrap(),
            );
        });

        assert_eq!(events.len(), 2);
        for event in &events {
            assert_eq!(event.level, sentry::Level::Warning);
            assert_eq!(
                event.message.as_deref(),
                Some("upstream returned a quota/overload response")
            );
        }
    }

    #[test]
    fn capture_upstream_outcome_emits_no_event_for_success_or_ordinary_4xx() {
        let events = sentry::test::with_captured_events(|| {
            capture_upstream_outcome("anthropic", "claude-opus-5", StatusCode::OK);
            capture_upstream_outcome("anthropic", "claude-opus-5", StatusCode::BAD_REQUEST);
            capture_upstream_outcome("anthropic", "claude-opus-5", StatusCode::NOT_FOUND);
        });

        assert!(events.is_empty());
    }

    #[test]
    fn capture_upstream_outcome_event_carries_exactly_model_provider_and_status_tags() {
        let events = sentry::test::with_captured_events(|| {
            capture_upstream_outcome(
                "anthropic",
                "claude-opus-5",
                StatusCode::SERVICE_UNAVAILABLE,
            );
        });

        assert_eq!(events.len(), 1);
        let tags = &events[0].tags;
        // Exactly these three keys — nothing else (no body, header, or credential
        // ever gets attached to this event; this is the no-PII guarantee, locked
        // down structurally rather than by spot-checking a couple of fields).
        assert_eq!(tags.len(), 3, "unexpected tags: {tags:?}");
        assert_eq!(tags.get("model").map(String::as_str), Some("claude-opus-5"));
        assert_eq!(tags.get("provider").map(String::as_str), Some("anthropic"));
        assert_eq!(tags.get("upstream_status").map(String::as_str), Some("503"));
    }

    #[test]
    fn capture_upstream_outcome_sanitizes_an_oversized_client_supplied_model_tag() {
        // A client-supplied `model` (e.g. the inbound Codex endpoint's
        // unvalidated request-body field) must not reach the Sentry tag
        // unbounded — this proves the sanitization from `sanitize_model_tag`
        // is actually wired into the event-capture call site, not just the
        // pure helper.
        let oversized_model = format!("{}-with-a-trailing-newline\n", "m".repeat(200));
        let events = sentry::test::with_captured_events(|| {
            capture_upstream_outcome("anthropic", &oversized_model, StatusCode::BAD_GATEWAY);
        });

        assert_eq!(events.len(), 1);
        let tag = events[0]
            .tags
            .get("model")
            .expect("model tag must be present");
        assert_eq!(tag.chars().count(), MAX_MODEL_TAG_LEN);
        assert!(!tag.contains('\n'), "control characters must be stripped");
    }

    // `record_stream_failure` is the mid-stream counterpart to
    // `record_span_outcome` + `capture_upstream_outcome` (see the module
    // docs for why it is a separate function rather than reusing those). The
    // wiring that only calls it for the right `stream_metrics::Outcome`
    // variants is exercised in `stream_metrics::tests`; these tests cover the
    // function itself in isolation, mirroring the two blocks above.
    //
    // Every one of them installs a scoped throttle first: the Sentry event is
    // rate-limited per (provider, model, outcome, cut kind) against a
    // process-global window table, so without a per-thread override an event
    // count would depend on what sibling tests happened to emit.

    /// A context with no counters, for the cases that only care about the
    /// level/message/tags. The extras are asserted on their own below.
    fn empty_context(cut_kind: Option<CutKind>) -> StreamFailureContext {
        StreamFailureContext {
            cut_kind,
            ..Default::default()
        }
    }

    #[test]
    fn record_stream_failure_overwrites_an_already_recorded_otel_status_code() {
        // A mid-stream failure always follows a `200`, which
        // `record_span_outcome` already recorded as `otel.status_code = "ok"`
        // at response-header time — this proves the second, later call
        // actually overwrites that value rather than being silently ignored
        // as a duplicate.
        let _throttle = throttle::test_support::scoped();
        let captured = CapturingLayer::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "test_stream_request",
                otel.status_code = tracing::field::Empty
            );
            let _entered = span.enter();
            span.record("otel.status_code", "ok");
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::ErrorEvent,
                &empty_context(None),
            );
        });

        let map = captured.0.lock().unwrap();
        assert_eq!(
            map.get("otel.status_code").map(String::as_str),
            Some("error")
        );
    }

    #[test]
    fn record_stream_failure_records_the_span_field_even_when_the_event_is_throttled() {
        // The throttle governs only the Sentry event. The span field — and,
        // at the call site, the `shunt.stream_outcome` metric — must stay
        // exact however many events are dropped.
        let _throttle = throttle::test_support::scoped();
        let captured = CapturingLayer::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());

        let events = tracing::subscriber::with_default(subscriber, || {
            sentry::test::with_captured_events(|| {
                let span = tracing::info_span!(
                    "test_throttled_stream_request",
                    otel.status_code = tracing::field::Empty
                );
                span.record("otel.status_code", "ok");
                for _ in 0..20 {
                    record_stream_failure(
                        &span,
                        "anthropic",
                        "claude-opus-5",
                        StreamFailureKind::UpstreamCut,
                        &empty_context(Some(CutKind::Eof)),
                    );
                }
            })
        });

        assert!(
            events.len() < 20,
            "the throttle must drop repeats of one key: {}",
            events.len()
        );
        let map = captured.0.lock().unwrap();
        assert_eq!(
            map.get("otel.status_code").map(String::as_str),
            Some("error")
        );
    }

    #[test]
    fn record_stream_failure_reports_the_suppressed_count_after_a_window_rolls_over() {
        // Overshoot one window's budget, then cross the boundary: the first
        // event through afterwards must say how many were dropped in between,
        // so the gap the throttle creates is visible in the signal itself.
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let start = std::time::Instant::now();
        let events = sentry::test::with_captured_events(|| {
            for _ in 0..12 {
                record_stream_failure_at(
                    &span,
                    "anthropic",
                    "claude-opus-5",
                    StreamFailureKind::UpstreamCut,
                    &empty_context(Some(CutKind::Eof)),
                    start,
                );
            }
            record_stream_failure_at(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &empty_context(Some(CutKind::Eof)),
                start + throttle::WINDOW,
            );
        });

        let (last, earlier) = events.split_last().expect("at least one event");
        assert!(
            earlier
                .iter()
                .all(|event| !event.extra.contains_key("suppressed_count")),
            "only the first event after a rollover may report suppressions"
        );
        assert_eq!(
            last.extra.get("suppressed_count"),
            Some(&((12 - earlier.len()) as u64).into()),
            "the count must be exactly what the first window dropped"
        );
    }

    #[test]
    fn record_stream_failure_emits_an_error_event_for_an_error_event_outcome() {
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::ErrorEvent,
                &empty_context(None),
            );
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, sentry::Level::Error);
        assert_eq!(
            events[0].message.as_deref(),
            Some("upstream SSE stream sent an error event mid-stream")
        );
    }

    #[test]
    fn record_stream_failure_emits_a_warning_event_for_an_upstream_cut_outcome() {
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &empty_context(Some(CutKind::Eof)),
            );
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, sentry::Level::Warning);
        assert_eq!(
            events[0].message.as_deref(),
            Some("upstream SSE stream was cut before a terminal event")
        );
    }

    #[test]
    fn record_stream_failure_event_carries_exactly_model_provider_outcome_and_cut_kind_tags() {
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &empty_context(Some(CutKind::TransportError)),
            );
        });

        assert_eq!(events.len(), 1);
        let tags = &events[0].tags;
        // Exactly these four keys — nothing else (no body, header, or credential
        // ever gets attached to this event; this is the no-PII guarantee, locked
        // down structurally rather than by spot-checking a couple of fields).
        assert_eq!(tags.len(), 4, "unexpected tags: {tags:?}");
        assert_eq!(tags.get("model").map(String::as_str), Some("claude-opus-5"));
        assert_eq!(tags.get("provider").map(String::as_str), Some("anthropic"));
        assert_eq!(
            tags.get("outcome").map(String::as_str),
            Some("upstream_cut")
        );
        assert_eq!(
            tags.get("cut_kind").map(String::as_str),
            Some("transport_error")
        );
    }

    #[test]
    fn record_stream_failure_tags_each_cut_kind_distinctly_and_omits_it_for_an_error_event() {
        // The whole point of #310: `upstream_cut` alone conflated a clean EOF,
        // a failed body read, and a gateway-synthesized completion.
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            for cut_kind in [CutKind::Eof, CutKind::TransportError, CutKind::Marker] {
                record_stream_failure(
                    &span,
                    "anthropic",
                    "claude-opus-5",
                    StreamFailureKind::UpstreamCut,
                    &empty_context(Some(cut_kind)),
                );
            }
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::ErrorEvent,
                &empty_context(None),
            );
        });

        assert_eq!(events.len(), 4);
        let cut_kinds: Vec<Option<&str>> = events
            .iter()
            .map(|event| event.tags.get("cut_kind").map(String::as_str))
            .collect();
        assert_eq!(
            cut_kinds,
            vec![
                Some("eof"),
                Some("transport_error"),
                Some("marker"),
                // An `event: error` frame is not a cut, so it carries no
                // `cut_kind` at all rather than a misleading default.
                None
            ]
        );
    }

    #[test]
    fn record_stream_failure_attaches_the_observer_counters_as_extras() {
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &StreamFailureContext {
                    cut_kind: Some(CutKind::Eof),
                    upstream_error: None,
                    sse_events: 42,
                    bytes_forwarded: 4096,
                    last_event_type: Some("content_block_delta".to_string()),
                    elapsed_ms: 7_500,
                    ttft_ms: Some(310),
                },
            );
        });

        assert_eq!(events.len(), 1);
        let extra = &events[0].extra;
        assert_eq!(extra.get("sse_events"), Some(&42u64.into()));
        assert_eq!(extra.get("bytes_forwarded"), Some(&4096u64.into()));
        assert_eq!(extra.get("elapsed_ms"), Some(&7_500u64.into()));
        assert_eq!(extra.get("ttft_ms"), Some(&310u64.into()));
        assert_eq!(
            extra.get("last_event_type"),
            Some(&"content_block_delta".into())
        );
        // Absent rather than null when there was nothing to report.
        assert!(!extra.contains_key("upstream_error"));
        assert!(!extra.contains_key("suppressed_count"));
    }

    #[test]
    fn record_stream_failure_event_carries_exactly_the_expected_extras() {
        // The structural counterpart to the tag lockdown above. `docs/running.md`
        // promises request/response bodies, headers, credentials, and the host
        // name are never sent; pinning the exact key set is what keeps a future
        // extra from quietly breaking that, rather than spot-checking a few
        // fields and hoping.
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &StreamFailureContext {
                    cut_kind: Some(CutKind::TransportError),
                    upstream_error: Some("error reading a body: connection reset".to_string()),
                    sse_events: 3,
                    bytes_forwarded: 128,
                    last_event_type: Some("content_block_delta".to_string()),
                    elapsed_ms: 900,
                    ttft_ms: Some(120),
                },
            );
        });

        assert_eq!(events.len(), 1);
        let mut keys: Vec<&str> = events[0].extra.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bytes_forwarded",
                "elapsed_ms",
                "last_event_type",
                "sse_events",
                "ttft_ms",
                "upstream_error",
            ],
            "unexpected extras: {:?}",
            events[0].extra
        );
        let upstream_error = events[0]
            .extra
            .get("upstream_error")
            .and_then(|value| value.as_str())
            .expect("upstream_error extra must be present");
        assert!(
            !upstream_error.contains("://"),
            "an upstream URL would put a host name in the event: {upstream_error:?}"
        );
    }

    #[test]
    fn record_stream_failure_omits_ttft_when_no_body_chunk_ever_arrived() {
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &empty_context(Some(CutKind::Eof)),
            );
        });

        assert_eq!(events.len(), 1);
        assert!(!events[0].extra.contains_key("ttft_ms"));
        assert_eq!(events[0].extra.get("sse_events"), Some(&0u64.into()));
    }

    #[test]
    fn record_stream_failure_caps_and_strips_the_upstream_error_and_event_type() {
        // Both come from upstream, not from this process — an oversized or
        // control-character-laden value must not reach Sentry unchanged, for
        // the same reason the model tag is sanitized.
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                "claude-opus-5",
                StreamFailureKind::UpstreamCut,
                &StreamFailureContext {
                    cut_kind: Some(CutKind::TransportError),
                    upstream_error: Some(format!("connection reset\n{}", "e".repeat(4096))),
                    last_event_type: Some(format!("evt\r{}", "n".repeat(4096))),
                    ..Default::default()
                },
            );
        });

        assert_eq!(events.len(), 1);
        let extra = &events[0].extra;
        let error = extra
            .get("upstream_error")
            .and_then(|value| value.as_str())
            .expect("upstream_error extra must be present");
        assert_eq!(error.chars().count(), MAX_UPSTREAM_ERROR_LEN);
        assert!(error.starts_with("connection reset"));
        assert!(!error.contains('\n'), "control characters must be stripped");
        let event_type = extra
            .get("last_event_type")
            .and_then(|value| value.as_str())
            .expect("last_event_type extra must be present");
        assert_eq!(event_type.chars().count(), MAX_EVENT_TYPE_LEN);
        assert!(!event_type.contains('\r'));
    }

    #[test]
    fn record_stream_failure_sanitizes_an_oversized_client_supplied_model_tag() {
        let _throttle = throttle::test_support::scoped();
        let span = tracing::Span::none();
        let oversized_model = format!("{}-with-a-trailing-newline\n", "m".repeat(200));
        let events = sentry::test::with_captured_events(|| {
            record_stream_failure(
                &span,
                "anthropic",
                &oversized_model,
                StreamFailureKind::ErrorEvent,
                &empty_context(None),
            );
        });

        assert_eq!(events.len(), 1);
        let tag = events[0]
            .tags
            .get("model")
            .expect("model tag must be present");
        assert_eq!(tag.chars().count(), MAX_MODEL_TAG_LEN);
        assert!(!tag.contains('\n'), "control characters must be stripped");
    }
}
