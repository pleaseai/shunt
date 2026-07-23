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

fn append_gemini_events(line: &[u8], machine: &mut GeminiSseMachine, output: &mut Vec<u8>) {
    let Ok(line) = std::str::from_utf8(line) else {
        return;
    };
    let line = line.trim();
    let Some(json_str) = line.strip_prefix("data: ").map(str::trim) else {
        return;
    };
    if json_str.is_empty() || json_str == "[DONE]" {
        return;
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(json_str) {
        append_sse_events(machine.process_chunk(&parsed), output);
    }
}

fn append_sse_events(events: Vec<crate::model::gemini::SseEvent>, output: &mut Vec<u8>) {
    for event in events {
        let formatted = format!("event: {}\ndata: {}\n\n", event.event, event.data);
        output.extend_from_slice(formatted.as_bytes());
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
            failure: None,
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
                failure: None,
            });
        }
    };

    let json_body: Value = serde_json::from_slice(&body).map_err(|error| AdapterError {
        message: format!("invalid JSON in Anthropic request body: {error}"),
        response: Box::new(StatusCode::BAD_REQUEST.into_response()),
        failure: None,
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
        failure: None,
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
            (byte_stream, Vec::<u8>::new(), machine, false),
            |(mut bytes, mut line_buffer, mut machine, finished)| async move {
                if finished {
                    return None;
                }
                loop {
                    let mut sse_bytes = Vec::new();
                    while let Some(pos) = line_buffer.iter().position(|byte| *byte == b'\n') {
                        let line = line_buffer.drain(..=pos).collect::<Vec<_>>();
                        append_gemini_events(&line[..line.len() - 1], &mut machine, &mut sse_bytes);
                    }

                    if !sse_bytes.is_empty() {
                        return Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(sse_bytes)),
                            (bytes, line_buffer, machine, false),
                        ));
                    }

                    match bytes.next().await {
                        Some(Ok(chunk)) => line_buffer.extend_from_slice(&chunk),
                        Some(Err(error)) => {
                            return Some((
                                Err(std::io::Error::other(format!(
                                    "Gemini response stream failed: {error}"
                                ))),
                                (bytes, line_buffer, machine, true),
                            ));
                        }
                        None => {
                            let mut terminal_bytes = Vec::new();
                            if !line_buffer.is_empty() {
                                append_gemini_events(
                                    &line_buffer,
                                    &mut machine,
                                    &mut terminal_bytes,
                                );
                            }
                            let mut events = Vec::new();
                            machine.finish(&mut events);
                            append_sse_events(events, &mut terminal_bytes);
                            if terminal_bytes.is_empty() {
                                return None;
                            }
                            return Some((
                                Ok::<_, std::io::Error>(axum::body::Bytes::from(terminal_bytes)),
                                (bytes, Vec::new(), machine, true),
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
                failure: None,
            })?;

        Ok((StatusCode::OK, response_res))
    } else {
        let full_text = response.text().await.map_err(|error| AdapterError {
            message: format!("failed to read response body: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            failure: None,
        })?;

        let parsed = serde_json::from_str::<Value>(&full_text).map_err(|error| AdapterError {
            message: format!("invalid JSON from Gemini backend: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            failure: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_utf8_line_survives_arbitrary_byte_chunking() {
        let line = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Olá 🌊\"}]}}]}";
        let split = line.find('🌊').unwrap() + 1;
        let mut buffered = line.as_bytes()[..split].to_vec();
        buffered.extend_from_slice(&line.as_bytes()[split..]);
        let mut machine = GeminiSseMachine::new("gemini-test");
        let mut output = Vec::new();

        append_gemini_events(&buffered, &mut machine, &mut output);

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Olá 🌊"));
        assert!(!output.contains('�'));
    }

    #[test]
    fn unterminated_final_data_line_is_processed() {
        let mut machine = GeminiSseMachine::new("gemini-test");
        let mut output = Vec::new();

        append_gemini_events(
            br#"data: {"candidates":[{"content":{"parts":[{"text":"final"}]},"finishReason":"STOP"}]}"#,
            &mut machine,
            &mut output,
        );

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("final"));
        assert!(output.contains("event: message_stop"));
    }
}
