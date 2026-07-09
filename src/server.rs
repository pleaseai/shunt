use axum::{
    routing::{head, post},
    Router,
};

use crate::{config::Config, proxy};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub http_client: reqwest::Client,
}

pub fn build_router(config: Config) -> Router {
    let state = AppState {
        config,
        http_client: reqwest::Client::new(),
    };

    Router::new()
        .route("/", head(root_probe))
        .route("/v1/messages", post(proxy::post))
        .route("/v1/messages/count_tokens", post(proxy::post))
        .with_state(state)
}

async fn root_probe() {}
