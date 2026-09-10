//! Inbound OpenAI Responses (Codex) endpoint (`[server.codex_endpoint]`).
//!
//! Lets the OpenAI Codex CLI point its `chatgpt_base_url` (or a custom
//! `model_provider`) at shunt and be load-balanced across a ChatGPT/Codex OAuth
//! account pool. Unlike the Anthropic Messages path (`/v1/messages`), this is a
//! **raw passthrough**: the inbound Responses body is forwarded upstream
//! unchanged and the upstream response is relayed verbatim — only the M10
//! account-pool machinery (selection, failover, refresh) is reused.
//!
//! Two dispatch modes share that shape (issue #436). By default every inbound
//! request goes to the one configured `chatgpt_oauth` provider and the body
//! `model` is a metrics label only. When `[[server.codex_endpoint.routes]]`
//! declares an entry for the model the client asked for, the request instead
//! goes to that entry's provider — another ChatGPT/Codex pool, or a third-party
//! Responses-compatible upstream via [`responses::forward_codex_routed`] — with
//! the body `model` rewritten to the route's `upstream_model` when they differ.
//! See `docs/m11-inbound-codex-endpoint.md`.

use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use tracing::Instrument;

use crate::{
    adapters::{responses, AdapterError},
    config::CodexRouteConfig,
    error::ShuntError,
    routing::{AdapterKind, Route},
    server::AppState,
};

pub mod frame;
mod model;
mod routing;
pub mod websocket;

#[cfg(test)]
use self::model::model_label;
use self::model::{resolve_model, UNKNOWN_MODEL};
use self::routing::{identity_body, BodyError};

/// Inbound Responses routes this handler serves, registered by
/// [`crate::server::build_router`] when `[server.codex_endpoint]` is set.
///
/// This is the single source of truth for the path set: the router registers
/// exactly these, and `concurrency::is_codex_path` classifies against them so a
/// gateway-owned error on any of them uses the OpenAI Responses envelope rather
/// than the Anthropic one (AGENTS.md). Adding a path here registers it and gives
/// it the right error shape together — they cannot drift apart.
pub(crate) const PATHS: [&str; 3] = [
    "/backend-api/codex/responses",
    "/responses",
    "/v1/responses",
];

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
    let session_id = extract_session_id(&headers);
    // Withhold the request-derived id from exported spans unless the operator
    // opted in per backend (same rule as `proxy::post`).
    let span_session_id = if crate::telemetry::withhold_session_id() {
        ""
    } else {
        session_id.as_deref().unwrap_or("")
    };
    // See `proxy::post`'s equivalent span for why these start empty: the model
    // and outcome are only known inside `forward`, once the body is parsed and
    // the upstream has responded (`crate::observability`, #281).
    let span = tracing::info_span!(
        "codex_endpoint_request",
        method = %method,
        path = %path,
        session_id = span_session_id,
        gen_ai.request.model = tracing::field::Empty,
        shunt.provider = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty
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
                crate::error::into_openai_error_shape(*error.response).await
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
pub(crate) struct ForwardError {
    pub(crate) message: String,
    /// Boxed to keep `Result<_, ForwardError>` small: an `axum` `Response` alone
    /// is 128 bytes, which trips `clippy::result_large_err` on [`forward`].
    pub(crate) response: Box<axum::response::Response>,
}

impl From<AdapterError> for ForwardError {
    fn from(error: AdapterError) -> Self {
        Self {
            message: error.message,
            response: error.response,
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
            response: Box::new(
                ShuntError::bad_gateway("codex endpoint is not configured".to_string())
                    .into_response(),
            ),
        });
    };
    let provider = codex_endpoint.provider.clone();

    let inbound_client = authenticate_inbound(state.inbound_auth.as_deref(), &headers, &provider)
        .map_err(|err| ForwardError {
        message: "inbound authentication failed".to_string(),
        response: Box::new(err.into_response()),
    })?;

    let max_request_bytes = state.config.server.limits.max_request_bytes;
    if crate::http_tuning::content_length_exceeds(&headers, max_request_bytes) {
        return Err(ForwardError {
            message: "request body exceeds the configured limit".to_string(),
            response: Box::new(crate::http_tuning::request_too_large(true).await),
        });
    }
    let body = crate::http_tuning::read_body(body, max_request_bytes, true)
        .await
        .map_err(|response| ForwardError {
            message: if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
                "request body exceeds the configured limit"
            } else {
                "failed to read request body"
            }
            .to_string(),
            response,
        })?;

    let pool_key = pool_sticky_key(inbound_client.as_deref(), session_id);
    forward_turn(state, None, pool_key, headers, body, started_at).await
}

pub(crate) fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("session-id")
        .or_else(|| headers.get("x-claude-code-session-id"))
        .and_then(|value| value.to_str().ok())
        .filter(|session_id| !session_id.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn authenticate_inbound(
    auth: Option<&crate::auth::inbound::InboundAuth>,
    headers: &HeaderMap,
    provider: &str,
) -> Result<Option<String>, ShuntError> {
    if let Some(auth) = auth {
        match auth.authenticate_bearer(headers) {
            Some(client) => Ok(Some(client.to_string())),
            None => {
                tracing::warn!(
                    provider = %provider,
                    "inbound codex auth failed: missing or invalid client token"
                );
                let message = format!(
                    "missing or invalid client token for the inbound codex endpoint: provide it via the `{}` header or `Authorization: Bearer <token>` (e.g. OPENAI_API_KEY); ask the operator for one",
                    auth.header()
                );
                Err(ShuntError::new(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    message,
                ))
            }
        }
    } else {
        Ok(None)
    }
}

pub(crate) async fn forward_turn(
    state: AppState,
    model: Option<String>,
    pool_key: Option<String>,
    headers: HeaderMap,
    body: Bytes,
    started_at: Instant,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let Some(codex_endpoint) = &state.config.server.codex_endpoint else {
        return Err(ForwardError {
            message: "codex endpoint is not configured".to_string(),
            response: Box::new(
                ShuntError::bad_gateway("codex endpoint is not configured".to_string())
                    .into_response(),
            ),
        });
    };
    let default_provider = codex_endpoint.provider.clone();
    let max_request_bytes = state.config.server.limits.max_request_bytes;

    let (matched, model_name, decoded) = if let Some(m) = model {
        let matched = codex_endpoint.route_for(&m).cloned();
        (matched, m, None)
    } else {
        let resolved = resolve_model(
            &headers,
            &body,
            max_request_bytes,
            !codex_endpoint.routes.is_empty(),
        )
        .await;
        let matched = resolved
            .model
            .as_deref()
            .and_then(|m| codex_endpoint.route_for(m))
            .cloned();
        let name = resolved
            .model
            .as_deref()
            .unwrap_or(UNKNOWN_MODEL)
            .to_string();
        (matched, name, resolved.decoded)
    };

    crate::observability::record_requested_model(&model_name);

    let (provider, result) = match matched {
        Some(route) => {
            dispatch_routed(
                state,
                route,
                model_name.clone(),
                pool_key,
                headers,
                body,
                decoded,
                max_request_bytes,
            )
            .await?
        }
        None => {
            drop(decoded);
            let route = Route {
                provider: default_provider.clone(),
                adapter: AdapterKind::Responses,
                model: model_name.clone(),
                upstream_model: model_name.clone(),
                effort: None,
                service_tier: None,
            };
            let result =
                responses::forward_codex_inbound(state, route, pool_key, headers, body).await;
            (default_provider, result)
        }
    };

    record_outcome(provider, model_name, started_at, result)
}

/// Dispatch a request whose `model` matched a `[[server.codex_endpoint.routes]]`
/// entry. Returns the provider the observability tail should label the request
/// with (the *routed* provider, not the endpoint's configured default) alongside
/// the forwarder's result.
///
/// A ChatGPT/Codex-backed route keeps the full pool passthrough
/// (`forward_codex_inbound`: selection, failover, refresh, `x-shunt-account`)
/// and forwards the client's own headers; every other provider goes through
/// [`responses::forward_codex_routed`], which sends a single credential over a
/// fresh header allowlist. Only the routed provider's `upstream_model` can
/// differ from what the client asked for, so the body is rewritten only then.
///
/// `decoded` is the decoded body `resolve_model` already produced for a zstd
/// request, threaded through so the routed path does not decode the same body a
/// second time (PR #478 review, P2).
#[allow(clippy::too_many_arguments)]
async fn dispatch_routed(
    state: AppState,
    route_config: CodexRouteConfig,
    model: String,
    pool_key: Option<String>,
    mut headers: HeaderMap,
    body: Bytes,
    decoded: Option<Bytes>,
    max_request_bytes: usize,
) -> Result<
    (
        String,
        Result<(StatusCode, axum::response::Response), AdapterError>,
    ),
    ForwardError,
> {
    let upstream_model = route_config.upstream_model().to_string();
    let chatgpt_backend = state.config.is_chatgpt_backend(&route_config.provider);
    let rewrite = (upstream_model != model).then_some(upstream_model.as_str());
    let route = Route {
        provider: route_config.provider.clone(),
        adapter: AdapterKind::Responses,
        // The public, client-requested id labels metrics and spans; only the
        // wire body and `upstream_model` carry the route's upstream id.
        model: model.clone(),
        upstream_model: upstream_model.clone(),
        effort: None,
        service_tier: None,
    };

    if rewrite.is_some() {
        // The Codex CLI sends `x-codex-routing-hint: model=<public id>`, and the
        // pool passthrough forwards the client's headers verbatim. Once the body
        // names the route's `upstream_model` instead, that hint contradicts it —
        // a routing signal the ChatGPT backend reads, pointing at a model the
        // request no longer asks for. shunt cannot re-derive the hint (it does
        // not own the upstream's grammar for the routed id), so drop it and let
        // the backend route on the body alone. The routed third-party path
        // builds a fresh allowlist that never carried it.
        headers.remove("x-codex-routing-hint");
    }

    // The ChatGPT backend accepts the inbound encoding as-is, so it only needs a
    // materialized body when the `model` actually changes. A third-party
    // Responses API does not accept a zstd-encoded request, so that path always
    // sends identity bytes — decoded even when nothing is rewritten.
    let body = if chatgpt_backend && rewrite.is_none() {
        // The original bytes forward as-is, so the decoded copy is dead weight
        // for the whole upstream round trip — release it, as the unrouted path
        // does (PR #478 review, P2).
        drop(decoded);
        body
    } else {
        let prepared =
            match identity_body(&headers, &body, decoded, rewrite, max_request_bytes).await {
                Ok(prepared) => prepared,
                Err(error) => return Err(routed_body_error(error, &route_config.provider).await),
            };
        // The prepared body is identity-encoded; leaving the inbound
        // `content-encoding` on would tell the upstream to inflate plain bytes.
        headers.remove(axum::http::header::CONTENT_ENCODING);
        prepared
    };

    let result = if chatgpt_backend {
        responses::forward_codex_inbound(state, route, pool_key, headers, body).await
    } else {
        responses::forward_codex_routed(state, route, headers, body).await
    };
    Ok((route_config.provider, result))
}

/// Turn a routed body failure into the gateway-owned response the client sees.
/// Both arms are re-shaped into the OpenAI error envelope by [`post`].
async fn routed_body_error(error: BodyError, provider: &str) -> ForwardError {
    match error {
        BodyError::TooLarge => ForwardError {
            message: "request body exceeds the configured limit".to_string(),
            response: Box::new(crate::http_tuning::request_too_large(true).await),
        },
        BodyError::Invalid => {
            tracing::warn!(
                provider = %provider,
                "routed inbound codex request rejected: its body could not be prepared"
            );
            ForwardError {
                message: "routed request body could not be prepared".to_string(),
                response: Box::new(
                    ShuntError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "the request body could not be read as a JSON object, so its `model` \
                         could not be rewritten for the configured \
                         [[server.codex_endpoint.routes]] entry"
                            .to_string(),
                    )
                    .into_response(),
                ),
            }
        }
    }
}

/// Record the request's outcome on the span, Sentry, and the metrics registry,
/// then wrap the response in the streaming-metrics observer. Shared by both
/// dispatch modes so a routed request is observed exactly like a fixed one —
/// labeled with the provider that actually served it and the model the client
/// asked for.
fn record_outcome(
    provider: String,
    model: String,
    started_at: Instant,
    result: Result<(StatusCode, axum::response::Response), AdapterError>,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let status_code = match &result {
        Ok((status, _)) => *status,
        Err(error) => error.response.status(),
    };
    crate::observability::record_span_outcome(&provider, status_code);
    crate::observability::capture_upstream_outcome(&provider, &model, status_code);
    crate::metrics::record_proxied_request(
        &provider,
        &model,
        status_code.as_u16(),
        started_at.elapsed().as_secs_f64() * 1000.0,
    );
    result
        .map(|(status, response)| {
            let response = crate::stream_metrics::observe_response(
                response,
                crate::stream_metrics::Protocol::Responses,
                provider,
                model,
                started_at,
            );
            (status, response)
        })
        .map_err(ForwardError::from)
}

/// Namespace the account-pool sticky key with the authenticated inbound client so
/// that, in a multi-tenant deployment, one client cannot pin another client's Codex
/// session onto a chosen pool account by replaying its `session-id` header. Mirrors
/// the outbound Responses path's `{client}:{session_id}` key (`adapters/responses/mod.rs`).
/// With no inbound auth (`client == None`) the bare session id is used — single-tenant,
/// there is no client identity to bind. Returns `None` when the request carries no
/// session id (nothing to key the pool on).
pub(crate) fn pool_sticky_key(client: Option<&str>, session_id: Option<String>) -> Option<String> {
    session_id.map(|session_id| match client {
        Some(client) => format!("{client}:{session_id}"),
        None => session_id,
    })
}

#[cfg(test)]
mod tests;
