use std::time::Instant;

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::IntoResponse,
};
use serde_json::Value;
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
    let body = normalize_request_body(body.to_vec());
    let (route, requested_model) =
        routing::resolve_request(&state.config, &body).map_err(|error| ForwardError {
            message: "failed to route request".to_string(),
            response: error.into_response(),
        })?;
    // Inbound client auth (M4): only routes where shunt injects a server-side
    // credential are gated — passthrough callers pay with their own credential.
    // The client-token header is stripped below either way (and on gated routes
    // the standard credential headers too), so a gate token never leaks
    // upstream.
    let (headers, gateway_claims) =
        check_inbound_auth(&state, &route, headers).map_err(|error| *error)?;
    enforce_managed_model_policy(&state, gateway_claims.as_ref(), &requested_model)
        .map_err(|error| *error)?;
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
    result.map(|(status, response)| {
        let response = crate::stream_metrics::observe_response(
            response,
            crate::stream_metrics::Protocol::Anthropic,
            provider,
            model,
            started_at,
        );
        (status, response)
    })
}

fn enforce_managed_model_policy(
    state: &AppState,
    claims: Option<&crate::gateway::jwt::Claims>,
    requested_model: &str,
) -> Result<(), Box<ForwardError>> {
    let Some((auth, claims)) = state.gateway_auth.as_ref().zip(claims) else {
        return Ok(());
    };
    let Some(available_models) = auth
        .managed_settings(&claims.email)
        .and_then(crate::gateway::managed::available_models)
    else {
        return Ok(());
    };
    let policy_model = routing::strip_context_window_hint(requested_model);
    if available_models
        .iter()
        .any(|model| model.as_str() == Some(policy_model))
    {
        return Ok(());
    }
    let message =
        format!("model \"{requested_model}\" is not permitted by this gateway's managed policy");
    Err(Box::new(ForwardError {
        message: message.clone(),
        response: ShuntError::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
            .into_response(),
    }))
}

/// Enforce configured client authentication on injected-credential routes and
/// strip every accepted credential slot from what gets forwarded upstream.
/// `[server.auth]` tokens and `[server.gateway]` JWTs compose as alternatives:
/// either valid credential grants access when both features are enabled.
fn check_inbound_auth(
    state: &AppState,
    route: &routing::Route,
    headers: &HeaderMap,
) -> Result<(HeaderMap, Option<crate::gateway::jwt::Claims>), Box<ForwardError>> {
    let mut forwarded = headers.clone();
    forwarded.remove("x-shunt-inbound-client");
    if let Some(auth) = &state.inbound_auth {
        forwarded.remove(auth.header());
    }

    let gateway_identity = state
        .gateway_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_bearer(headers));
    let injects_credential = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.auth != AuthMode::Passthrough)
        .unwrap_or(false);
    if !injects_credential || (state.inbound_auth.is_none() && state.gateway_auth.is_none()) {
        return Ok((forwarded, gateway_identity));
    }

    let static_client = state
        .inbound_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_client(headers));
    if static_client.is_some() || gateway_identity.is_some() {
        let client = static_client
            .map(str::to_string)
            .or_else(|| gateway_identity.as_ref().map(|claims| claims.email.clone()))
            .expect("one composed authentication branch matched");
        tracing::info!(client = %client, provider = %route.provider, "inbound client authenticated");
        forwarded.remove("authorization");
        forwarded.remove("x-api-key");
        if static_client.is_some() {
            if let Ok(client) = HeaderValue::from_str(&client) {
                forwarded.insert("x-shunt-inbound-client", client);
            }
        }
        return Ok((forwarded, gateway_identity));
    }

    tracing::warn!(provider = %route.provider, "inbound auth failed: missing or invalid client credential");
    let message = if let Some(auth) = &state.inbound_auth {
        format!(
            "missing or invalid credential: this gateway requires a client token (via {}, Authorization: Bearer, or x-api-key) or gateway login",
            auth.header()
        )
    } else {
        "missing or invalid credential: sign in to this gateway and send the issued bearer token"
            .to_string()
    };
    Err(Box::new(ForwardError {
        message: "inbound authentication failed".to_string(),
        response: ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
            .into_response(),
    }))
}

pub(crate) fn is_count_tokens(uri: &Uri) -> bool {
    uri.path().ends_with("/count_tokens")
}

fn normalize_request_body(body: Vec<u8>) -> Vec<u8> {
    let Ok(mut request) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    // Re-serialize only when a block was actually dropped. The common case (no
    // empty text block) keeps the original bytes and skips the encode entirely.
    if normalize_empty_text_blocks(&mut request) {
        serde_json::to_vec(&request).unwrap_or(body)
    } else {
        body
    }
}

pub(crate) fn normalize_empty_text_blocks(request: &mut Value) -> bool {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if content.iter().all(is_empty_text_block) {
            // Every block is empty text: a message with no text, tool_use, or
            // thinking at all. The #132 poisoned turns (tool-only /
            // reasoning-only) always carry a surviving tool_use or thinking
            // block, so the adapter never produces this shape — it is a
            // degenerate case only. Anthropic rejects both an empty `content`
            // array and an empty text block, and dropping the message would
            // break user/assistant alternation, so there is no local transform
            // that makes such a message valid. Keep the first block as a last
            // resort (truncating in place, no clone); the block may still be
            // empty (see the `keeps_one_block...` test).
            if content.len() > 1 {
                content.truncate(1);
                changed = true;
            }
        } else {
            let before = content.len();
            content.retain(|block| !is_empty_text_block(block));
            changed |= content.len() != before;
        }
    }
    changed
}

fn is_empty_text_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.trim().is_empty())
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
    use serde_json::json;

    use super::{is_count_tokens, is_empty_text_block, normalize_empty_text_blocks};

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

    #[test]
    fn strips_empty_text_blocks_and_preserves_other_content() {
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": ""},
                    {"type": "text", "text": "  \n"},
                    {"type": "thinking", "thinking": "reason"},
                    {"type": "tool_use", "id": "tool_1", "name": "work", "input": {}}
                ]
            }]
        });

        normalize_empty_text_blocks(&mut body);

        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "thinking", "thinking": "reason"},
                {"type": "tool_use", "id": "tool_1", "name": "work", "input": {}}
            ])
        );
    }

    #[test]
    fn truncates_an_all_empty_text_message_to_one_block() {
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": ""},
                    {"type": "text", "text": "  \n"}
                ]
            }]
        });

        assert!(normalize_empty_text_blocks(&mut body));

        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "must retain exactly one content block");
        assert!(is_empty_text_block(&content[0]));
    }

    // A message whose content is *only* empty text is a degenerate shape the
    // #132 adapter path never produces (tool-only / reasoning-only turns always
    // keep a tool_use or thinking block). Anthropic rejects both an empty
    // `content` array and an empty text block, and dropping the message would
    // break role alternation, so normalization keeps one block rather than risk
    // an alternation error — meaning the surviving block may itself still be
    // empty. This test pins that known limitation so the fallback is not
    // mistaken for a fix that guarantees a non-empty survivor.
    #[test]
    fn keeps_one_block_for_an_all_empty_text_message_even_if_still_empty() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": [{"type": "text", "text": "  "}]}]
        });

        normalize_empty_text_blocks(&mut body);

        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "must never leave an empty content array");
        // Known limitation: with no non-empty block to fall back to, the
        // surviving block is still the empty text block.
        assert!(is_empty_text_block(&content[0]));
    }
}
