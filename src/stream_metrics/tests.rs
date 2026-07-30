use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{header::CONTENT_TYPE, Response},
};
use futures_util::{future::FutureExt, stream, StreamExt};
use serde_json::json;
use tracing_subscriber::{
    layer::{Context as LayerContext, SubscriberExt},
    Layer,
};

use super::{observe_response, ObserverState, Outcome, Protocol, MAX_EVENT_BYTES};

fn state(protocol: Protocol) -> ObserverState {
    ObserverState::new(
        protocol,
        "provider".to_string(),
        "model".to_string(),
        Instant::now(),
        tracing::Span::none(),
    )
}

/// A `Visit` that stringifies every recorded field into a shared map — the
/// same pattern `observability::tests` uses to assert on `Span::record`
/// calls independent of any particular exporter. Duplicated here rather than
/// shared: `observability`'s copy is private to its own `#[cfg(test)]`
/// module.
struct CapturingVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for CapturingVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

/// A minimal `Layer` that records every `Span::record(...)` call into a
/// shared map, so a test can assert `ObserverState::finish` actually reached
/// `crate::observability::record_stream_failure`'s `span.record(...)` call
/// through the real `observe_response` → stream-poll → `finish` path, not
/// just by calling the function directly.
#[derive(Clone, Default)]
struct CapturingLayer(Arc<Mutex<HashMap<String, String>>>);

impl<S: tracing::Subscriber> Layer<S> for CapturingLayer {
    fn on_record(
        &self,
        _id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: LayerContext<'_, S>,
    ) {
        let mut map = self.0.lock().expect("capturing layer mutex poisoned");
        values.record(&mut CapturingVisitor(&mut map));
    }
}

fn anth_event(name: &str, data: serde_json::Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

#[test]
fn parses_anthropic_events_split_across_chunks() {
    let mut observer = state(Protocol::Anthropic);
    let event = anth_event(
        "message_start",
        json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 10,
                "output_tokens": 1,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 4
            }}
        }),
    );
    for bytes in event.as_bytes().chunks(7) {
        observer.push_bytes(bytes);
    }
    observer.push_bytes(
        anth_event(
            "message_delta",
            json!({"type": "message_delta", "usage": {"output_tokens": 21}}),
        )
        .as_bytes(),
    );
    observer.push_bytes(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    assert_eq!(observer.tokens.input, Some(10));
    assert_eq!(observer.tokens.output, Some(21));
    assert_eq!(observer.tokens.cache_read, Some(3));
    assert_eq!(observer.tokens.cache_creation, Some(4));
    assert_eq!(observer.outcome(true), Outcome::Completed);
}

#[test]
fn message_delta_updates_any_input_fields_it_reports() {
    let mut observer = state(Protocol::Anthropic);
    observer.push_bytes(
        anth_event(
            "message_delta",
            json!({"usage": {
                "input_tokens": 15,
                "output_tokens": 8,
                "cache_read_input_tokens": 6,
                "cache_creation_input_tokens": 2
            }}),
        )
        .as_bytes(),
    );

    assert_eq!(observer.tokens.input, Some(15));
    assert_eq!(observer.tokens.output, Some(8));
    assert_eq!(observer.tokens.cache_read, Some(6));
    assert_eq!(observer.tokens.cache_creation, Some(2));
}

#[test]
fn parses_crlf_boundaries() {
    let mut observer = state(Protocol::Anthropic);
    observer.push_bytes(b"event: message_st");
    observer.push_bytes(b"op\r\ndata: {\"type\":\"message_stop\"}\r\n");
    assert!(!observer.terminal_seen);
    observer.push_bytes(b"\r\n");
    assert!(observer.terminal_seen);
}

#[test]
fn mixed_boundaries_are_processed_in_wire_order() {
    let mut observer = state(Protocol::Anthropic);
    observer.push_bytes(b"event: message_stop\r\ndata: {}\r\n\r\nevent: error\ndata: {}\n\n");
    assert!(observer.terminal_seen);
    assert!(observer.error_seen);
    assert_eq!(observer.outcome(true), Outcome::ErrorEvent);
}

#[test]
fn error_event_takes_precedence_over_terminal_and_end() {
    let mut observer = state(Protocol::Anthropic);
    observer.push_bytes(b"event: error\ndata: {\"type\":\"error\"}\n\n");
    observer.push_bytes(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    assert_eq!(observer.outcome(true), Outcome::ErrorEvent);
    assert_eq!(observer.outcome(false), Outcome::ErrorEvent);
}

#[test]
fn distinguishes_upstream_cut_and_client_disconnect() {
    let observer = state(Protocol::Anthropic);
    assert_eq!(observer.outcome(true), Outcome::UpstreamCut);
    assert_eq!(observer.outcome(false), Outcome::ClientDisconnect);
}

#[test]
fn parses_responses_completion_usage_and_done() {
    let mut observer = state(Protocol::Responses);
    observer.push_bytes(
        format!(
            "event: response.completed\ndata: {}\n\n",
            json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 30,
                "output_tokens": 12,
                "input_tokens_details": {"cached_tokens": 7}
            }}})
        )
        .as_bytes(),
    );

    assert_eq!(observer.tokens.input, Some(30));
    assert_eq!(observer.tokens.output, Some(12));
    assert_eq!(observer.tokens.cache_read, Some(7));
    assert_eq!(observer.tokens.cache_creation, None);
    assert_eq!(observer.outcome(true), Outcome::Completed);

    let mut done = state(Protocol::Responses);
    done.push_bytes(b"data: [DONE]\n\n");
    assert_eq!(done.outcome(true), Outcome::Completed);
}

#[test]
fn responses_failure_is_an_error_event() {
    let mut observer = state(Protocol::Responses);
    observer.push_bytes(b"event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n");
    assert_eq!(observer.outcome(true), Outcome::ErrorEvent);
}

#[test]
fn ping_and_content_deltas_are_ignored() {
    let mut observer = state(Protocol::Anthropic);
    observer.push_bytes(b"event: ping\ndata: {\"type\": \"ping\"}\n\n");
    observer.push_bytes(b"event: content_block_delta\ndata: not-json\n\n");
    assert_eq!(observer.tokens, Default::default());
    assert!(!observer.terminal_seen);
    assert!(!observer.error_seen);
}

#[test]
fn oversized_event_is_skipped_and_parsing_resumes() {
    let mut observer = state(Protocol::Anthropic);
    let mut oversized = b"event: message_start\ndata: ".to_vec();
    oversized.resize(MAX_EVENT_BYTES + 100, b'x');
    observer.push_bytes(&oversized);
    assert!(observer.skipping_oversized);

    observer.push_bytes(b"\n\nevent: message_stop\ndata: {}\n\n");
    assert!(!observer.skipping_oversized);
    assert!(observer.terminal_seen);
    assert_eq!(observer.tokens, Default::default());
}

#[test]
fn oversized_crlf_event_is_skipped_and_parsing_resumes() {
    let mut observer = state(Protocol::Anthropic);
    let mut oversized = b"event: message_start\r\ndata: ".to_vec();
    oversized.resize(MAX_EVENT_BYTES + 100, b'x');
    observer.push_bytes(&oversized);
    assert!(observer.skipping_oversized);

    observer.push_bytes(b"\r\n\r\nevent: message_stop\r\ndata: {}\r\n\r\n");
    assert!(!observer.skipping_oversized);
    assert!(observer.terminal_seen);
    assert_eq!(observer.tokens, Default::default());
}

#[tokio::test]
async fn wrapper_forwards_all_chunks_verbatim_and_preserves_headers() {
    let chunks = vec![
        Ok::<_, Infallible>(Bytes::from_static(b"event: message_")),
        Ok(Bytes::from_static(b"stop\ndata: {}\n")),
        Ok(Bytes::from_static(b"\n")),
    ];
    let response = Response::builder()
        .header(CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header("x-test", "kept")
        .body(Body::from_stream(stream::iter(chunks)))
        .unwrap();
    let wrapped = observe_response(
        response,
        Protocol::Anthropic,
        "provider".to_string(),
        "model".to_string(),
        Instant::now(),
    );

    assert_eq!(wrapped.headers()["x-test"], "kept");
    let bytes = to_bytes(wrapped.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&bytes[..], b"event: message_stop\ndata: {}\n\n");
}

#[tokio::test]
async fn dropping_body_mid_stream_exercises_client_disconnect_path() {
    let first = Ok::<_, Infallible>(Bytes::from_static(
        b"event: content_block_delta\ndata: {}\n\n",
    ));
    let upstream = stream::once(async { first }).chain(stream::pending());
    let response = Response::builder()
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(upstream))
        .unwrap();
    let wrapped = observe_response(
        response,
        Protocol::Anthropic,
        "provider".to_string(),
        "model".to_string(),
        Instant::now(),
    );
    let mut body_stream = wrapped.into_body().into_data_stream();
    assert_eq!(
        body_stream.next().await.unwrap().unwrap(),
        Bytes::from_static(b"event: content_block_delta\ndata: {}\n\n")
    );
    drop(body_stream);
}

// `ObserverState::finish` is the only caller of
// `crate::observability::record_stream_failure` (see `finish` and
// `Outcome::as_stream_failure`); these tests exercise it through the real
// `observe_response` → stream-poll → `finish` path (not by calling the
// observability function directly — that is covered in
// `observability::tests`) to prove the wiring itself: the hook fires for
// `ErrorEvent`/`UpstreamCut` and not for `Completed`/`ClientDisconnect`. Each
// test uses a plain `#[test]` (not `#[tokio::test]`) and drains the mock body
// with `.now_or_never()`: none of these mock streams ever produce a genuine
// `Poll::Pending`, so a single poll always resolves them, which keeps the
// capturing tracing subscriber and the captured-Sentry-events hub in scope
// for the whole synchronous call — both are thread-local / dynamic-scoped,
// so nesting an `.await` inside would require a second, real executor.

/// How the wrapped body is driven — the only axis, besides the SSE bytes and
/// the expectation, on which the `finish` wiring cases differ.
#[derive(Clone, Copy)]
enum Drive {
    /// Forward the mock stream to its end, so `finish` sees a natural end.
    ToEnd,
    /// Read the first chunk, then drop the body while the upstream is still
    /// open (`stream::pending()` never resolves) — the client-disconnect path,
    /// exercised via `ObservedStream`'s `Drop` impl calling `finish(false)`.
    DropAfterFirstChunk,
}

/// What `ObserverState::finish` must have wired up once the stream is over.
struct FinishWiring {
    /// The `otel.status_code` the request span carries afterwards. Every case
    /// starts from `"ok"`, mirroring what `record_span_outcome` recorded at
    /// response-header time for the `200` that opened the stream: a mid-stream
    /// failure has to overwrite it, anything else has to leave it untouched.
    span_status: &'static str,
    /// The single Sentry event `record_stream_failure` emits, or `None` when
    /// the outcome is not a failure and nothing at all may be captured.
    event: Option<(sentry::Level, &'static str)>,
}

/// Runs one wiring case: wrap an SSE response carrying `sse`, drive its body
/// per `drive`, and assert the span field and the Sentry capture match
/// `expected`.
fn assert_finish_wiring(sse: &'static [u8], drive: Drive, expected: FinishWiring) {
    let captured = CapturingLayer::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());

    let events = tracing::subscriber::with_default(subscriber, || {
        sentry::test::with_captured_events(|| {
            let span = tracing::info_span!(
                "test_stream_request",
                otel.status_code = tracing::field::Empty
            );
            let entered = span.enter();
            // Mirrors what `record_span_outcome` already recorded at
            // response-header time for the 200 that opened this stream.
            span.record("otel.status_code", "ok");
            let chunk = Ok::<_, Infallible>(Bytes::from_static(sse));
            let body = match drive {
                Drive::ToEnd => Body::from_stream(stream::iter(vec![chunk])),
                Drive::DropAfterFirstChunk => {
                    Body::from_stream(stream::once(async { chunk }).chain(stream::pending()))
                }
            };
            let response = Response::builder()
                .header(CONTENT_TYPE, "text/event-stream")
                .body(body)
                .unwrap();
            let wrapped = observe_response(
                response,
                Protocol::Anthropic,
                "provider".to_string(),
                "model".to_string(),
                Instant::now(),
            );
            drop(entered);

            match drive {
                Drive::ToEnd => {
                    to_bytes(wrapped.into_body(), usize::MAX)
                        .now_or_never()
                        .expect("mock stream resolves without a real pending state")
                        .unwrap();
                }
                Drive::DropAfterFirstChunk => {
                    let mut body_stream = wrapped.into_body().into_data_stream();
                    body_stream
                        .next()
                        .now_or_never()
                        .expect("first chunk is ready without a real pending state")
                        .unwrap()
                        .unwrap();
                    drop(body_stream);
                }
            }
        })
    });

    let fields = captured.0.lock().unwrap();
    assert_eq!(
        fields.get("otel.status_code").map(String::as_str),
        Some(expected.span_status),
        "finish must leave otel.status_code as {:?}, which header time recorded as \"ok\"",
        expected.span_status
    );
    drop(fields);

    match expected.event {
        Some((level, message)) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].level, level);
            assert_eq!(events[0].message.as_deref(), Some(message));
        }
        None => assert!(events.is_empty()),
    }
}

#[test]
fn finish_records_otel_error_and_emits_a_sentry_event_for_a_mid_stream_error_event() {
    assert_finish_wiring(
        b"event: error\ndata: {}\n\n",
        Drive::ToEnd,
        FinishWiring {
            span_status: "error",
            event: Some((
                sentry::Level::Error,
                "upstream SSE stream sent an error event mid-stream",
            )),
        },
    );
}

#[test]
fn finish_records_otel_error_and_emits_a_sentry_event_for_an_upstream_cut() {
    // A content delta with no terminal/error event, then the upstream simply
    // ends the stream (`natural_end = true`) — `UpstreamCut`.
    assert_finish_wiring(
        b"event: content_block_delta\ndata: {}\n\n",
        Drive::ToEnd,
        FinishWiring {
            span_status: "error",
            event: Some((
                sentry::Level::Warning,
                "upstream SSE stream was cut before a terminal event",
            )),
        },
    );
}

#[test]
fn finish_does_not_touch_the_span_or_emit_an_event_for_a_normal_completion() {
    // Untouched since header time: `record_stream_failure` never runs, so the
    // field still holds whatever the (simulated) header-time call recorded.
    assert_finish_wiring(
        b"event: message_stop\ndata: {}\n\n",
        Drive::ToEnd,
        FinishWiring {
            span_status: "ok",
            event: None,
        },
    );
}

#[test]
fn finish_does_not_touch_the_span_or_emit_an_event_for_a_client_disconnect() {
    assert_finish_wiring(
        b"event: content_block_delta\ndata: {}\n\n",
        Drive::DropAfterFirstChunk,
        FinishWiring {
            span_status: "ok",
            event: None,
        },
    );
}

#[tokio::test]
async fn non_sse_body_is_left_untouched() {
    let response = Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .header("content-length", "2")
        .body(Body::from("{}"))
        .unwrap();
    let response = observe_response(
        response,
        Protocol::Responses,
        "provider".to_string(),
        "model".to_string(),
        Instant::now(),
    );
    assert_eq!(response.headers()["content-length"], "2");
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "{}"
    );
}

#[test]
fn stream_failure_labels_match_outcome_labels() {
    // The Sentry `outcome` tag and the metrics `stream_outcome` label are
    // produced by two independent `as_str` functions; they must agree or the
    // two surfaces stop joining.
    for outcome in [
        Outcome::Completed,
        Outcome::ErrorEvent,
        Outcome::UpstreamCut,
        Outcome::ClientDisconnect,
    ] {
        if let Some(kind) = outcome.as_stream_failure() {
            assert_eq!(outcome.as_str(), kind.as_str());
        }
    }
}
