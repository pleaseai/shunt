pub mod client;
pub mod connect;
pub mod model;
pub mod proto;
pub mod request;
pub mod response;
pub mod sse;
pub mod stream;
#[cfg(test)]
pub(crate) mod test_frames;
pub mod tool_bridge;
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
    client::CursorHttpClient,
    response::{decode_cursor_upstream, decode_upstream_response, CursorDecodeError},
    tool_bridge::{
        advertised_tool_names, can_bridge_cursor_native_tools, find_tool_result,
        start_cursor_tool_bridge, BridgeRegistry,
    },
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
        Box::pin(async move { forward(state, route, headers, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    headers: &HeaderMap,
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
    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());

    if let Some(session_id) = session_id {
        if let Some(pending) = BridgeRegistry::pending_tool(session_id) {
            if find_tool_result(&request, pending.tool_use_id()).is_some() {
                // Resume: the client executed the pending tool and re-sent the
                // full conversation, which now includes the `tool_result`. Drop
                // the stale bridge state; the tool result reaches the upstream
                // Cursor agent through the rendered prompt history below, and its
                // fresh response is bridged like any other request (pausing again
                // if it emits another tool_use). We deliberately do NOT replay
                // the previous response's leftover events — those were generated
                // before the agent saw the tool result and would be stale.
                BridgeRegistry::remove(session_id);
            }
        }
    }

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
    let images = request::cursor_selected_images(&request);
    let base_url = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.base_url.as_str())
        .unwrap_or("https://api2.cursor.sh");
    let upstream = CursorHttpClient::new(state.http_client.clone(), base_url)
        .run_agent(&access_token, &prompt, &resolved, &images)
        .await
        .map_err(map_client_error)?;
    if !upstream.status().is_success() {
        return Err(map_upstream_error(upstream).await);
    }

    let want_stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !want_stream {
        let bytes = upstream
            .bytes()
            .await
            .map_err(|error| bad_gateway(error.to_string()))?;
        let json = decode_cursor_upstream(&bytes, &message_id, model).map_err(map_decode_error)?;
        return Ok((StatusCode::OK, axum::Json(json).into_response()));
    }

    if can_bridge_cursor_native_tools(&request, session_id) {
        let bytes = upstream
            .bytes()
            .await
            .map_err(|error| bad_gateway(error.to_string()))?;
        let events = decode_upstream_response(&bytes).map_err(map_decode_error)?;
        let (sse, _) = start_cursor_tool_bridge(
            &message_id,
            model,
            session_id.expect("bridge eligibility requires session id"),
            &events,
            advertised_tool_names(&request),
            Box::new(|| uuid::Uuid::new_v4().simple().to_string()),
        );
        return Ok((StatusCode::OK, sse_bytes_response(sse)));
    }

    let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
    Ok((
        StatusCode::OK,
        streaming_response(upstream, message_id, model.to_string(), keepalive),
    ))
}

fn streaming_response(
    upstream: reqwest::Response,
    message_id: String,
    model: String,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let machine = stream::CursorStreamMachine::new(message_id, model);
    let output = futures_stream::unfold((bytes, machine, false), |state| async move {
        let (mut bytes, mut machine, done) = state;
        if done {
            return None;
        }
        loop {
            match bytes.next().await {
                Some(Ok(chunk)) => {
                    let output = machine.push(&chunk);
                    if !output.is_empty() {
                        return Some((
                            Ok::<_, reqwest::Error>(Bytes::from(output)),
                            (bytes, machine, false),
                        ));
                    }
                }
                Some(Err(error)) => return Some((Err(error), (bytes, machine, true))),
                None => {
                    let output = machine.finish();
                    if output.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(output)), (bytes, machine, true)));
                }
            }
        }
    });
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

fn sse_bytes_response(bytes: Vec<u8>) -> axum::response::Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(bytes))
        .expect("valid Cursor SSE response")
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

fn map_decode_error(error: CursorDecodeError) -> AdapterError {
    // `error.status()` is the Connect-code-derived status from
    // `parse_connect_error` (401/403/429/502 in practice); reuse the same
    // status -> `error.type` table as `map_upstream_error` rather than a
    // second hardcoded mapping.
    let status = error
        .status()
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mapped_status = crate::model::responses::client_facing_status(status);
    let kind = crate::model::responses::anthropic_error_type(status);
    // A model context-overflow can arrive as a Connect error frame; surface the
    // Connect code so the shared rewrite fires even when the message text lacks
    // the OpenAI-style phrasing, matching the Responses path's auto-compact hook.
    let value = match &error {
        CursorDecodeError::ConnectEnd(err) => serde_json::json!({ "error": { "code": err.code } }),
        CursorDecodeError::Decode(_) => Value::Null,
    };
    let raw = error.to_string();
    let message = crate::model::responses::context_overflow_message(&value, &raw).unwrap_or(raw);
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
    use crate::adapters::cursor::connect::ConnectEndError;

    fn connect_end(status: u16) -> CursorDecodeError {
        CursorDecodeError::ConnectEnd(ConnectEndError {
            code: "x".to_string(),
            message: "boom".to_string(),
            detail: "boom".to_string(),
            status,
        })
    }

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

    #[test]
    fn decode_error_maps_auth_rate_limit_and_permission_statuses() {
        assert_eq!(
            map_decode_error(connect_end(401)).response.status(),
            StatusCode::UNAUTHORIZED
        );
        // Connect's `permission_denied` (403) is "authenticated but not
        // allowed" — a distinct error from 401 that must not be folded into
        // `authentication_error`.
        assert_eq!(
            map_decode_error(connect_end(403)).response.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            map_decode_error(connect_end(429)).response.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn decode_error_preserves_upstream_5xx_status() {
        // A real upstream 500 must reach the client as 500, not flattened to
        // a generic 502 that hides the actual signal.
        assert_eq!(
            map_decode_error(connect_end(500)).response.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn decode_error_defaults_to_bad_gateway_for_unmapped_status() {
        assert_eq!(
            map_decode_error(connect_end(418)).response.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            map_decode_error(CursorDecodeError::Decode("nope".to_string()))
                .response
                .status(),
            StatusCode::BAD_GATEWAY
        );
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

    #[tokio::test]
    async fn decode_error_rewrites_context_overflow_to_anthropic_wording() {
        // The same rewrite must fire when the overflow arrives as a Connect
        // error frame (the streaming path), not just an HTTP error.
        let error = map_decode_error(CursorDecodeError::ConnectEnd(ConnectEndError {
            code: "context_length_exceeded".to_string(),
            message: "This model's maximum context length is 272000 tokens. \
                      However, your messages resulted in 372982 tokens."
                .to_string(),
            detail: String::new(),
            status: 400,
        }));
        assert_eq!(error.response.status(), StatusCode::BAD_REQUEST);
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
