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
    config::CountTokens,
    count_tokens,
    error::{ShuntError, UpstreamError},
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
            response: *error.response,
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
    // The Responses API has no token-counting endpoint. For a responses-routed
    // model, either count locally with tiktoken (opt-in) or return 404 so Claude
    // Code estimates tokens locally (gateway protocol). Either way we must NOT let
    // the request reach the responses adapter, which would translate it into — and
    // bill it as — a full inference call. Anthropic-routed models still pass
    // through to the upstream count_tokens endpoint below.
    if is_count_tokens(uri) && route.adapter == AdapterKind::Responses {
        let mode = state
            .config
            .provider(&route.provider)
            .map(|provider| provider.count_tokens)
            .unwrap_or(CountTokens::Estimate);
        return Ok(match mode {
            CountTokens::Tiktoken => {
                let input_tokens = count_tokens::count_input_tokens(&body);
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response(),
                )
            }
            CountTokens::Estimate => (StatusCode::NOT_FOUND, count_tokens_unsupported()),
        });
    }
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

fn is_count_tokens(uri: &Uri) -> bool {
    uri.path().ends_with("/count_tokens")
}

fn count_tokens_unsupported() -> axum::response::Response {
    ShuntError::new(
        StatusCode::NOT_FOUND,
        "not_found_error",
        "count_tokens is not available for this model; Claude Code estimates tokens locally",
    )
    .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::Uri;

    use super::is_count_tokens;

    #[test]
    fn detects_count_tokens_path() {
        assert!(is_count_tokens(
            &"/v1/messages/count_tokens".parse::<Uri>().unwrap()
        ));
        assert!(is_count_tokens(
            &"http://host/v1/messages/count_tokens?beta=true"
                .parse::<Uri>()
                .unwrap()
        ));
        assert!(!is_count_tokens(&"/v1/messages".parse::<Uri>().unwrap()));
    }
}
