use axum::{extract::State, http::HeaderMap, Json};
use serde::Serialize;

use crate::server::AppState;

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

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Json<ModelsResponse> {
    let _credential = discovery_credential(&headers);
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
    Json(ModelsResponse { data })
}

fn discovery_credential(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .or_else(|| headers.get("x-api-key"))
        .and_then(|value| value.to_str().ok())
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
        let state = AppState {
            config,
            http_client: reqwest::Client::new(),
            inbound_auth: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test".parse().unwrap());

        let response = get(State(state), headers).await;
        let body = serde_json::to_value(response.0).unwrap();

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
        let state = AppState {
            config: crate::config::Config::default(),
            http_client: reqwest::Client::new(),
            inbound_auth: None,
        };

        let response = get(State(state), HeaderMap::new()).await;
        let body = serde_json::to_value(response.0).unwrap();

        assert_eq!(body, json!({"data": []}));
    }

    #[test]
    fn router_includes_get_models_route() {
        let _router = server::build_router(crate::config::Config::default()).unwrap();
    }
}
