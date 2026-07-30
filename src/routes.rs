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
    /// Verbatim from `RouteConfig.service_tier` (the normalized config
    /// value, not the resolved-with-provider-fallback one from
    /// `routing::route_for`). An explicit `"default"` override now serializes
    /// as `"service_tier": "default"` rather than being omitted like an
    /// unset route -- config validation preserves the sentinel instead of
    /// collapsing it to `None` (see config::normalize_service_tier_value), so
    /// this discovery response can distinguish "explicitly disabled" from
    /// "never configured". That is intentional and informative, not a leak:
    /// the sentinel is still stripped before any upstream request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
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
            service_tier: route.service_tier.clone(),
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
                    service_tier: Some("priority".to_string()),
                },
                RouteConfig {
                    model: "gpt-5.2".to_string(),
                    provider: "openai".to_string(),
                    upstream_model: None,
                    effort: None,
                    service_tier: None,
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
                    {"model": "gpt-5.6-luna", "provider": "codex", "upstream_model": "gpt-5.6-luna", "effort": "high", "service_tier": "priority"},
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

    #[tokio::test]
    async fn explicit_default_service_tier_is_distinguishable_from_unset() {
        // Regression test for issue #301: an explicit route-level
        // service_tier = "default" override must serialize distinctly from a
        // route that never configured service_tier at all, so operators can
        // tell "explicitly disabled" from "never configured" via discovery.
        let config = crate::config::Config {
            routes: vec![
                RouteConfig {
                    model: "gpt-5.6-sol".to_string(),
                    provider: "codex".to_string(),
                    upstream_model: None,
                    effort: None,
                    service_tier: Some("default".to_string()),
                },
                RouteConfig {
                    model: "gpt-5.2".to_string(),
                    provider: "openai".to_string(),
                    upstream_model: None,
                    effort: None,
                    service_tier: None,
                },
            ],
            ..crate::config::Config::default()
        };
        let config = config.validate().unwrap();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let response = get(State(state)).await;
        let body = serde_json::to_value(response.0).unwrap();

        assert_eq!(
            body,
            json!({
                "data": [
                    {"model": "gpt-5.6-sol", "provider": "codex", "service_tier": "default"},
                    {"model": "gpt-5.2", "provider": "openai"}
                ]
            })
        );
    }
}
