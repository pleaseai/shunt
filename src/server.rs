use std::sync::Arc;

use axum::{
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::{
    accounts::AccountPool,
    auth::inbound::InboundAuth,
    config::{Config, ConfigError},
    discovery, protocol, proxy,
    reload::{RuntimeState, SharedState},
    routes,
};

#[derive(Clone)]
pub struct AppState {
    /// Per-request config snapshot (see [`AppState::refreshed`]).
    pub config: Arc<Config>,
    pub http_client: reqwest::Client,
    pub accounts: Arc<AccountPool>,
    /// Inbound client-token auth snapshot for this request (None ⇒ open).
    pub inbound_auth: Option<Arc<InboundAuth>>,
    /// The live, hot-swappable runtime state a reload updates. Private so the
    /// only way in is a snapshot method that keeps `config`/`inbound_auth`
    /// consistent with it.
    shared: SharedState,
}

impl AppState {
    /// Build state from a config, owning a fresh shared store. Used by tests and
    /// by callers that do not thread an external [`SharedState`].
    pub fn new(config: Config, http_client: reqwest::Client) -> Result<Self, ConfigError> {
        let runtime = RuntimeState::from_config(config)?;
        let shared: SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(runtime));
        Ok(Self::from_shared(
            shared,
            http_client,
            Arc::new(AccountPool::new()),
        ))
    }

    /// Snapshot the current runtime state from an existing shared store.
    pub fn from_shared(
        shared: SharedState,
        http_client: reqwest::Client,
        accounts: Arc<AccountPool>,
    ) -> Self {
        let current = shared.load();
        Self {
            config: current.config.clone(),
            inbound_auth: current.inbound_auth.clone(),
            http_client,
            accounts,
            shared,
        }
    }

    /// Re-snapshot the live shared state into a new `AppState`, so a request
    /// entry picks up the latest reloaded config while holding one stable
    /// snapshot for the whole request. Cheap: clones `Arc`s and the client.
    pub(crate) fn refreshed(&self) -> Self {
        Self::from_shared(
            self.shared.clone(),
            self.http_client.clone(),
            self.accounts.clone(),
        )
    }
}

/// Build the router and return it alongside the [`SharedState`] it reads, so the
/// caller can spawn reload watchers that hot-swap the same store.
pub fn build_router(config: Config) -> Result<(Router, SharedState), ConfigError> {
    let runtime = RuntimeState::from_config(config)?;
    let shared: SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(runtime));
    let state = AppState::from_shared(
        shared.clone(),
        reqwest::Client::new(),
        Arc::new(AccountPool::new()),
    );

    // `/` and `/health` stay unauthenticated even when `[server.auth]` is
    // configured (healthcheck tools rarely carry tokens); they must never
    // expose config, credentials, or upstream details — only version, status,
    // and the already-public endpoint list.
    let router = Router::new()
        .route("/", get(root_index))
        .route("/health", get(health))
        .route("/protocol", get(protocol::get))
        .route("/v1/models", get(discovery::get))
        .route("/routes", get(routes::get))
        .route("/v1/messages", post(proxy::post))
        .route("/v1/messages/count_tokens", post(proxy::post))
        .with_state(state);
    Ok((router, shared))
}

/// Human-facing landing page; axum also serves HEAD `/` from this handler,
/// which keeps the pre-existing liveness probe working.
async fn root_index() -> String {
    format!(
        "shunt v{} — Anthropic Messages proxy. Endpoints: /v1/models, /routes, /v1/messages, /v1/messages/count_tokens, /protocol, /health\n",
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
