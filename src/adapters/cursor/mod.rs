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
    sse::{format_sse_error_typed, CursorSseFramer},
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
    let images = decode_cursor_images(&request);
    let tools = extract_cursor_tools(&request);
    let want_stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The env context frame carries the working directory; the gateway has no
    // per-request workspace, so use the process cwd (falling back to "/").
    static CWD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let cwd = CWD.get_or_init(|| {
        std::env::current_dir()
            .ok()
            .and_then(|path| path.to_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "/".to_string())
    });

    let client = CursorAgentClient::new(state.http_client.clone());
    let params = agent::AgentRunParams {
        prompt: &prompt,
        model_id: &resolved.model_id,
        cwd,
        mode: resolved.mode.wire_enum(),
        images: &images,
        tools: &tools,
    };
    // `open_turn` returns once the response headers arrive, keeping the paced
    // request stream open behind the returned turn. It is not wrapped in the
    // shared `send_with_retry` (which is typed to `reqwest::Response`); a
    // connection blip surfaces to the client. TODO(#170): bounded pre-response
    // retry for the streaming turn.
    let turn = client
        .open_turn(&access_token, &params)
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

/// Base64-decode the request's inline images into agent image inputs. URL
/// images (skipped upstream) and any that fail to decode are dropped; the
/// rendered prompt still carries a text placeholder for them.
fn decode_cursor_images(request: &Value) -> Vec<agent::AgentImage> {
    use base64::Engine;
    request::cursor_selected_images(request)
        .into_iter()
        .filter_map(|image| {
            let data = base64::engine::general_purpose::STANDARD
                .decode(image.data.as_bytes())
                .ok()?;
            Some(agent::AgentImage {
                data,
                uuid: image.uuid,
                path: image.path,
                mime_type: image.mime_type,
            })
        })
        .collect()
}

/// Extract advertised client tools into native MCP tool declarations. Tools
/// without a name are skipped; a missing schema defaults to an empty object.
fn extract_cursor_tools(request: &Value) -> Vec<agent::AgentTool> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?.to_string();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input_schema = tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object"}));
            Some(agent::AgentTool {
                name,
                description,
                input_schema,
            })
        })
        .collect()
}

/// Collect a full turn into a non-streaming Anthropic message JSON.
async fn aggregate_turn(
    turn: CursorAgentTurn,
    message_id: &str,
    model: &str,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let mut events = std::pin::pin!(turn.into_event_stream());
    let mut text = String::new();
    let mut tool_call: Option<(String, String)> = None;
    while let Some(event) = events.next().await {
        match event.map_err(map_cursor_stream_error)? {
            CursorStreamEvent::TextDelta { text: delta } => text.push_str(&delta),
            CursorStreamEvent::ToolCall { name, input_json } => {
                tool_call = Some((name, input_json));
                break;
            }
            CursorStreamEvent::End => break,
            // Reasoning and session markers do not surface in the text-only
            // non-streaming body.
            _ => {}
        }
    }
    let mut content: Vec<Value> = Vec::new();
    if !text.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": text}));
    }
    let stop_reason = if let Some((name, input_json)) = tool_call {
        let input: Value =
            serde_json::from_str(&input_json).unwrap_or_else(|_| serde_json::json!({}));
        content.push(serde_json::json!({
            "type": "tool_use",
            "id": format!("toolu_{}", uuid::Uuid::new_v4().simple()),
            "name": name,
            "input": input,
        }));
        "tool_use"
    } else {
        "end_turn"
    };
    // Anthropic messages must carry at least one content block.
    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }
    let json = serde_json::json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
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
                    Some(Ok(CursorStreamEvent::ToolCall { name, input_json })) => {
                        // Emit the tool_use pause (content block + message_delta
                        // stop_reason="tool_use" + message_stop) and end the SSE.
                        // The client executes the tool and re-sends the result in
                        // history, which the stateless bridge re-runs upstream.
                        let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                        framer.emit_tool_pause(&id, &name, &input_json);
                        return Some((
                            Ok(Bytes::from(framer.take_output())),
                            (events, framer, true),
                        ));
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
                        let status =
                            StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_GATEWAY);
                        let kind = crate::model::responses::anthropic_error_type(status);
                        let detail = connect_error_detail(&error);
                        let message = crate::model::responses::context_overflow_message(
                            &detail,
                            &error.message,
                        )
                        .unwrap_or(error.message);
                        let mut output = framer.take_output();
                        output.extend_from_slice(&format_sse_error_typed(kind, &message));
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
    let mapped_status = crate::model::responses::client_facing_status(status);
    let kind = crate::model::responses::anthropic_error_type(status);
    let stream = futures_stream::once(async move {
        let text = upstream.text().await.unwrap_or_default();
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
        let message = crate::model::responses::context_overflow_message(
            body.as_ref().unwrap_or(&Value::Null),
            &message,
        )
        .unwrap_or(message);
        let envelope = serde_json::json!({
            "type": "error",
            "error": {"type": kind, "message": message}
        });
        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
            serde_json::to_vec(&envelope).unwrap_or_default(),
        ))
    });
    let mut error = Response::builder()
        .status(mapped_status)
        .header("content-type", "application/json")
        .body(Body::from_stream(stream))
        .expect("valid mapped Cursor error response");
    if let Some(value) = retry_after {
        error.headers_mut().insert("retry-after", value);
    }
    AdapterError {
        message: format!("Cursor upstream request failed with {status}"),
        response: Box::new(error),
        failure: Some(crate::adapters::AdapterFailure::UpstreamStatus(status)),
    }
}

fn map_client_error(error: client::CursorError) -> AdapterError {
    let mut error = bad_gateway(error.to_string());
    error.failure = Some(crate::adapters::AdapterFailure::BeforeHeaders);
    error
}

/// Parse a Cursor Connect error's `detail` (the raw end-frame JSON body) so the
/// context-overflow rewrite can read the upstream error `code`. Falls back to
/// `Value::Null` when there is no detail or it isn't JSON.
fn connect_error_detail(error: &client::CursorError) -> Value {
    error
        .detail
        .as_deref()
        .and_then(|detail| serde_json::from_str::<Value>(detail).ok())
        .unwrap_or(Value::Null)
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
    let detail = connect_error_detail(&error);
    let message = crate::model::responses::context_overflow_message(&detail, &error.message)
        .unwrap_or(error.message);
    let error = own_error(mapped_status, kind, message);
    debug_assert!(error.failure.is_none());
    error
}

fn bad_gateway(message: String) -> AdapterError {
    own_error(StatusCode::BAD_GATEWAY, "api_error", message)
}

fn own_error(status: StatusCode, kind: &'static str, message: impl Into<String>) -> AdapterError {
    AdapterError {
        message: "Cursor adapter failed".to_string(),
        response: Box::new(ShuntError::new(status, kind, message).into_response()),
        failure: None,
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

    fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push(((value as u8) & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn field_ld(field: u64, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 4);
        encode_varint((field << 3) | 2, &mut out);
        encode_varint(data.len() as u64, &mut out);
        out.extend_from_slice(data);
        out
    }

    fn field_str(field: u64, value: &str) -> Vec<u8> {
        field_ld(field, value.as_bytes())
    }

    fn connect_frame(payload: &[u8]) -> Vec<u8> {
        connect::encode_connect_frame(payload, 0).to_vec()
    }

    fn text_turn_frames(text: &str) -> Vec<u8> {
        let mut frames = connect_frame(&field_ld(1, &field_ld(1, &field_str(1, text))));
        frames.extend_from_slice(&connect::encode_connect_frame(b"{}", connect::FLAG_END));
        frames
    }

    fn tool_call_turn_frames(name: &str, key: &str, value: &str) -> Vec<u8> {
        // AgentServerMessage(2) → ExecServerMessage.mcp_args(11) → McpArgs,
        // where args(2) is a map entry containing a protobuf string Value.
        let mut entry = field_str(1, key);
        entry.extend(field_ld(2, &field_ld(3, value.as_bytes())));
        let mut mcp_args = field_str(5, name);
        mcp_args.extend(field_ld(2, &entry));
        let mut frames = connect_frame(&field_ld(2, &field_ld(11, &mcp_args)));
        frames.extend_from_slice(&connect::encode_connect_frame(b"{}", connect::FLAG_END));
        frames
    }

    fn reasoning_turn_frames(text: &str) -> Vec<u8> {
        let mut frames = connect_frame(&field_ld(1, &field_ld(4, &field_str(1, text))));
        frames.extend_from_slice(&connect::encode_connect_frame(b"{}", connect::FLAG_END));
        frames
    }

    fn error_turn_frames(code: &str, message: &str) -> Vec<u8> {
        connect::encode_connect_frame(
            serde_json::json!({"error": {"code": code, "message": message}})
                .to_string()
                .as_bytes(),
            connect::FLAG_END,
        )
        .to_vec()
    }

    async fn turn_from_frames(frames: Vec<u8>) -> CursorAgentTurn {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/turn"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(frames, "application/connect+proto"),
            )
            .mount(&server)
            .await;
        let response = reqwest::Client::new()
            .get(format!("{}/turn", server.uri()))
            .send()
            .await
            .expect("mock turn should be available");
        CursorAgentTurn::from_response_for_test(response)
    }

    async fn response_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&bytes).expect("response body should be JSON")
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
    fn cursor_stream_error_keeps_client_mapping_but_stops_failover() {
        let error = map_cursor_stream_error(client::CursorError::new(
            503,
            "backend failed after accepting turn",
            None,
        ));

        assert!(error.failure.is_none());
        assert_eq!(error.response.status(), StatusCode::SERVICE_UNAVAILABLE);
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

    #[test]
    fn extract_cursor_tools_maps_definitions_and_defaults() {
        let request = serde_json::json!({
            "tools": [
                {
                    "name": "Read",
                    "description": "Read a file",
                    "input_schema": {
                        "type": "object",
                        "properties": {"file_path": {"type": "string"}}
                    }
                },
                {"name": "NoSchema", "description": "Uses the default"},
                {"description": "missing name"}
            ]
        });

        let tools = extract_cursor_tools(&request);

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "Read");
        assert_eq!(tools[0].description, "Read a file");
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(tools[1].name, "NoSchema");
        assert_eq!(tools[1].input_schema, serde_json::json!({"type": "object"}));
        assert!(extract_cursor_tools(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn decode_cursor_images_decodes_base64_and_skips_unsupported_images() {
        let request = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "aGVsbG8="
                        }
                    },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": "%%%"
                        }
                    },
                    {
                        "type": "image",
                        "source": {
                            "type": "url",
                            "url": "https://example.com/image.png"
                        }
                    }
                ]
            }]
        });

        let images = decode_cursor_images(&request);

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, b"hello");
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].path, "claude-image-1.png");
        assert!(!images[0].uuid.is_empty());
    }

    #[tokio::test]
    async fn aggregate_turn_builds_text_response() {
        let turn = turn_from_frames(text_turn_frames("hello")).await;

        let (status, response) = aggregate_turn(turn, "msg_test", "cursor:test")
            .await
            .expect("turn should aggregate");
        let body = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn aggregate_turn_builds_tool_use_response() {
        let turn = turn_from_frames(tool_call_turn_frames("Read", "file_path", "/tmp/x")).await;

        let (status, response) = aggregate_turn(turn, "msg_test", "cursor:test")
            .await
            .expect("turn should aggregate");
        let body = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["stop_reason"], "tool_use");
        assert_eq!(body["content"][0]["type"], "tool_use");
        assert_eq!(body["content"][0]["name"], "Read");
        assert_eq!(body["content"][0]["input"]["file_path"], "/tmp/x");
    }

    #[tokio::test]
    async fn streaming_response_emits_tool_use_pause() {
        let turn = turn_from_frames(tool_call_turn_frames("Read", "file_path", "/tmp/x")).await;
        let response = streaming_response(
            turn,
            "msg_test".to_string(),
            "cursor:test".to_string(),
            std::time::Duration::from_secs(60),
        );

        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("SSE response should be readable");
        let body = String::from_utf8(bytes.to_vec()).expect("SSE body should be UTF-8");

        assert!(body.contains("\"type\":\"tool_use\""));
        assert!(body.contains("\"stop_reason\":\"tool_use\""));
        assert!(body.contains("\"partial_json\""));
        assert!(body.contains("file_path"));
        assert!(body.contains("/tmp/x"));
    }

    #[tokio::test]
    async fn streaming_response_emits_text_and_end_turn() {
        let turn = turn_from_frames(text_turn_frames("hello")).await;
        let response = streaming_response(
            turn,
            "msg_test".to_string(),
            "cursor:test".to_string(),
            std::time::Duration::from_secs(60),
        );

        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("SSE response should be readable");
        let body = String::from_utf8(bytes.to_vec()).expect("SSE body should be UTF-8");

        assert!(body.contains("content_block_delta"));
        assert!(body.contains("\"text\":\"hello\""));
        assert!(body.contains("\"stop_reason\":\"end_turn\""));
    }

    #[tokio::test]
    async fn aggregate_turn_ignores_reasoning_and_fills_empty_content() {
        let turn = turn_from_frames(reasoning_turn_frames("thinking")).await;

        let (_, response) = aggregate_turn(turn, "msg_test", "cursor:test")
            .await
            .expect("turn should aggregate");
        let body = response_json(response).await;

        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(
            body["content"][0],
            serde_json::json!({"type": "text", "text": ""})
        );
    }

    #[tokio::test]
    async fn aggregate_turn_maps_connect_error() {
        let turn = turn_from_frames(error_turn_frames("unauthenticated", "bad token")).await;

        let error = aggregate_turn(turn, "msg_test", "cursor:test")
            .await
            .expect_err("Connect error should fail aggregation");
        let status = error.response.status();
        let body = body_json(error).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"], "authentication_error");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bad token"));
    }

    #[tokio::test]
    async fn streaming_response_emits_thinking_delta() {
        let turn = turn_from_frames(reasoning_turn_frames("thinking")).await;
        let response = streaming_response(
            turn,
            "msg_test".to_string(),
            "cursor:test".to_string(),
            std::time::Duration::from_secs(60),
        );

        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("SSE response should be readable");
        let body = String::from_utf8(bytes.to_vec()).expect("SSE body should be UTF-8");

        assert!(body.contains("thinking_delta"));
        assert!(body.contains("\"thinking\":\"thinking\""));
        assert!(body.contains("\"stop_reason\":\"end_turn\""));
    }

    #[tokio::test]
    async fn streaming_response_formats_connect_error() {
        let turn = turn_from_frames(error_turn_frames("unauthenticated", "bad token")).await;
        let response = streaming_response(
            turn,
            "msg_test".to_string(),
            "cursor:test".to_string(),
            std::time::Duration::from_secs(60),
        );

        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("SSE response should be readable");
        let body = String::from_utf8(bytes.to_vec()).expect("SSE body should be UTF-8");

        assert!(body.contains("event: error"));
        assert!(body.contains("bad token"));
    }

    #[test]
    fn client_and_stream_error_mappers_preserve_semantics() {
        let client_error = client::CursorError::internal("transport failed");
        assert_eq!(
            map_client_error(client_error).response.status(),
            StatusCode::BAD_GATEWAY
        );

        let context_error = client::CursorError::new(
            400,
            "maximum context length is 100 tokens but 150 tokens were supplied",
            None,
        );
        let mapped = map_cursor_stream_error(context_error);
        assert_eq!(mapped.response.status(), StatusCode::BAD_REQUEST);

        let invalid_status = client::CursorError::new(99, "invalid status", None);
        assert_eq!(
            map_cursor_stream_error(invalid_status).response.status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
