use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{resolve_credential, Credential},
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
    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let upstream = request_builder(&state, &route, credential)
        .body(upstream_body.to_string())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(mapped_upstream_error(status, upstream, &route.provider).await);
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

async fn mapped_upstream_error(
    status: StatusCode,
    upstream: reqwest::Response,
    provider: &str,
) -> AdapterError {
    let text = upstream.text().await.unwrap_or_default();
    let value = if status == StatusCode::UNAUTHORIZED && matches!(provider, "codex" | "chatgpt") {
        json!({"message": "ChatGPT authentication failed; run codex login"})
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text}))
    };
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
        response: Box::new(
            (shunt_status, axum::Json(map_error_value(&value, status))).into_response(),
        ),
    }
}

fn own_error(message: String) -> AdapterError {
    let error = ShuntError::bad_gateway(message);
    AdapterError {
        message: "responses adapter failed".to_string(),
        response: Box::new(error.into_response()),
    }
}

fn request_builder(
    state: &AppState,
    route: &Route,
    credential: Credential,
) -> reqwest::RequestBuilder {
    let mut request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .header("OpenAI-Beta", "responses=experimental")
        .header("content-type", "application/json");
    match credential {
        Credential::ApiKey(api_key) => {
            request = request.bearer_auth(api_key);
        }
        Credential::ChatGptOAuth {
            access_token,
            account_id,
        } => {
            request = request
                .bearer_auth(access_token)
                .header("chatgpt-account-id", account_id)
                .header("originator", "codex_cli_rs");
        }
    }
    request
}

pub fn responses_url(config: &crate::config::Config, provider: &str) -> String {
    let base = match provider {
        "codex" | "chatgpt" => &config.providers.codex.base_url,
        _ => &config.providers.openai.base_url,
    }
    .trim_end_matches('/');
    if matches!(provider, "codex" | "chatgpt") {
        format!("{base}/codex/responses")
    } else {
        format!("{base}/responses")
    }
}

#[cfg(test)]
pub fn build_test_request(
    state: &AppState,
    route: &Route,
    credential: Credential,
) -> reqwest::Request {
    request_builder(state, route, credential)
        .body("{}")
        .build()
        .expect("test request should build")
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

#[cfg(test)]
mod tests {
    use crate::{
        auth::Credential,
        config::Config,
        routing::{AdapterKind, Route},
        server::AppState,
    };

    use super::{build_test_request, responses_url};

    fn codex_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
        }
    }

    #[test]
    fn builds_codex_url_and_headers_without_sending() {
        let state = AppState {
            config: Config::default(),
            http_client: reqwest::Client::new(),
        };

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
        );

        assert_eq!(
            request.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            request.headers().get("chatgpt-account-id").unwrap(),
            "account-id"
        );
        assert_eq!(request.headers().get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(
            request.headers().get("OpenAI-Beta").unwrap(),
            "responses=experimental"
        );
    }

    #[test]
    fn builds_openai_responses_url() {
        assert_eq!(
            responses_url(&Config::default(), "openai"),
            "https://api.openai.com/v1/responses"
        );
    }
}
