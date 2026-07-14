//! Relay a Codex websocket event stream to the client — as Anthropic SSE for a
//! streaming client or a single collected JSON message for a non-streaming one.
//! The first event has already been peeked (see [`super::websocket::open_ws_turn`]),
//! so a transport error here is genuinely mid-stream.

use axum::{
    body::{Body, Bytes},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use futures_util::stream;
use serde_json::json;

use crate::{
    error::ShuntError,
    model::responses::{map_error_value, AnthropicSseMachine},
};

use super::codex_ws::{CodexWsError, CodexWsEvents};
use super::error::backend_error_response;
use super::websocket::BufferedEvent;

/// Stream translated events to the client as Anthropic SSE. Mirrors
/// [`stream_response`] but reads from the websocket event channel. By the time
/// this runs the first event has already been delivered (peeked in
/// [`open_ws_turn`], replayed here via `buffered`), so a transport error at this
/// point is genuinely mid-stream: it is surfaced as an Anthropic `error` event so
/// the client sees a reason rather than a silent truncation — an HTTP restart is
/// no longer safe because output has already been streamed.
pub(super) fn stream_events_response(
    buffered: BufferedEvent,
    events: CodexWsEvents,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
    input_tokens_estimate: u64,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native)
        .with_input_estimate(input_tokens_estimate);
    let output = stream::unfold(
        (buffered, events, machine, false),
        |(mut buffered, mut events, mut machine, finished)| async move {
            if finished {
                return None;
            }
            loop {
                let item = match buffered.take() {
                    Some(item) => Some(item),
                    None => events.recv().await,
                };
                match item {
                    Some(Ok(event)) => {
                        let data = machine.apply(event).into_iter().collect::<String>();
                        if !data.is_empty() {
                            return Some((
                                Ok::<_, std::convert::Infallible>(Bytes::from(data)),
                                (buffered, events, machine, false),
                            ));
                        }
                    }
                    Some(Err(error)) => {
                        return Some((
                            Ok(Bytes::from(ws_error_sse(&error))),
                            (buffered, events, machine, true),
                        ));
                    }
                    None => {
                        let data = machine.finish().join("");
                        if data.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(data)), (buffered, events, machine, true)));
                    }
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

/// Collect the full websocket event stream into a single Anthropic message for a
/// non-streaming client. The first event was peeked in [`open_ws_turn`] (a
/// pre-first-event failure already fell back to HTTP), so a transport error here
/// is mid-stream: return a gateway error instead of presenting partial output as
/// a successful response. `buffered` is the replayed first event, if any.
///
/// A backend-sent error *event* (arriving as `Ok`, e.g. rate-limit or
/// content-policy refusal) is likewise surfaced as a gateway error rather than a
/// `200 OK` with the partial content collected before it (issue #113): the
/// machine records the mapped error envelope. Because such an event is terminal
/// (the machine ignores everything after it), the collector returns the moment
/// the envelope is recorded rather than draining to channel close — a backend
/// that sends the error but holds the socket open would otherwise hang the
/// request on `recv()`. This matches both the mid-stream *transport* error above
/// and the streaming path, which emits the same error inline as an SSE `error`
/// event.
pub(super) async fn json_events_response(
    buffered: BufferedEvent,
    mut events: CodexWsEvents,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> axum::response::Response {
    let mut machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native);
    let mut buffered = buffered;
    loop {
        let item = match buffered.take() {
            Some(item) => Some(item),
            None => events.recv().await,
        };
        match item {
            Some(Ok(event)) => {
                let _ = machine.apply(event);
                // A backend error event is terminal: the machine records the
                // mapped envelope and ignores everything after. Return the moment
                // it lands instead of looping on `recv()` for a channel close the
                // backend may never send — that would hang the request.
                if let Some(error) = machine.take_backend_error() {
                    return backend_error_response(error);
                }
            }
            Some(Err(error)) => {
                tracing::warn!(error = %error.message, "codex websocket stream error");
                let message = if error.body.is_empty() {
                    error.message
                } else {
                    error.body
                };
                return ShuntError::bad_gateway(message).into_response();
            }
            None => break,
        }
    }
    (StatusCode::OK, axum::Json(machine.final_json())).into_response()
}

/// Render a websocket transport error as an Anthropic `error` SSE event.
fn ws_error_sse(error: &CodexWsError) -> String {
    let message = if error.body.is_empty() {
        error.message.clone()
    } else {
        error.body.clone()
    };
    let value = map_error_value(&json!({ "message": message }), StatusCode::BAD_GATEWAY);
    format!("event: error\ndata: {value}\n\n")
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    use crate::adapters::responses::codex_ws::CodexWsError;
    use crate::model::responses::ResponseEvent;

    use super::{json_events_response, stream_events_response, ws_error_sse};

    fn transport_error(body: &str, message: &str) -> CodexWsError {
        CodexWsError {
            status: None,
            retry_after: None,
            body: body.to_string(),
            message: message.to_string(),
            previous_response_missing: false,
        }
    }

    fn created_event() -> ResponseEvent {
        ResponseEvent {
            event: Some("response.created".to_string()),
            data: json!({ "response": { "id": "resp_1" } }),
        }
    }

    #[test]
    fn ws_error_sse_prefers_body_then_falls_back_to_message() {
        // A non-empty body wins over the internal message.
        let sse = ws_error_sse(&transport_error("upstream body detail", "internal log msg"));
        assert!(sse.starts_with("event: error\ndata: "));
        assert!(sse.ends_with("\n\n"));
        assert!(sse.contains("upstream body detail"));
        assert!(!sse.contains("internal log msg"));

        // An empty body falls back to the internal message.
        let sse = ws_error_sse(&transport_error("", "fallback message"));
        assert!(sse.contains("fallback message"));
    }

    #[tokio::test]
    async fn json_events_response_surfaces_mid_stream_transport_error_as_bad_gateway() {
        // A transport error mid-stream must not be presented as a successful
        // partial answer — it is surfaced as a gateway error carrying the body.
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Err(transport_error("upstream blew up", "socket dropped")))
            .unwrap();
        drop(tx);

        let response =
            json_events_response(None, rx, "gpt-5.2-codex".to_string(), false, false).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.to_string().contains("upstream blew up"));
    }

    #[tokio::test]
    async fn json_events_response_collects_ok_events_then_finishes() {
        // The `Ok` and channel-closed (`None`) arms produce a 200 message.
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Ok(created_event())).unwrap();
        tx.send(Ok(ResponseEvent {
            event: Some("response.completed".to_string()),
            data: json!({ "response": { "id": "resp_1" } }),
        }))
        .unwrap();
        drop(tx);

        let response =
            json_events_response(None, rx, "gpt-5.2-codex".to_string(), false, false).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A backend-sent error *event* (an `Ok` on the channel, distinct from a
    /// transport `Err`) is surfaced as a `502` gateway error, mirroring the HTTP
    /// `json_response` path (issue #113). A partial content delta precedes the
    /// failure so the "error after real content" case is exercised too.
    #[tokio::test]
    async fn json_events_response_surfaces_backend_error_event_as_gateway_error() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Ok(created_event())).unwrap();
        tx.send(Ok(ResponseEvent {
            event: Some("response.output_text.delta".to_string()),
            data: json!({ "delta": "partial" }),
        }))
        .unwrap();
        tx.send(Ok(ResponseEvent {
            event: Some("response.failed".to_string()),
            data: json!({
                "type": "response.failed",
                "response": { "error": { "code": "rate_limit_exceeded", "message": "Rate limit reached" } }
            }),
        }))
        .unwrap();
        drop(tx);

        let response =
            json_events_response(None, rx, "gpt-5.2-codex".to_string(), false, false).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["message"], "Rate limit reached");
    }

    /// The collector must not block waiting for the channel to close after a
    /// backend error event — a backend can send `response.failed` and hold the
    /// socket open. Keep the sender alive (no `drop(tx)`) and assert the collector
    /// still returns a `502` promptly instead of hanging on `recv()`.
    #[tokio::test]
    async fn json_events_response_returns_on_backend_error_without_channel_close() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Ok(created_event())).unwrap();
        tx.send(Ok(ResponseEvent {
            event: Some("response.failed".to_string()),
            data: json!({
                "type": "response.failed",
                "response": { "error": { "code": "rate_limit_exceeded", "message": "Rate limit reached" } }
            }),
        }))
        .unwrap();
        // Deliberately keep `tx` alive: the channel never closes, so the collector
        // must return on the error event rather than looping forever on `recv()`.

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            json_events_response(None, rx, "gpt-5.2-codex".to_string(), false, false),
        )
        .await
        .expect("collector returns without waiting for channel close");

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["message"], "Rate limit reached");

        drop(tx);
    }

    #[tokio::test]
    async fn stream_events_response_emits_error_event_on_mid_stream_failure() {
        // Mid-stream transport errors become an Anthropic `error` SSE event so the
        // client sees a reason instead of a silent truncation.
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Err(transport_error("mid stream boom", "socket dropped")))
            .unwrap();
        drop(tx);

        let response = stream_events_response(
            None,
            rx,
            "gpt-5.2-codex".to_string(),
            false,
            false,
            0,
            std::time::Duration::from_secs(15),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: error"));
        assert!(text.contains("mid stream boom"));
    }

    #[tokio::test]
    async fn stream_events_response_replays_buffered_event_and_flushes_on_close() {
        // The peeked first event (buffered) is replayed and, once the channel
        // closes, `machine.finish()` flushes the terminal Anthropic events.
        let (tx, rx) = mpsc::unbounded_channel::<Result<ResponseEvent, CodexWsError>>();
        drop(tx); // channel closed: only the buffered event drives output

        let response = stream_events_response(
            Some(Ok(created_event())),
            rx,
            "gpt-5.2-codex".to_string(),
            false,
            false,
            42,
            std::time::Duration::from_secs(15),
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        // `response.created` opens the stream with `message_start`.
        assert!(text.contains("message_start"));
    }
}
