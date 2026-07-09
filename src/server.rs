use std::sync::Arc;

use axum::{
    routing::{get, head, post},
    Router,
};

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

    Ok(Router::new()
        .route("/", head(root_probe))
        .route("/v1/models", get(discovery::get))
        .route("/v1/messages", post(proxy::post))
        .route("/v1/messages/count_tokens", post(proxy::post))
        .with_state(state))
}

async fn root_probe() {}
