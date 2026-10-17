//! Inbound relay for the Codex CLI's built-in web-search tool.
//!
//! The Codex CLI runs its `web.run` tool (a `search_query`, an `open`, …) by
//! posting to `{base_url}/alpha/search` — the same base URL it appends
//! `/responses` to for a turn. With that base URL pointed at shunt, those calls
//! must reach the ChatGPT/Codex backend's `/codex/alpha/search` over the same
//! account pool as the turns, or every web search fails with an empty `404`.
//!
//! The relay shares the Responses routes' `[server.auth]` gate, body limit,
//! session-sticky pool selection, failover and refresh, and relays the body
//! and the upstream reply verbatim. Two things differ from a turn: it is
//! HTTP-only (the operation has no WebSocket form, so only `POST` is
//! registered), and it is not a model turn, so
//! `[[server.codex_endpoint.routes]]` never applies — a search always goes to
//! the endpoint's own provider. See `docs/m11-inbound-codex-endpoint.md`.

use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
};

use crate::{
    adapters::responses::{self, CodexOperation},
    routing::{AdapterKind, Route},
    server::AppState,
};

use super::ForwardError;

/// Inbound web-search routes, registered by [`crate::server::build_router`]
/// next to [`super::PATHS`] when `[server.codex_endpoint]` is set: one per
/// Codex CLI base-URL style, as for `/responses`. `concurrency::is_codex_path`
/// classifies against this set too, so a gateway-owned error on these paths
/// uses the OpenAI Responses envelope.
pub(crate) const PATHS: [&str; 3] = [
    "/backend-api/codex/alpha/search",
    "/alpha/search",
    "/v1/alpha/search",
];

/// The metrics and pool-selection label of a search request. It names no
/// model, so a per-model cooldown recorded for a turn never blocks a search.
const LABEL: &str = "web_search";

/// Handler for the inbound web-search routes ([`PATHS`]).
pub async fn post(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> axum::response::Response {
    super::serve(
        state,
        method,
        uri,
        headers,
        body,
        CodexOperation::AlphaSearch,
    )
    .await
}

/// Relay an authenticated, already-read search body over `provider`'s pool.
pub(super) async fn forward(
    state: AppState,
    provider: String,
    pool_key: Option<String>,
    headers: HeaderMap,
    body: Bytes,
    started_at: Instant,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let route = Route {
        provider: provider.clone(),
        adapter: AdapterKind::Responses,
        model: LABEL.to_string(),
        upstream_model: LABEL.to_string(),
        effort: None,
        service_tier: None,
    };
    let result = responses::forward_codex_inbound(
        state,
        route,
        pool_key,
        headers,
        body,
        CodexOperation::AlphaSearch,
    )
    .await;
    super::record_outcome(provider, LABEL.to_string(), started_at, result)
}
