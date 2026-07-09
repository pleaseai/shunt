use std::time::Instant;

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, Response, StatusCode, Uri},
    response::IntoResponse,
};
use tracing::Instrument;

use crate::{error::UpstreamError, headers, server::AppState};

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
    response: UpstreamError,
}

impl From<reqwest::Error> for ForwardError {
    fn from(error: reqwest::Error) -> Self {
        let message = error.to_string();
        Self {
            message,
            response: UpstreamError::from_reqwest(error),
        }
    }
}

impl IntoResponse for ForwardError {
    fn into_response(self) -> axum::response::Response {
        self.response.into_response()
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
                response: UpstreamError::from_message(message),
            }
        })?;
    let upstream_url = upstream_url(&state, uri);
    let upstream = state
        .http_client
        .post(upstream_url)
        .headers(headers::filtered(headers))
        .body(body)
        .send()
        .await?;
    let status = upstream.status();
    let response_headers = headers::filtered(upstream.headers());
    let stream = upstream.bytes_stream();

    let mut builder = Response::builder().status(status);
    for (name, value) in response_headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }

    let response = builder
        .body(Body::from_stream(stream))
        .expect("response builder uses valid upstream status and headers")
        .into_response();
    Ok((status, response))
}

fn upstream_url(state: &AppState, uri: &Uri) -> String {
    let base = state
        .config
        .providers
        .anthropic
        .base_url
        .trim_end_matches('/');
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(uri.path());
    format!("{base}{path_and_query}")
}
