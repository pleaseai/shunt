use axum::{extract::State, Json};
use serde::Serialize;

use crate::server::AppState;

#[derive(Debug, Serialize)]
pub struct RoutesResponse {
    pub data: Vec<RouteEntry>,
}

#[derive(Debug, Serialize)]
pub struct RouteEntry {
    pub model: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Shunt-native endpoint exposing the configured `[[routes]]` table verbatim,
/// including any `claude-`/`anthropic-`-prefixed discovery aliases. Distinct
/// from `/v1/models`, which serves the narrower Anthropic-protocol
/// model-discovery response (only `id`/`display_name` from `[[models]]`).
pub async fn get(State(state): State<AppState>) -> Json<RoutesResponse> {
    // Snapshot the live config so this response reflects the latest reload.
    let state = state.refreshed();
    let data: Vec<RouteEntry> = state
        .config
        .routes
        .iter()
        .map(|route| RouteEntry {
            model: route.model.clone(),
            provider: route.provider.clone(),
            upstream_model: route.upstream_model.clone(),
            effort: route.effort.clone(),
        })
        .collect();
    tracing::info!(routes = data.len(), "served GET /routes discovery");
    Json(RoutesResponse { data })
}

#[cfg(test)]
mod tests {
    use axum::extract::State;
    use serde_json::json;

    use crate::{
        config::RouteConfig,
        server::{self, AppState},
    };

    use super::get;

    #[tokio::test]
    async fn returns_configured_routes_with_optional_fields() {
        let config = crate::config::Config {
            routes: vec![
                RouteConfig {
                    model: "gpt-5.6-luna".to_string(),
                    provider: "codex".to_string(),
                    upstream_model: Some("gpt-5.6-luna".to_string()),
                    effort: Some("high".to_string()),
                },
                RouteConfig {
                    model: "gpt-5.2".to_string(),
                    provider: "openai".to_string(),
                    upstream_model: None,
                    effort: None,
                },
            ],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let response = get(State(state)).await;
        let body = serde_json::to_value(response.0).unwrap();

        assert_eq!(
            body,
            json!({
                "data": [
                    {"model": "gpt-5.6-luna", "provider": "codex", "upstream_model": "gpt-5.6-luna", "effort": "high"},
                    {"model": "gpt-5.2", "provider": "openai"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn returns_empty_data_when_routes_are_unconfigured() {
        let state =
            AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();

        let response = get(State(state)).await;
        let body = serde_json::to_value(response.0).unwrap();

        assert_eq!(body, json!({"data": []}));
    }

    #[test]
    fn router_includes_get_routes_route() {
        let (_router, _shared, _state) =
            server::build_router(crate::config::Config::default()).unwrap();
    }
}
