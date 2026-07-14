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
                    let events = parser.push(&String::from_utf8_lossy(&chunk));
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

#[derive(Default)]
struct SseParser {
    buffer: String,
}

impl SseParser {
    fn push(&mut self, chunk: &str) -> Vec<ResponseEvent> {
        self.buffer.push_str(chunk);
        let mut out = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let frame = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            out.extend(parse_sse_events(&(frame + "\n\n")));
        }
        out
    }
}
