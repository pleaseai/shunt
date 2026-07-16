pub mod agent;
pub mod connect;
pub mod model;
pub mod request;
pub mod sse;
// Retained pending #170 follow-up: the old `api2.cursor.sh` proto/transport and
// tool-bridge machinery are bound to the decommissioned wire format and are off
// the live path. Kept (not deleted) so the tool-bridge/image work can be ported
// once the new agent wire is reverse-engineered. `allow(dead_code)` keeps the
// warnings-as-errors build green until then.
#[allow(dead_code)]
pub mod client;
#[allow(dead_code)]
pub mod proto;
#[allow(dead_code)]
pub mod response;
#[allow(dead_code)]
pub mod stream;
#[cfg(test)]
pub(crate) mod test_frames;
#[allow(dead_code)]
pub mod tool_bridge;
#[allow(dead_code)]
pub mod tool_use_xml;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream as futures_stream, StreamExt};
use serde_json::Value;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{resolve_credential, Credential},
    error::ShuntError,
    routing::Route,
    server::AppState,
};

use self::{
    agent::{CursorAgentClient, CursorAgentTurn},
    response::CursorStreamEvent,
    sse::{format_sse_error, CursorSseFramer},
};

pub struct CursorAdapter;

impl Adapter for CursorAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        _uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        let _ = headers;
        Box::pin(async move { forward(state, route, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let request: Value = serde_json::from_slice(&body).map_err(|error| {
        own_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("invalid JSON request: {error}"),
        )
    })?;
    let model = route.upstream_model.as_str();
    let resolved = model::resolve_cursor_model(model).map_err(|error| {
        own_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Model {model:?} is not supported: {error}"),
        )
    })?;
    let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let access_token = match credential {
        Credential::CursorOauth { access_token } => access_token,
        _ => {
            return Err(own_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Cursor provider requires auth = \"cursor_oauth\"",
            ))
        }
    };
    let prompt = request::render_cursor_prompt(&request);
    let want_stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The env context frame carries the working directory; the gateway has no
    // per-request workspace, so use the process cwd (falling back to "/").
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "/".to_string());

    let client = CursorAgentClient::new(state.http_client.clone());
    // `open_turn` returns once the response headers arrive, keeping the paced
    // request stream open behind the returned turn. It is not wrapped in the
    // shared `send_with_retry` (which is typed to `reqwest::Response`); a
    // connection blip surfaces to the client. TODO(#170): bounded pre-response
    // retry for the streaming turn.
    let turn = client
        .open_turn(&access_token, &prompt, &resolved.model_id, &cwd)
        .await
        .map_err(map_client_error)?;
    if !turn.status().is_success() {
        return Err(map_upstream_error(turn.into_response()).await);
    }

    if !want_stream {
        return aggregate_turn(turn, &message_id, model).await;
    }

    let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
    Ok((
        StatusCode::OK,
        streaming_response(turn, message_id, model.to_string(), keepalive),
    ))
}

/// Collect a full turn into a non-streaming Anthropic message JSON.
async fn aggregate_turn(
    turn: CursorAgentTurn,
    message_id: &str,
    model: &str,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let mut events = std::pin::pin!(turn.into_event_stream());
    let mut text = String::new();
    while let Some(event) = events.next().await {
        match event.map_err(map_cursor_stream_error)? {
            CursorStreamEvent::TextDelta { text: delta } => text.push_str(&delta),
            CursorStreamEvent::End => break,
            // Reasoning and session markers do not surface in the text-only
            // non-streaming body.
            _ => {}
        }
    }
    let json = serde_json::json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "model": model,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 1,
            "output_tokens": 1,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    Ok((StatusCode::OK, axum::Json(json).into_response()))
}

fn streaming_response(
    turn: CursorAgentTurn,
    message_id: String,
    model: String,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let framer = CursorSseFramer::new(message_id, model);
    let events = turn.into_event_stream();
    let output = futures_stream::unfold(
        (Box::pin(events), framer, false),
        |(mut events, mut framer, done)| async move {
            if done {
                return None;
            }
            loop {
                match events.next().await {
                    Some(Ok(CursorStreamEvent::TextDelta { text })) => {
                        framer.emit_text_delta(&text);
                        let output = framer.take_output();
                        if !output.is_empty() {
                            return Some((
                                Ok::<_, std::convert::Infallible>(Bytes::from(output)),
                                (events, framer, false),
                            ));
                        }
                    }
                    Some(Ok(CursorStreamEvent::ThinkingDelta { text })) => {
                        framer.emit_thinking_delta(&text);
                        let output = framer.take_output();
                        if !output.is_empty() {
                            return Some((Ok(Bytes::from(output)), (events, framer, false)));
                        }
                    }
                    Some(Ok(CursorStreamEvent::End)) | None => {
                        framer.emit_final_message("end_turn");
                        framer.finalize();
                        return Some((
                            Ok(Bytes::from(framer.take_output())),
                            (events, framer, true),
                        ));
                    }
                    Some(Ok(CursorStreamEvent::Session { .. }))
                    | Some(Ok(CursorStreamEvent::Usage { .. })) => {}
                    Some(Err(error)) => {
                        let message = crate::model::responses::context_overflow_message(
                            &Value::Null,
                            &error.message,
                        )
                        .unwrap_or(error.message);
                        let mut output = framer.take_output();
                        output.extend_from_slice(&format_sse_error(&message));
                        return Some((Ok(Bytes::from(output)), (events, framer, true)));
                    }
                }
            }
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("valid Cursor streaming response")
        .into_response()
}

async fn map_upstream_error(upstream: reqwest::Response) -> AdapterError {
    let status = upstream.status();
    let retry_after = upstream.headers().get("retry-after").cloned();
    let grpc_message = upstream
        .headers()
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let text = upstream.text().await.unwrap_or_default();
    // Cursor may return a Connect JSON error body (`{"error":{"message":"…"}}`
    // or a bare `{"message":"…"}`). Parse it once: the body feeds both the
    // human-readable message and the context-overflow detection below.
    let body: Option<Value> = serde_json::from_str(&text).ok();
    let parsed_message = body.as_ref().and_then(|value| {
        value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    let message = grpc_message
        .or(parsed_message)
        .or_else(|| (!text.is_empty()).then_some(text))
        .unwrap_or_else(|| format!("Cursor upstream returned HTTP {status}"));
    // Reuse the Responses path's context-overflow rewrite so a Cursor
    // "context length exceeded" surfaces as Anthropic's "prompt is too long"
    // wording that triggers Claude Code's auto-compact-and-retry (see
    // `map_error_value`).
    let message = crate::model::responses::context_overflow_message(
        body.as_ref().unwrap_or(&Value::Null),
        &message,
    )
    .unwrap_or(message);
    // Shares the status -> `error.type` table with the other translated
    // backends (Responses/Codex, xAI) so Cursor surfaces the same vocabulary
    // the Anthropic-direct path streams verbatim; see
    // `docs/gateway-protocol.md#error-envelopes`.
    let mapped_status = crate::model::responses::client_facing_status(status);
    let kind = crate::model::responses::anthropic_error_type(status);
    let mut error = ShuntError::new(mapped_status, kind, message).into_response();
    if let Some(value) = retry_after {
        error.headers_mut().insert("retry-after", value);
    }
    AdapterError {
        message: format!("Cursor upstream request failed with {status}"),
        response: Box::new(error),
    }
}

fn map_client_error(error: client::CursorError) -> AdapterError {
    bad_gateway(error.to_string())
}

/// Map an error surfaced while reading a turn (a Connect end-frame error that
/// carries an upstream status) to the client, reusing the shared status ->
/// `error.type` table so a Cursor 401/403/429/5xx keeps its meaning instead of
/// flattening to a generic 502. A context-overflow message is rewritten to the
/// Anthropic "prompt is too long" wording so Claude Code auto-compacts.
fn map_cursor_stream_error(error: client::CursorError) -> AdapterError {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mapped_status = crate::model::responses::client_facing_status(status);
    let kind = crate::model::responses::anthropic_error_type(status);
    let message = crate::model::responses::context_overflow_message(&Value::Null, &error.message)
        .unwrap_or(error.message);
    own_error(mapped_status, kind, message)
}

fn bad_gateway(message: String) -> AdapterError {
    own_error(StatusCode::BAD_GATEWAY, "api_error", message)
}

fn own_error(status: StatusCode, kind: &'static str, message: impl Into<String>) -> AdapterError {
    AdapterError {
        message: "Cursor adapter failed".to_string(),
        response: Box::new(ShuntError::new(status, kind, message).into_response()),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use serde_json::Value;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    /// Serves an empty `status` response from a mock server and returns the
    /// resulting `reqwest::Response`, mirroring what `map_upstream_error`
    /// sees in production (a response read off the wire, not built
    /// in-process).
    async fn upstream_response(status: u16, headers: &[(&str, &str)]) -> reqwest::Response {
        let server = MockServer::start().await;
        let mut template = ResponseTemplate::new(status).set_body_string("boom");
        for (name, value) in headers {
            template = template.insert_header(*name, *value);
        }
        Mock::given(method("GET"))
            .and(path("/e"))
            .respond_with(template)
            .mount(&server)
            .await;
        reqwest::Client::new()
            .get(format!("{}/e", server.uri()))
            .send()
            .await
            .expect("mock request should succeed")
    }

    async fn body_json(error: AdapterError) -> Value {
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&bytes).expect("error body should be JSON")
    }

    #[tokio::test]
    async fn upstream_error_maps_403_to_permission_error() {
        let upstream = upstream_response(403, &[]).await;
        let error = map_upstream_error(upstream).await;
        assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "permission_error");
    }

    #[tokio::test]
    async fn upstream_error_maps_529_to_overloaded_error() {
        let upstream = upstream_response(529, &[]).await;
        let error = map_upstream_error(upstream).await;
        assert_eq!(error.response.status().as_u16(), 529);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "overloaded_error");
    }

    #[tokio::test]
    async fn upstream_error_preserves_503_instead_of_bad_gateway() {
        let upstream = upstream_response(503, &[]).await;
        let error = map_upstream_error(upstream).await;
        assert_eq!(error.response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn upstream_error_maps_413_to_request_too_large() {
        let upstream = upstream_response(413, &[]).await;
        let error = map_upstream_error(upstream).await;
        assert_eq!(error.response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "request_too_large");
    }

    #[tokio::test]
    async fn upstream_error_preserves_retry_after_on_429() {
        let upstream = upstream_response(429, &[("retry-after", "3")]).await;
        let error = map_upstream_error(upstream).await;
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.response.headers().get("retry-after").unwrap(), "3");
    }

    #[tokio::test]
    async fn upstream_error_rewrites_context_overflow_to_anthropic_wording() {
        // A Cursor HTTP context-overflow must surface as Anthropic's "prompt is
        // too long" wording so Claude Code auto-compacts and retries instead of
        // stranding the session on the raw upstream message.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/e"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"This model's maximum context length is 272000 tokens. However, your messages resulted in 372982 tokens."}}"#,
            ))
            .mount(&server)
            .await;
        let upstream = reqwest::Client::new()
            .get(format!("{}/e", server.uri()))
            .send()
            .await
            .expect("mock request should succeed");
        let error = map_upstream_error(upstream).await;
        let body = body_json(error).await;
        assert_eq!(
            body["error"]["message"],
            "prompt is too long: 372982 tokens > 272000 maximum"
        );
    }

    #[test]
    fn bad_gateway_and_own_error_carry_their_status() {
        assert_eq!(
            bad_gateway("boom".to_string()).response.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            own_error(StatusCode::UNAUTHORIZED, "authentication_error", "no")
                .response
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
