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
        resume_cursor_tool_bridge, start_cursor_tool_bridge, BridgeRegistry,
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
            if let Some(result) = find_tool_result(&request, pending.tool_use_id()) {
                let (_, bytes) =
                    resume_cursor_tool_bridge(session_id, &message_id, model, result, &pending);
                return Ok((StatusCode::OK, sse_bytes_response(bytes)));
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
    let message = grpc_message
        .or_else(|| (!text.is_empty()).then_some(text))
        .unwrap_or_else(|| format!("Cursor upstream returned HTTP {status}"));
    let (mapped_status, kind) = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            (StatusCode::UNAUTHORIZED, "authentication_error")
        }
        StatusCode::TOO_MANY_REQUESTS => (status, "rate_limit_error"),
        _ => (StatusCode::BAD_GATEWAY, "api_error"),
    };
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
    let (status, kind) = match error.status() {
        Some(401 | 403) => (StatusCode::UNAUTHORIZED, "authentication_error"),
        Some(429) => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
        _ => (StatusCode::BAD_GATEWAY, "api_error"),
    };
    own_error(status, kind, error.to_string())
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
