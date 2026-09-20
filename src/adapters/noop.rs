//! `type = "noop"` — answer without an upstream call.
//!
//! A `[models.router]` entry with `type = "noop"` resolves to a route with this
//! adapter, which synthesizes an empty terminal assistant message **in the
//! caller's mode**: a valid Anthropic SSE sequence for a `stream: true` caller,
//! one Message JSON object otherwise. No HTTP client is built, no credential is
//! read, and no provider is looked up — the route's `provider` is the literal
//! `"noop"`, which names no `[providers.*]` entry.
//!
//! That last point is why [`crate::routing::AdapterKind::Noop`] is excluded
//! from the passthrough test in `proxy::failover`: an unknown provider is
//! treated as credential-injecting (fail closed), and a provider an operator
//! happened to *name* `noop` must not flip a noop route to passthrough and
//! drop `[server.auth]` for it.

use axum::{
    body::Body,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

use crate::{
    adapters::{Adapter, AdapterFuture},
    request::RequestBody,
    routing::Route,
    server::AppState,
};

pub(crate) struct NoopAdapter;

impl Adapter for NoopAdapter {
    fn forward<'a>(
        &'a self,
        _state: AppState,
        route: Route,
        _uri: &'a axum::http::Uri,
        _headers: &'a axum::http::HeaderMap,
        body: RequestBody,
        // Answers locally and reads no upstream body, so there is nothing to cap.
        _response_byte_cap: Option<usize>,
    ) -> AdapterFuture<'a> {
        let streaming = wants_stream(body.json());
        Box::pin(async move { Ok(respond(&route.model, streaming)) })
    }
}

/// Whether the caller asked for SSE.
///
/// Only a JSON `true` counts. Anthropic's own API rejects a non-boolean
/// `stream`, and reading `"true"` or `1` as streaming here would answer a
/// malformed request in a shape its client cannot parse.
pub(crate) fn wants_stream(request: &Value) -> bool {
    request.get("stream") == Some(&Value::Bool(true))
}

/// The synthesized response, in the caller's mode.
pub(crate) fn respond(model: &str, streaming: bool) -> (StatusCode, Response) {
    if streaming {
        let mut response = Response::new(Body::from(stream_bytes(model)));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        (StatusCode::OK, response)
    } else {
        let mut message = message(model);
        message["stop_reason"] = json!("end_turn");
        (StatusCode::OK, axum::Json(message).into_response())
    }
}

/// The Message object both modes report, with `stop_reason` still open.
///
/// `model` is the id the client asked for, as every other route reports it
/// (issue #172): Claude Code records `message_start.model` to restore the model
/// on `--resume`, so a synthesized turn must not hand it a different one.
fn message(model: &str) -> Value {
    json!({
        "id": message_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [],
        "stop_reason": Value::Null,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 0, "output_tokens": 0},
    })
}

/// A per-response `msg_noop_…` id.
///
/// Distinct per response rather than a constant: a client that keys a turn on
/// the message id — Claude Code does, for its own transcript — would otherwise
/// see every noop turn of a session collapse onto one entry.
fn message_id() -> String {
    use rand::Rng;
    let suffix: u64 = rand::rng().random();
    format!("msg_noop_{suffix:016x}")
}

/// `message_start` → `message_delta` → `message_stop`.
///
/// The minimum sequence an Anthropic SSE client accepts as a complete turn with
/// no content: `content_block_*` events are omitted because there is no block,
/// and `message_delta` carries the terminal `stop_reason` the client waits for.
fn stream_bytes(model: &str) -> Vec<u8> {
    use crate::adapters::cursor::sse::format_sse_event_bytes;

    let mut out = Vec::new();
    out.extend_from_slice(&format_sse_event_bytes(
        "message_start",
        &json!({"type": "message_start", "message": message(model)}),
    ));
    out.extend_from_slice(&format_sse_event_bytes(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
            "usage": {"output_tokens": 0},
        }),
    ));
    out.extend_from_slice(&format_sse_event_bytes(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    fn events(bytes: &str) -> Vec<&str> {
        bytes
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect()
    }

    #[tokio::test]
    async fn a_streaming_caller_gets_a_complete_sse_turn() {
        let (status, response) = respond("claude-quiet", true);

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(
            events(&body),
            ["message_start", "message_delta", "message_stop"],
            "the order is the contract, not just the set"
        );
        assert!(
            body.contains(r#""model":"claude-quiet""#),
            "message_start must carry the requested id, got:\n{body}"
        );
        assert!(
            body.contains(r#""stop_reason":"end_turn""#),
            "the delta must terminate the turn, got:\n{body}"
        );
    }

    #[tokio::test]
    async fn a_non_streaming_caller_gets_one_terminal_message() {
        let (status, response) = respond("claude-quiet", false);

        assert_eq!(status, StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let message: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "claude-quiet");
        assert_eq!(message["content"], json!([]));
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(message["usage"]["output_tokens"], 0);
        assert!(
            message["id"].as_str().unwrap().starts_with("msg_noop_"),
            "the id must be recognisable as synthesized"
        );
    }

    #[test]
    fn only_a_json_true_requests_streaming() {
        assert!(wants_stream(&json!({"stream": true})));
        for not_streaming in [
            json!({}),
            json!({"stream": false}),
            json!({"stream": "true"}),
        ] {
            assert!(
                !wants_stream(&not_streaming),
                "{not_streaming} must not be read as a streaming request"
            );
        }
    }
}
