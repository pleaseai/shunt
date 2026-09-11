//! OpenAI Chat Completions adapter for Anthropic Messages ingress.

mod sse;

use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFailure, AdapterFuture},
    auth::{resolve_credential, Credential},
    config::ApiKeyHeader,
    model::openai_chat_request::translate_request,
    model::openai_chat_response::OpenAiChatSseMachine,
    request::RequestBody,
    routing::Route,
    server::AppState,
};

use self::sse::{Decoder as OpenAiChatSseDecoder, Item as OpenAiChatSseItem};

const MAX_OPENAI_CHAT_UNARY_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Generation POSTs are non-idempotent and reachable in one send: never
/// re-dispatch after the request could have reached the upstream, and never
/// follow a redirect that could carry the bearer off the configured origin.
pub(crate) const OPENAI_CHAT_RETRY_SAFETY: crate::retry::RetrySafety =
    crate::retry::RetrySafety::NonIdempotentPost;

pub struct OpenAiChatAdapter;

impl Adapter for OpenAiChatAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: RequestBody,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body).await })
    }
}

/// One shared client for every Chat upstream. Redirects are refused outright:
/// a 3xx could otherwise move the injected bearer to a different origin than
/// the operator configured.
fn chat_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("OpenAI Chat HTTP client must build")
    })
}

fn local_openai_chat_error(message: impl Into<String>) -> AdapterError {
    let message = message.into();
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": message}
    });
    AdapterError {
        message,
        response: Box::new((StatusCode::BAD_GATEWAY, axum::Json(body)).into_response()),
        failure: None,
    }
}

/// Map a non-success upstream status into the gateway-owned Anthropic error
/// shape with failover metadata attached.
fn map_openai_chat_error(status: StatusCode, body: &str) -> AdapterError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
        })
        .and_then(Value::as_str)
        .unwrap_or("OpenAI Chat backend returned a non-JSON error")
        .to_string();
    let message = if message.len() > 4096 {
        format!("{}…", &message[..message.floor_char_boundary(4096)])
    } else {
        message
    };
    let error_type = match status {
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "authentication_error",
        status if status.is_client_error() => "invalid_request_error",
        _ => "api_error",
    };
    let error_body = serde_json::json!({
        "type": "error",
        "error": {"type": error_type, "message": message}
    });
    AdapterError {
        message,
        response: Box::new((status, axum::Json(error_body)).into_response()),
        failure: Some(AdapterFailure::UpstreamStatus(status)),
    }
}

fn append_sse_events(events: Vec<crate::model::gemini::SseEvent>, output: &mut Vec<u8>) {
    for event in events {
        let formatted = format!("event: {}\ndata: {}\n\n", event.event, event.data);
        output.extend_from_slice(formatted.as_bytes());
    }
}

fn append_protocol_error(message: impl Into<String>, output: &mut Vec<u8>) {
    append_sse_events(
        vec![crate::model::gemini::SseEvent {
            event: "error".to_string(),
            data: serde_json::json!({
                "type": "error",
                "error": {"type": "api_error", "message": message.into()}
            }),
        }],
        output,
    );
}

async fn collect_unary_response(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, AdapterError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(local_openai_chat_error(format!(
            "OpenAI Chat response exceeded {max_bytes} bytes"
        )));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            local_openai_chat_error(format!("failed to read OpenAI Chat response body: {error}"))
        })?;
        let new_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| local_openai_chat_error("OpenAI Chat response size overflow"))?;
        if new_len > max_bytes {
            return Err(local_openai_chat_error(format!(
                "OpenAI Chat response exceeded {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn forward(
    state: AppState,
    route: Route,
    _uri: &Uri,
    _headers: &HeaderMap,
    body: RequestBody,
) -> Result<(StatusCode, Response<Body>), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| AdapterError {
            message: format!("unknown provider {}", route.provider),
            response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            failure: None,
        })?;

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    // Resolved fresh from api_key_env on every request, so concurrent turns
    // cannot exchange credentials; the value lives only inside this future.
    let token = match credential {
        Credential::ApiKey { value, .. } => value,
        _ => {
            return Err(AdapterError {
                message: "OpenAI Chat provider requires an api_key credential".to_string(),
                response: Box::new(StatusCode::UNAUTHORIZED.into_response()),
                failure: None,
            })
        }
    };

    let json_body = body.json();
    let is_streaming = json_body.get("stream").and_then(Value::as_bool) == Some(true);
    let payload = translate_request(json_body, &route.upstream_model, is_streaming)?;

    // One-time path append on the trimmed base URL. The full base-URL
    // ambiguity grammar (trailing path, versioned prefixes) is 13-02 Task 3;
    // this shape must be replaceable without architectural change.
    let base_url = provider.base_url.trim_end_matches('/');
    let endpoint = format!("{base_url}/chat/completions");

    let policy = provider.retry.policy();
    let ttfb_ms = state.config.server.timeouts.upstream_ttfb_ms;
    let client = chat_client().clone();
    let send_once = |token: String| {
        let client = client.clone();
        let payload = payload.clone();
        let endpoint = endpoint.clone();
        async move {
            let request = match provider.api_key_header {
                ApiKeyHeader::Bearer => client.post(&endpoint).bearer_auth(&token),
                ApiKeyHeader::XApiKey => client.post(&endpoint).header("x-api-key", &token),
            };
            crate::upstream_timeout::wait(ttfb_ms, request.json(&payload).send()).await
        }
    };
    let response = crate::retry::send_with_retry_with_safety(
        policy,
        &route.provider,
        OPENAI_CHAT_RETRY_SAFETY,
        move || send_once(token.clone()),
    )
    .await
    .map_err(|error| {
        // BeforeHeaders is reserved for genuine pre-send connect failures:
        // reqwest reports those before the request was written. A TTFB
        // timeout or a post-send transport cut is ambiguous and must not
        // authorize a re-dispatch elsewhere.
        match error {
            crate::upstream_timeout::SendError::Transport(ref transport_error)
                if transport_error.is_connect() =>
            {
                let message =
                    format!("network error calling OpenAI Chat backend: {transport_error}");
                AdapterError {
                    message,
                    response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                    failure: Some(AdapterFailure::BeforeHeaders),
                }
            }
            error => error.into_adapter_error(|transport_error| AdapterError {
                message: format!("network error calling OpenAI Chat backend: {transport_error}"),
                response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                failure: None,
            }),
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        let body = collect_unary_response(response, MAX_OPENAI_CHAT_UNARY_RESPONSE_BYTES).await?;
        let body_text = std::str::from_utf8(&body)
            .map_err(|_| local_openai_chat_error("invalid UTF-8 in OpenAI Chat error response"))?;
        return Err(map_openai_chat_error(status, body_text));
    }

    if is_streaming {
        stream_sse_response(state, route, response).await
    } else {
        let full_body =
            collect_unary_response(response, MAX_OPENAI_CHAT_UNARY_RESPONSE_BYTES).await?;
        let parsed = serde_json::from_slice::<Value>(&full_body).map_err(|error| {
            local_openai_chat_error(format!("invalid JSON from OpenAI Chat backend: {error}"))
        })?;
        let mut machine = OpenAiChatSseMachine::new_for_upstream(&route.model);
        let events = machine
            .process_chunk_checked(&parsed)
            .map_err(|error| local_openai_chat_error(error.to_string()))?;
        if let Some(error) = events.into_iter().find(|event| event.event == "error") {
            let message = error
                .data
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Chat backend error")
                .to_string();
            return Err(local_openai_chat_error(message));
        }
        machine
            .transport_close_checked()
            .map_err(|error| local_openai_chat_error(error.to_string()))?;
        let final_json = machine
            .final_json_checked()
            .map_err(|error| local_openai_chat_error(error.to_string()))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            axum::http::HeaderValue::from_static("application/json"),
        );
        let response_res = (StatusCode::OK, headers, axum::Json(final_json)).into_response();
        Ok((StatusCode::OK, response_res))
    }
}

/// Incremental SSE relay. The unfold tuple owns the byte stream, decoder, and
/// machine, so dropping the client connection drops everything together: no
/// registry, no detached task, no buffered full body.
async fn stream_sse_response(
    state: AppState,
    route: Route,
    response: reqwest::Response,
) -> Result<(StatusCode, Response<Body>), AdapterError> {
    let byte_stream = response.bytes_stream();
    let decoder = OpenAiChatSseDecoder::default();
    let machine = OpenAiChatSseMachine::new_streaming_for_upstream(&route.model);

    let sse_stream = futures_util::stream::unfold(
        (
            byte_stream,
            decoder,
            machine,
            false,
            None::<Bytes>,
            None::<Bytes>,
        ),
        |(mut bytes, mut decoder, mut machine, finished, mut pending, deferred_terminal)| {
            async move {
                if finished {
                    return None;
                }
                loop {
                    if let Some(chunk) = pending.take() {
                        let (consumed, item) = match decoder.push_one(&chunk) {
                            Ok(step) => step,
                            Err(error) => {
                                let mut output = Vec::new();
                                append_protocol_error(error, &mut output);
                                return Some((
                                    Ok::<_, std::convert::Infallible>(Bytes::from(output)),
                                    (bytes, decoder, machine, true, None, None),
                                ));
                            }
                        };
                        if consumed < chunk.len() {
                            pending = Some(chunk.slice(consumed..));
                        }
                        let Some(item) = item else {
                            continue;
                        };

                        let is_done = item == OpenAiChatSseItem::Done;
                        let result = match item {
                            OpenAiChatSseItem::Json(value) => machine.process_chunk_checked(&value),
                            OpenAiChatSseItem::Done => machine.transport_close_checked(),
                        };
                        let result_succeeded = result.is_ok();
                        let mut output = Vec::new();
                        let terminal = match result {
                            Ok(events) => {
                                let terminal = events.iter().any(|event| {
                                    event.event == "error" || event.event == "message_stop"
                                });
                                append_sse_events(events, &mut output);
                                terminal
                            }
                            Err(error) => {
                                append_protocol_error(error.to_string(), &mut output);
                                true
                            }
                        };
                        if is_done && !terminal {
                            unreachable!("[DONE] completion must be terminal");
                        }
                        // [DONE] is the provider's authoritative stream
                        // boundary. Return the terminal immediately so a
                        // backend that leaves the HTTP body open cannot hang
                        // the client indefinitely.
                        if is_done && terminal && result_succeeded {
                            return Some((
                                Ok(Bytes::from(output)),
                                (bytes, decoder, machine, true, None, None),
                            ));
                        }
                        if !output.is_empty() {
                            return Some((
                                Ok(Bytes::from(output)),
                                (
                                    bytes,
                                    decoder,
                                    machine,
                                    terminal,
                                    pending,
                                    deferred_terminal,
                                ),
                            ));
                        }
                        continue;
                    }

                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            pending = Some(chunk);
                        }
                        Some(Err(error)) => {
                            let mut output = Vec::new();
                            append_protocol_error(
                                format!("OpenAI Chat response stream failed: {error}"),
                                &mut output,
                            );
                            return Some((
                                Ok(Bytes::from(output)),
                                (bytes, decoder, machine, true, None, None),
                            ));
                        }
                        None => {
                            let mut output = Vec::new();
                            match decoder.finish() {
                                Err(error) => append_protocol_error(error, &mut output),
                                Ok(()) => {
                                    if let Some(terminal) = deferred_terminal {
                                        output.extend_from_slice(&terminal);
                                    } else {
                                        match machine.transport_close_checked() {
                                            Ok(events) => append_sse_events(events, &mut output),
                                            Err(error) => append_protocol_error(
                                                error.to_string(),
                                                &mut output,
                                            ),
                                        }
                                    }
                                }
                            }
                            return Some((
                                Ok(Bytes::from(output)),
                                (bytes, decoder, machine, true, None, None),
                            ));
                        }
                    }
                }
            }
        },
    );

    let keepalive_interval = Duration::from_secs(state.config.server.sse_keepalive_seconds);
    let response_res = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .body(Body::from_stream(crate::keepalive::with_pings(
            sse_stream,
            keepalive_interval,
        )))
        .map_err(|error| AdapterError {
            message: format!("failed to build response: {error}"),
            response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            failure: None,
        })?;

    Ok((StatusCode::OK, response_res))
}
