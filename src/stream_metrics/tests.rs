use std::{convert::Infallible, time::Instant};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{header::CONTENT_TYPE, Response},
};
use futures_util::{stream, StreamExt};
use serde_json::json;

use super::{observe_response, ObserverState, Outcome, Protocol, MAX_EVENT_BYTES};

fn state(protocol: Protocol) -> ObserverState {
    ObserverState::new(
        protocol,
        "provider".to_string(),
        "model".to_string(),
        Instant::now(),
    )
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
