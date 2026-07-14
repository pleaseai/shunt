use std::time::Instant;

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::IntoResponse,
};
use tracing::Instrument;

use crate::{
    adapters::{
        anthropic::AnthropicAdapter, cursor::CursorAdapter, responses::ResponsesAdapter, Adapter,
        AdapterError,
    },
    config::{AuthMode, CountTokens},
    count_tokens,
    error::{ShuntError, UpstreamError},
    model::responses::anthropic_error_type,
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
    // Snapshot the live config once, at request entry: a mid-request reload
    // never changes config underneath an in-flight request, while a request that
    // arrives after a reload sees the new config.
    let state = state.refreshed();
    let started_at = Instant::now();
    let path = uri.path().to_string();
    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    // The `session_id` span field rides into any span export — the OTel trace
    // bridge and the Sentry tracing layer both forward span fields — so withhold
    // the request-derived id unless the operator opted in for their backend
    // (`[otel] include_session_id` / `[sentry] include_session_id`). The decision
    // is pinned at startup (see `telemetry::withhold_session_id`), not read from
    // the hot-swappable request config: both exporters are built once and never
    // rebuilt on reload, so a mid-run config edit must not flip what the running
    // exporters emit. With neither exporting spans the id stays on the span for
    // local stderr logs, exactly as before.
    let span_session_id = if crate::telemetry::withhold_session_id() {
        ""
    } else {
        session_id.as_deref().unwrap_or("")
    };
    let span = tracing::info_span!(
        "proxy_request",
        method = %method,
        path = %path,
        session_id = span_session_id
    );

    async move {
        match forward(state, &uri, &headers, body, started_at).await {
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
    started_at: Instant,
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
    // Inbound client auth (M4): only routes where shunt injects a server-side
    // credential are gated — passthrough callers pay with their own credential.
    // The client-token header is stripped below either way, so it never leaks
    // upstream.
    let headers = check_inbound_auth(&state, &route, headers).map_err(|error| *error)?;
    let headers = &headers;
    // The Responses API has no token-counting endpoint. For a responses-routed
    // model, either count locally with tiktoken (the default) or return 501
    // `not_supported` so Claude Code falls back to estimating tokens. Either way
    // we must NOT let the request reach the responses adapter, which would
    // translate it into — and bill it as — a full inference call.
    // Anthropic-routed models still pass through to the upstream count_tokens
    // endpoint below.
    if is_count_tokens(uri) && matches!(route.adapter, AdapterKind::Responses | AdapterKind::Cursor)
    {
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
            CountTokens::Estimate => count_tokens_unsupported(),
        });
    }
    let body = body.to_vec();
    let provider = route.provider.clone();
    let model = route.model.clone();
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
        AdapterKind::Cursor => {
            CursorAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
    };
    let result = result.map_err(ForwardError::from);
    // Usage metrics count inference calls only, so Anthropic-routed
    // count_tokens requests are excluded here just like the Responses-routed
    // ones that early-returned above — cheap token counts would otherwise be
    // indistinguishable from real inference in the request/latency series.
    if !is_count_tokens(uri) {
        // For streaming responses this measures time to response headers, not
        // to stream completion — the body is forwarded without buffering.
        let status = match &result {
            Ok((status, _)) => status.as_u16(),
            Err(error) => error.response.status().as_u16(),
        };
        crate::metrics::record_proxied_request(
            &provider,
            &model,
            status,
            started_at.elapsed().as_secs_f64() * 1000.0,
        );
    }
    result
}

/// Enforce `[server.auth]` on injected-credential routes and strip the client
/// token header from what gets forwarded upstream. Returns the headers to
/// forward. Token values are never logged.
fn check_inbound_auth(
    state: &AppState,
    route: &routing::Route,
    headers: &HeaderMap,
) -> Result<HeaderMap, Box<ForwardError>> {
    let mut forwarded = headers.clone();
    forwarded.remove("x-shunt-inbound-client");
    let Some(auth) = &state.inbound_auth else {
        return Ok(forwarded);
    };
    forwarded.remove(auth.header());

    let injects_credential = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.auth != AuthMode::Passthrough)
        .unwrap_or(false);
    if !injects_credential {
        return Ok(forwarded);
    }

    match auth.authenticate(headers) {
        Some(client) => {
            tracing::info!(client = %client, provider = %route.provider, "inbound client authenticated");
            if let Ok(client) = HeaderValue::from_str(client) {
                forwarded.insert("x-shunt-inbound-client", client);
            }
            Ok(forwarded)
        }
        None => {
            tracing::warn!(provider = %route.provider, "inbound auth failed: missing or invalid client token");
            let message = format!(
                "missing or invalid {} header: this gateway requires a client token for mapped models; ask the operator for one",
                auth.header()
            );
            Err(Box::new(ForwardError {
                message: "inbound authentication failed".to_string(),
                response: ShuntError::new(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    message,
                )
                .into_response(),
            }))
        }
    }
}

pub(crate) fn is_count_tokens(uri: &Uri) -> bool {
    uri.path().ends_with("/count_tokens")
}

fn count_tokens_unsupported() -> (StatusCode, axum::response::Response) {
    let status = StatusCode::NOT_IMPLEMENTED;
    (
        status,
        ShuntError::new(
            status,
            anthropic_error_type(status),
            "count_tokens is not available for this model; Claude Code estimates tokens locally",
        )
        .into_response(),
    )
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
