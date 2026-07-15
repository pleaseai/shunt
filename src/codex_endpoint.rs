//! Inbound OpenAI Responses (Codex) endpoint (`[server.codex_endpoint]`).
//!
//! Lets the OpenAI Codex CLI point its `chatgpt_base_url` (or a custom
//! `model_provider`) at shunt and be load-balanced across a ChatGPT/Codex OAuth
//! account pool. Unlike the Anthropic Messages path (`/v1/messages`), this is a
//! **raw passthrough**: the inbound Responses body is forwarded upstream
//! unchanged and the upstream response is relayed verbatim — only the M10
//! account-pool machinery (selection, failover, refresh) is reused. See
//! `docs/m11-inbound-codex-endpoint.md`.

use std::time::Instant;

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::Instrument;

use crate::{
    adapters::{responses, AdapterError},
    error::{ShuntError, UpstreamError},
    routing::{AdapterKind, Route},
    server::AppState,
};

/// Same inbound body cap as the Anthropic Messages path (`proxy::post`).
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Minimal view of the inbound Responses body: the `model` is read only for
/// metrics/logging labels — the body itself forwards upstream byte-for-byte, so
/// a missing or malformed model never blocks the request (the upstream rejects it).
#[derive(Debug, Deserialize)]
struct ModelView {
    model: Option<String>,
}

/// Handler for the inbound Responses routes (`/backend-api/codex/responses`,
/// `/responses`, `/v1/responses`). Mirrors `proxy::post`'s shape: snapshot the
/// live state, trace the request, and relay a gateway-owned error as a response.
pub async fn post(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> axum::response::Response {
    let state = state.refreshed();
    let started_at = Instant::now();
    let path = uri.path().to_string();
    // The Codex CLI keys a conversation with a `session-id` header; fall back to
    // Claude Code's header for parity. Used both for the tracing span and as the
    // account-pool sticky key so one conversation stays on one account.
    let session_id = headers
        .get("session-id")
        .or_else(|| headers.get("x-claude-code-session-id"))
        .and_then(|value| value.to_str().ok())
        .filter(|session_id| !session_id.is_empty())
        .map(ToOwned::to_owned);
    // Withhold the request-derived id from exported spans unless the operator
    // opted in per backend (same rule as `proxy::post`).
    let span_session_id = if crate::telemetry::withhold_session_id() {
        ""
    } else {
        session_id.as_deref().unwrap_or("")
    };
    let span = tracing::info_span!(
        "codex_endpoint_request",
        method = %method,
        path = %path,
        session_id = span_session_id
    );

    async move {
        match forward(state, session_id, headers, body, started_at).await {
            Ok((status, response)) => {
                tracing::info!(
                    upstream_status = status.as_u16(),
                    latency_ms = started_at.elapsed().as_millis(),
                    "proxied inbound codex request"
                );
                response
            }
            Err(error) => {
                // Log *why* the request failed before returning the client-facing
                // response — without this a shunt-owned failure (bad credential,
                // unreachable backend, exhausted pool) leaves no server-side signal
                // an operator could grep. Mirrors `proxy::post`.
                tracing::warn!(
                    latency_ms = started_at.elapsed().as_millis(),
                    error = %error.message,
                    "inbound codex request failed"
                );
                // Gateway-owned errors on this endpoint are built with the gateway's
                // Anthropic-shaped responders (`ShuntError` / `UpstreamError` /
                // adapter+auth `AdapterError`s). A Codex CLI (or any OpenAI Responses
                // client) pointed here expects the OpenAI `{"error":{...}}` envelope,
                // so re-shape at this single boundary (status preserved). Relayed
                // upstream errors never reach here — they return verbatim as `Ok`.
                crate::error::into_openai_error_shape(error.response).await
            }
        }
    }
    .instrument(span)
    .await
}

/// A gateway-owned error from [`forward`] carrying a log message alongside the
/// client-facing response, so [`post`] can record *why* the request failed
/// (mirrors `proxy::ForwardError`). An upstream error response relayed verbatim is
/// an `Ok`, not this — only shunt-owned failures (config, auth, body read, account
/// resolution/transport) surface here.
struct ForwardError {
    message: String,
    response: axum::response::Response,
}

impl From<AdapterError> for ForwardError {
    fn from(error: AdapterError) -> Self {
        Self {
            message: error.message,
            response: *error.response,
        }
    }
}

async fn forward(
    state: AppState,
    session_id: Option<String>,
    headers: HeaderMap,
    body: Body,
    started_at: Instant,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    // The routes are only registered when `[server.codex_endpoint]` is set, but
    // read the snapshot defensively; config validation guarantees the named
    // provider exists and uses `chatgpt_oauth`.
    let Some(codex_endpoint) = &state.config.server.codex_endpoint else {
        return Err(ForwardError {
            message: "codex endpoint is not configured".to_string(),
            response: ShuntError::bad_gateway("codex endpoint is not configured".to_string())
                .into_response(),
        });
    };
    let provider = codex_endpoint.provider.clone();

    // Inbound client auth (M4): the target provider injects a server-side Codex
    // bearer, so a configured `[server.auth]` gates this endpoint. The passthrough
    // forwards the Codex CLI's own request headers verbatim but swaps in the pool
    // account's credential and strips the shunt client-token header (in
    // `forward_codex_inbound`), so neither the client's own credential nor the
    // shunt token ever reaches the Codex backend.
    // The authenticated inbound client's name, used below to namespace the
    // account-pool sticky key. `None` when no `[server.auth]` is configured
    // (single-tenant: the bare session id keys the pool).
    let inbound_client = if let Some(auth) = &state.inbound_auth {
        // Accept the shunt token via the configured header OR an OpenAI-style
        // `Authorization: Bearer <token>` (the `OPENAI_API_KEY` / `env_key` idiom
        // the Codex CLI and llmgateway/LiteLLM setups use), so no custom header is
        // required. The client's Bearer is only checked here — it is stripped and
        // never forwarded upstream (see `forward_codex_inbound`).
        match auth.authenticate_bearer(&headers) {
            Some(client) => Some(client.to_string()),
            None => {
                tracing::warn!(
                    provider = %provider,
                    "inbound codex auth failed: missing or invalid client token"
                );
                let message = format!(
                    "missing or invalid client token for the inbound codex endpoint: provide it via the `{}` header or `Authorization: Bearer <token>` (e.g. OPENAI_API_KEY); ask the operator for one",
                    auth.header()
                );
                return Err(ForwardError {
                    message: "inbound authentication failed".to_string(),
                    response: ShuntError::new(
                        StatusCode::UNAUTHORIZED,
                        "authentication_error",
                        message,
                    )
                    .into_response(),
                });
            }
        }
    } else {
        None
    };

    let body = to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|error| {
            let message = error.to_string();
            ForwardError {
                message: message.clone(),
                response: UpstreamError::from_message(message).into_response(),
            }
        })?;

    // Read the model for metrics/logging only; the body forwards verbatim.
    let model = serde_json::from_slice::<ModelView>(&body)
        .ok()
        .and_then(|view| view.model)
        .unwrap_or_else(|| "unknown".to_string());
    // The body-`model` does not pick a provider (the endpoint is pinned to one
    // `chatgpt_oauth` provider). `request_builder` only reads `route.provider`,
    // so `model`/`upstream_model` are labels, not routing inputs.
    let route = Route {
        provider: provider.clone(),
        adapter: AdapterKind::Responses,
        model: model.clone(),
        upstream_model: model.clone(),
        effort: None,
    };

    // Namespace the account-pool sticky key with the authenticated client so that,
    // in a multi-tenant deployment, one client cannot pin another client's Codex
    // session onto a chosen pool account by replaying its `session-id` header. This
    // mirrors the outbound Responses path's `{client}:{session_id}` pool key (see
    // `adapters/responses/mod.rs`). The raw `session_id` is still what the tracing
    // span records above; only the pool key is namespaced.
    let pool_key = pool_sticky_key(inbound_client.as_deref(), session_id);

    // Pass the client's inbound headers through so the passthrough can forward the
    // Codex CLI's own request headers verbatim (swapping only the credential); the
    // shunt client-token header is stripped inside `forward_codex_inbound`.
    let result = responses::forward_codex_inbound(state, route, pool_key, headers, body).await;
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
    result.map_err(ForwardError::from)
}

/// Namespace the account-pool sticky key with the authenticated inbound client so
/// that, in a multi-tenant deployment, one client cannot pin another client's Codex
/// session onto a chosen pool account by replaying its `session-id` header. Mirrors
/// the outbound Responses path's `{client}:{session_id}` key (`adapters/responses/mod.rs`).
/// With no inbound auth (`client == None`) the bare session id is used — single-tenant,
/// there is no client identity to bind. Returns `None` when the request carries no
/// session id (nothing to key the pool on).
fn pool_sticky_key(client: Option<&str>, session_id: Option<String>) -> Option<String> {
    session_id.map(|session_id| match client {
        Some(client) => format!("{client}:{session_id}"),
        None => session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::pool_sticky_key;

    #[test]
    fn prefixes_the_authenticated_client() {
        assert_eq!(
            pool_sticky_key(Some("alice"), Some("sess-1".to_string())),
            Some("alice:sess-1".to_string())
        );
    }

    #[test]
    fn distinguishes_clients_sharing_a_session_id() {
        // Two tenants replaying the same `session-id` must not collide on the pool,
        // so one cannot pin another's session onto a chosen account.
        let alice = pool_sticky_key(Some("alice"), Some("shared".to_string()));
        let bob = pool_sticky_key(Some("bob"), Some("shared".to_string()));
        assert_ne!(alice, bob);
    }

    #[test]
    fn falls_back_to_the_bare_session_without_auth() {
        assert_eq!(
            pool_sticky_key(None, Some("sess-1".to_string())),
            Some("sess-1".to_string())
        );
    }

    #[test]
    fn is_none_without_a_session_id() {
        assert_eq!(pool_sticky_key(Some("alice"), None), None);
        assert_eq!(pool_sticky_key(None, None), None);
    }
}
