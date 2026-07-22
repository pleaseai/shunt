//! Gemini adapter implementation for Google Code Assist / Gemini endpoints.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{resolve_credential, Credential},
    config::AuthMode,
    model::gemini::{map_gemini_error, GeminiSseMachine},
    model::gemini_request::{translate_request, wrap_code_assist_envelope},
    routing::Route,
    server::AppState,
};

pub struct GeminiAdapter;

impl Adapter for GeminiAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    _uri: &Uri,
    _headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<(StatusCode, Response<Body>), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| AdapterError {
            message: format!("unknown provider {}", route.provider),
            response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        })?;

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;

    let (access_token, project_id) = match credential {
        Credential::GoogleOauth {
            access_token,
            project_id,
        } => (access_token, project_id),
        Credential::ApiKey { value, .. } => (value, String::new()),
        _ => {
            return Err(AdapterError {
                message: "unsupported credential for Gemini adapter".to_string(),
                response: Box::new(StatusCode::UNAUTHORIZED.into_response()),
            });
        }
    };

    let json_body: Value = serde_json::from_slice(&body).map_err(|error| AdapterError {
        message: format!("invalid JSON in Anthropic request body: {error}"),
        response: Box::new(StatusCode::BAD_REQUEST.into_response()),
    })?;

    let is_streaming = json_body.get("stream").and_then(Value::as_bool) == Some(true);

    let inner_req = translate_request(&json_body)?;

    let base_url = provider.base_url.trim_end_matches('/');

    let method = if is_streaming {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    let (endpoint, payload) = if provider.auth == AuthMode::GoogleOauth {
        let endpoint = format!("{base_url}/v1internal:{method}");
        let envelope = wrap_code_assist_envelope(&route.upstream_model, &project_id, inner_req);
        (endpoint, envelope)
    } else {
        let model_slug = &route.upstream_model;
        let endpoint = format!("{base_url}/v1beta/models/{model_slug}:{method}");
        (endpoint, inner_req)
    };

    let policy = provider.retry.policy();
    let http_client = state.http_client.clone();
    let payload_clone = payload.clone();
    let endpoint_clone = endpoint.clone();
    let is_google_oauth = provider.auth == AuthMode::GoogleOauth;
    let token = access_token.clone();

    let response = crate::retry::send_with_retry(policy, &route.provider, || {
        let client = http_client.clone();
        let payload = payload_clone.clone();
        let endpoint = endpoint_clone.clone();
        let token = token.clone();
        async move {
            let mut req = client
                .post(&endpoint)
                .header("Content-Type", "application/json");

            if is_google_oauth {
                req = req.bearer_auth(&token);
            } else {
                req = req.header("x-goog-api-key", &token);
            }

            req.json(&payload).send().await
        }
    })
    .await
    .map_err(|error| AdapterError {
        message: format!("network error calling Gemini backend: {error}"),
        response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
    })?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|_| "failed to read error response".to_string());
        return Err(map_gemini_error(status, &body_text));
    }

    if is_streaming {
        let byte_stream = response.bytes_stream();
        let machine = GeminiSseMachine::new(&route.model);

        let sse_stream = futures_util::stream::unfold(
            (byte_stream, String::new(), machine, false),
            |(mut bytes, mut line_buffer, mut machine, finished)| async move {
                if finished {
                    return None;
                }
                loop {
                    let mut sse_bytes = Vec::new();
                    while let Some(pos) = line_buffer.find('\n') {
                        let line = line_buffer[..pos].trim().to_string();
                        line_buffer.drain(..=pos);

                        if line.starts_with("data: ") {
                            let json_str = line.trim_start_matches("data: ").trim();
                            if json_str.is_empty() || json_str == "[DONE]" {
                                continue;
                            }
                            if let Ok(parsed) = serde_json::from_str::<Value>(json_str) {
                                let events = machine.process_chunk(&parsed);
                                for ev in events {
                                    let formatted =
                                        format!("event: {}\ndata: {}\n\n", ev.event, ev.data);
                                    sse_bytes.extend_from_slice(formatted.as_bytes());
                                }
                            }
                        }
                    }

                    if !sse_bytes.is_empty() {
                        return Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(sse_bytes)),
                            (bytes, line_buffer, machine, false),
                        ));
                    }

                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            let text = String::from_utf8_lossy(&chunk);
                            line_buffer.push_str(&text);
                        }
                        Some(Err(_)) | None => {
                            let mut events = Vec::new();
                            machine.finish(&mut events);
                            if events.is_empty() {
                                return None;
                            }
                            let mut terminal_bytes = Vec::new();
                            for ev in events {
                                let formatted =
                                    format!("event: {}\ndata: {}\n\n", ev.event, ev.data);
                                terminal_bytes.extend_from_slice(formatted.as_bytes());
                            }
                            return Some((
                                Ok::<_, std::io::Error>(axum::body::Bytes::from(terminal_bytes)),
                                (bytes, line_buffer, machine, true),
                            ));
                        }
                    }
                }
            },
        );

        let res_builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .header("Cache-Control", "no-cache");

        let response_res = res_builder
            .body(Body::from_stream(sse_stream))
            .map_err(|error| AdapterError {
                message: format!("failed to build response: {error}"),
                response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            })?;

        Ok((StatusCode::OK, response_res))
    } else {
        let full_text = response.text().await.map_err(|error| AdapterError {
            message: format!("failed to read response body: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
        })?;

        let parsed = serde_json::from_str::<Value>(&full_text).map_err(|error| AdapterError {
            message: format!("invalid JSON from Gemini backend: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
        })?;
        let mut machine = GeminiSseMachine::new(&route.model);
        let _ = machine.process_chunk(&parsed);
        let final_json = machine.final_json();

        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let response_res = (StatusCode::OK, headers, axum::Json(final_json)).into_response();
        Ok((StatusCode::OK, response_res))
    }
}
