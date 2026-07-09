use std::time::Instant;

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
};
use tracing::Instrument;

use crate::{
    adapters::{anthropic::AnthropicAdapter, responses::ResponsesAdapter, Adapter, AdapterError},
    error::UpstreamError,
    routing::{self, AdapterKind},
    server::AppState,
};

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

pub async fn post(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> axum::response::Response {
    let started_at = Instant::now();
    let path = uri.path().to_string();
    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let span = tracing::info_span!(
        "proxy_request",
        method = %method,
        path = %path,
        session_id = session_id.as_deref().unwrap_or("")
    );

    async move {
        match forward(state, &uri, &headers, body).await {
            Ok((status, response)) => {
                tracing::info!(
                    upstream_status = status.as_u16(),
                    latency_ms = started_at.elapsed().as_millis(),
                    "proxied request"
                );
                response
            }
            Err(error) => {
                tracing::warn!(
                    latency_ms = started_at.elapsed().as_millis(),
                    error = %error.message,
                    "upstream request failed"
                );
                error.into_response()
            }
        }
    }
    .instrument(span)
    .await
}

struct ForwardError {
    message: String,
    response: axum::response::Response,
}

impl From<reqwest::Error> for ForwardError {
    fn from(error: reqwest::Error) -> Self {
        let message = error.to_string();
        Self {
            message,
            response: UpstreamError::from_reqwest(error).into_response(),
        }
    }
}

impl From<AdapterError> for ForwardError {
    fn from(error: AdapterError) -> Self {
        Self {
            message: error.message,
            response: error.response,
        }
    }
}

impl IntoResponse for ForwardError {
    fn into_response(self) -> axum::response::Response {
        self.response
    }
}

async fn forward(
    state: AppState,
    uri: &Uri,
    headers: &HeaderMap,
    body: Body,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let body = to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|error| {
            let message = error.to_string();
            ForwardError {
                message: message.clone(),
                response: UpstreamError::from_message(message).into_response(),
            }
        })?;
    let route = routing::resolve(&state.config, &body).map_err(|error| ForwardError {
        message: "failed to route request".to_string(),
        response: error.into_response(),
    })?;
    let body = body.to_vec();
    let result = match route.adapter {
        AdapterKind::Anthropic => {
            AnthropicAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
        AdapterKind::Responses => {
            ResponsesAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
    };
    result.map_err(ForwardError::from)
}
