use std::sync::Arc;

use axum::{
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::{
    auth::inbound::InboundAuth,
    config::{Config, ConfigError},
    discovery, proxy,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub http_client: reqwest::Client,
    /// Inbound client-token auth, resolved once at startup (None ⇒ open).
    pub inbound_auth: Option<Arc<InboundAuth>>,
}

pub fn build_router(config: Config) -> Result<Router, ConfigError> {
    let inbound_auth = config
        .server
        .auth
        .as_ref()
        .map(|auth| auth.resolve())
        .transpose()?
        .map(Arc::new);
    let state = AppState {
        config,
        http_client: reqwest::Client::new(),
        inbound_auth,
    };

    // `/` and `/health` stay unauthenticated even when `[server.auth]` is
    // configured (healthcheck tools rarely carry tokens); they must never
    // expose config, credentials, or upstream details — only version, status,
    // and the already-public endpoint list.
    Ok(Router::new()
        .route("/", get(root_index))
        .route("/health", get(health))
        .route("/v1/models", get(discovery::get))
        .route("/v1/messages", post(proxy::post))
        .route("/v1/messages/count_tokens", post(proxy::post))
        .with_state(state))
}

/// Human-facing landing page; axum also serves HEAD `/` from this handler,
/// which keeps the pre-existing liveness probe working.
async fn root_index() -> String {
    format!(
        "shunt v{} — Anthropic Messages proxy. Endpoints: /v1/models, /v1/messages, /v1/messages/count_tokens, /health\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

/// Machine-facing liveness endpoint: the process is up and config loaded
/// (the router cannot exist otherwise). Deliberately does not check upstream
/// connectivity — that is decided per request and would only cause flapping.
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
