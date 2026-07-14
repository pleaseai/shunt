use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::{error::ShuntError, server::AppState};

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Snapshot the live config so this response reflects the latest reload.
    let state = state.refreshed();
    if let Some(auth) = &state.inbound_auth {
        let Some(client) = auth.authenticate_client(&headers) else {
            tracing::warn!(
                "inbound auth failed for GET /v1/models: missing or invalid client token"
            );
            let message = format!(
                "missing or invalid credential: this gateway requires a client token (via {}, x-api-key, or Authorization: Bearer) for model discovery; ask the operator for one",
                auth.header()
            );
            return ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
                .into_response();
        };
        tracing::info!(client = %client, "inbound client authenticated for GET /v1/models");
    }
    let data: Vec<ModelEntry> = state
        .config
        .models
        .iter()
        .map(|model| ModelEntry {
            id: model.id.clone(),
            display_name: model.display_name.clone(),
        })
        .collect();
    tracing::info!(models = data.len(), "served GET /v1/models discovery");
    Json(ModelsResponse { data }).into_response()
}

#[cfg(test)]
mod tests {
    use axum::{extract::State, http::HeaderMap};
    use serde_json::json;

    use crate::{
        config::ModelConfig,
        server::{self, AppState},
    };

    use super::get;

    #[tokio::test]
    async fn returns_configured_models_with_optional_display_name() {
        let config = crate::config::Config {
            models: vec![
                ModelConfig {
                    id: "claude-opus-via-codex".to_string(),
                    display_name: Some("Opus (via Codex)".to_string()),
                },
                ModelConfig {
                    id: "anthropic-sonnet-via-codex".to_string(),
                    display_name: None,
                },
            ],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test".parse().unwrap());

        let response = get(State(state), headers).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            body,
            json!({
                "data": [
                    {"id": "claude-opus-via-codex", "display_name": "Opus (via Codex)"},
                    {"id": "anthropic-sonnet-via-codex"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn returns_empty_data_when_models_are_unconfigured() {
        let state =
            AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();

        let response = get(State(state), HeaderMap::new()).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(body, json!({"data": []}));
    }

    #[test]
    fn router_includes_get_models_route() {
        let (_router, _shared, _state) =
            server::build_router(crate::config::Config::default()).unwrap();
    }
}
