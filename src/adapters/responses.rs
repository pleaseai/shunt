use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    error::ShuntError,
    model::responses::{
        map_error_value, parse_sse_events, translate_request, AnthropicSseMachine, ResponseEvent,
    },
    routing::Route,
    server::AppState,
};

pub struct ResponsesAdapter;

impl Adapter for ResponsesAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        _uri: &'a Uri,
        _headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let client_wants_stream = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false);
    let upstream_body =
        translate_request(&body, &route).map_err(|error| own_error(error.to_string()))?;
    let api_key = std::env::var(&state.config.providers.openai.api_key_env).map_err(|_| {
        own_error(format!(
            "{} is not set",
            state.config.providers.openai.api_key_env
        ))
    })?;
    let upstream = state
        .http_client
        .post(responses_url(&state))
        .bearer_auth(api_key)
        .header("OpenAI-Beta", "responses=experimental")
        .header("content-type", "application/json")
        .body(upstream_body.to_string())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(mapped_upstream_error(status, upstream).await);
    }
    if client_wants_stream {
        Ok((StatusCode::OK, stream_response(upstream, route.model)))
    } else {
        Ok((StatusCode::OK, json_response(upstream, route.model).await?))
    }
}

fn stream_response(upstream: reqwest::Response, model: String) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let parser = SseParser::default();
    let machine = AnthropicSseMachine::new(model);
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
        .body(Body::from_stream(output))
        .expect("response builder uses valid status and headers")
        .into_response()
}

async fn json_response(
    upstream: reqwest::Response,
    model: String,
) -> Result<axum::response::Response, AdapterError> {
    let body = upstream
        .text()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let mut machine = AnthropicSseMachine::new(model);
    for event in parse_sse_events(&body) {
        let _ = machine.apply(event);
    }
    Ok((StatusCode::OK, axum::Json(machine.final_json())).into_response())
}

async fn mapped_upstream_error(status: StatusCode, upstream: reqwest::Response) -> AdapterError {
    let text = upstream.text().await.unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text}));
    let shunt_status = if status == StatusCode::UNAUTHORIZED
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::BAD_REQUEST
    {
        status
    } else {
        StatusCode::BAD_GATEWAY
    };
    AdapterError {
        message: format!("upstream responses request failed with {status}"),
        response: (shunt_status, axum::Json(map_error_value(&value, status))).into_response(),
    }
}

fn own_error(message: String) -> AdapterError {
    let error = ShuntError::bad_gateway(message);
    AdapterError {
        message: "responses adapter failed".to_string(),
        response: error.into_response(),
    }
}

fn responses_url(state: &AppState) -> String {
    let base = state.config.providers.openai.base_url.trim_end_matches('/');
    format!("{base}/responses")
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
