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

use super::error::{backend_error_response, mapped_upstream_error, own_error};
use super::request::request_builder;

/// Send the upstream Responses HTTP request and return the raw response
/// without judging its status. Split out of [`forward_http`] so the account
/// pool path ([`forward_chatgpt_oauth`]) can classify a response for failover
/// before deciding whether to relay, retry, or rotate. Returns the raw
/// `reqwest::Error` so the bounded-retry layer can distinguish transient
/// transport failures from deterministic ones.
pub(super) async fn http_send(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
    body: bytes::Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    request_builder(state, route, credential, session_id)
        .body(body)
        .send()
        .await
}

/// The bounded-retry policy for `route`'s provider (issue #48), or a disabled
/// policy when the provider somehow isn't found (it was validated at routing).
fn provider_retry_policy(state: &AppState, route: &Route) -> crate::retry::RetryPolicy {
    state
        .config
        .provider(&route.provider)
        .map(|provider| provider.retry.policy())
        .unwrap_or(crate::retry::RetryPolicy::DISABLED)
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
    // The account-pool path drives its own failover and deliberately does not
    // layer retry on top. This single-credential path retries only before any
    // response body is handed to the streaming/JSON relay.
    let policy = provider_retry_policy(state, route);
    let body = bytes::Bytes::from(upstream_body.to_string());
    let upstream = crate::retry::send_with_retry_with_safety(
        policy,
        &route.provider,
        crate::retry::RetrySafety::NonIdempotentPost,
        || http_send(state, route, credential.clone(), session_id, body.clone()),
    )
    .await
    .map_err(|error| {
        // Preserve the raw transport cause in logs before own_error maps it to
        // the stable gateway-facing Responses error envelope.
        tracing::warn!(
            provider = %route.provider,
            error = %error,
            "responses upstream request failed after retries"
        );
        own_error(error.to_string())
    })?;
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
        // Thread the real response status: `json_response` returns a `502` when
        // a backend error event surfaced via `backend_error` (issue #113), so
        // the proxy's access log (`upstream_status`) and `record_proxied_request`
        // metrics reflect the failure instead of a hardcoded `200`.
        let response = json_response(
            upstream,
            route.model.clone(),
            thinking_enabled,
            tool_search_native,
        )
        .await?;
        Ok((response.status(), response))
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

/// Collect the full HTTP Responses SSE body into a single Anthropic message for
/// a non-streaming client. A backend-sent `error` / `response.failed` event
/// (delivered as a normal event on the `200 OK` stream — rate-limit,
/// content-policy refusal) is surfaced as a gateway error rather than a `200 OK`
/// with the partial content accumulated before it, so the client cannot mistake
/// a backend failure for a truncated-but-successful result (issue #113). This
/// mirrors the streaming path, which emits the same error inline as an SSE
/// `error` event.
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
    if let Some(error) = machine.take_backend_error() {
        return Ok(backend_error_response(error));
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
    use axum::body::to_bytes;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serves `body` at `status` from a mock server and returns the resulting
    /// `reqwest::Response`, mirroring the shape `json_response` reads in
    /// production (a response off the wire, not built in-process).
    async fn upstream_response(status: u16, body: &str) -> reqwest::Response {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/e"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
            .mount(&server)
            .await;
        reqwest::Client::new()
            .get(format!("{}/e", server.uri()))
            .send()
            .await
            .expect("mock request should succeed")
    }

    async fn response_body_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&bytes).expect("response body should be JSON")
    }

    /// A backend-sent `response.failed` event on the HTTP JSON path surfaces as a
    /// `502` gateway error rather than a `200 OK` with the partial content
    /// collected before it (issue #113).
    #[tokio::test]
    async fn json_response_surfaces_backend_error_event_as_gateway_error() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"partial\"}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let response = json_response(upstream, "gpt-5.2-codex".to_string(), false, false)
            .await
            .expect("json_response builds a response");

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response_body_json(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["message"], "Rate limit reached");
    }

    /// A clean turn still returns the collected Anthropic message as `200 OK` —
    /// the backend-error gate must not regress the success path.
    #[tokio::test]
    async fn json_response_returns_ok_for_a_clean_turn() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hello\"}\n\n",
            "event: response.output_text.done\n",
            "data: {}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let response = json_response(upstream, "gpt-5.2-codex".to_string(), false, false)
            .await
            .expect("json_response builds a response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body_json(response).await;
        assert_eq!(body["type"], "message");
        assert_eq!(body["content"][0]["text"], "hello");
    }

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
