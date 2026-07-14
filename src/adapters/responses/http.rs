//! The HTTP Responses transport: send the request, then relay the upstream
//! answer to the client as Anthropic SSE or a single JSON body. The default
//! path for every provider and the fallback when the websocket transport fails
//! to connect (see [`super::forward`]).

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};
use serde_json::Value;

use crate::{
    adapters::AdapterError,
    auth::Credential,
    config::AuthMode,
    model::responses::{parse_sse_events, AnthropicSseMachine, ResponseEvent},
    routing::Route,
    server::AppState,
};

use super::error::{mapped_upstream_error, own_error};
use super::request::request_builder;

/// Send the upstream Responses HTTP request and return the raw response
/// without judging its status. Split out of [`forward_http`] so the account
/// pool path ([`forward_chatgpt_oauth`]) can classify a response for failover
/// before deciding whether to relay, retry, or rotate — while single-account
/// callers still get byte-identical behavior through the [`forward_http`]
/// wrapper below.
pub(super) async fn http_send(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
    upstream_body: &Value,
) -> Result<reqwest::Response, AdapterError> {
    request_builder(state, route, credential, session_id)
        .body(upstream_body.to_string())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))
}

/// Drive a turn over the HTTP Responses path. The default transport for every
/// provider, and the fallback when the opt-in websocket transport fails to
/// connect (see [`forward`]).
#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_http(
    state: &AppState,
    route: &Route,
    upstream_body: Value,
    credential: Credential,
    auth: AuthMode,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
    estimate_input: Option<Arc<Value>>,
    session_id: Option<&str>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    // Kick off the CPU-bound tiktoken encode on the blocking pool *before* the
    // upstream request so it overlaps that round-trip; the result is not needed
    // until the response stream (and thus message_start) begins. `None` on
    // non-streaming turns and non-tiktoken providers (gated in `forward`).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    // Send via the shared helper (extracted so the account-pool path can classify
    // a response before relaying); it wraps the same request_builder + send used
    // above the merge with #112's estimate overlap.
    let upstream = http_send(state, route, credential, session_id, &upstream_body).await?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(mapped_upstream_error(status, upstream, auth).await);
    }
    if client_wants_stream {
        let input_tokens_estimate = match estimate_handle {
            Some(handle) => handle.await.unwrap_or(0),
            None => 0,
        };
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_response(
                upstream,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
                input_tokens_estimate,
                keepalive,
            ),
        ))
    } else {
        Ok((
            StatusCode::OK,
            json_response(
                upstream,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
            )
            .await?,
        ))
    }
}

pub(super) fn stream_response(
    upstream: reqwest::Response,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
    input_tokens_estimate: u64,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let parser = SseParser::default();
    let machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native)
        .with_input_estimate(input_tokens_estimate);
    let output = stream::unfold((bytes, parser, machine, false), |state| async move {
        let (mut bytes, mut parser, mut machine, mut finished) = state;
        if finished {
            return None;
        }
        loop {
            match bytes.next().await {
                Some(Ok(chunk)) => {
                    let events = parser.push(&chunk);
                    let data = events
                        .into_iter()
                        .flat_map(|event| machine.apply(event))
                        .collect::<String>();
                    if !data.is_empty() {
                        return Some((
                            Ok::<_, reqwest::Error>(Bytes::from(data)),
                            (bytes, parser, machine, false),
                        ));
                    }
                }
                Some(Err(error)) => return Some((Err(error), (bytes, parser, machine, true))),
                None => {
                    let data = machine.finish().join("");
                    finished = true;
                    if data.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(data)), (bytes, parser, machine, finished)));
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

pub(super) async fn json_response(
    upstream: reqwest::Response,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> Result<axum::response::Response, AdapterError> {
    let body = upstream
        .text()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let mut machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native);
    for event in parse_sse_events(&body) {
        let _ = machine.apply(event);
    }
    Ok((StatusCode::OK, axum::Json(machine.final_json())).into_response())
}

/// Frame-buffers the upstream SSE byte stream. Buffering raw bytes — rather than
/// decoding each transport chunk with `from_utf8_lossy` — keeps a multi-byte
/// UTF-8 code point intact when it straddles a chunk boundary: the incomplete
/// trailing bytes stay in the buffer until the next chunk completes them. Frame
/// boundaries are the ASCII `\n\n`, which can never fall inside a multi-byte
/// sequence, so every extracted frame is already complete UTF-8.
#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<ResponseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(index) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            // Drain through the frame terminator so the decoded frame keeps its
            // trailing `\n\n`, matching what `parse_sse_events` expects.
            let frame: Vec<u8> = self.buffer.drain(..index + 2).collect();
            out.extend(parse_sse_events(&String::from_utf8_lossy(&frame)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multi-byte code point split across two transport chunks must survive
    /// intact. Decoding each chunk with `from_utf8_lossy` in isolation would
    /// replace the straddling bytes with U+FFFD; buffering raw bytes until a
    /// frame boundary keeps the text whole.
    #[test]
    fn sse_parser_preserves_multibyte_char_split_across_chunks() {
        let frame = "event: delta\ndata: {\"text\":\"안녕\"}\n\n";
        // Split one byte into the 3-byte '녕' so the first chunk ends
        // mid-code-point.
        let split = frame.find('녕').unwrap() + 1;
        let (head, tail) = frame.as_bytes().split_at(split);

        let mut parser = SseParser::default();
        // No frame boundary yet, and the incomplete byte must be held back
        // rather than decoded and corrupted.
        assert!(parser.push(head).is_empty());

        let events = parser.push(tail);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data["text"], "안녕");
    }

    /// A frame that arrives split at an arbitrary ASCII byte still parses once
    /// the terminator lands, and only completed frames are emitted per push.
    #[test]
    fn sse_parser_emits_only_completed_frames() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: a\ndata: {\"n\":1}\n").is_empty());
        let events = parser.push(b"\nevent: b\ndata: {\"n\":2}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data["n"], 1);
        assert_eq!(events[1].data["n"], 2);
    }
}
