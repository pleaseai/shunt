use axum::{extract::State, Json};
use serde::Serialize;

use crate::server::AppState;

#[derive(Debug, Serialize)]
pub struct RoutesResponse {
    pub data: Vec<RouteEntry>,
    /// The `[[models]]` entries carrying a `[models.stage_router]` table.
    ///
    /// Omitted entirely when none is configured, so a deployment without a
    /// router serves the byte-identical response it served before routers
    /// existed — the two assertions in this module's tests pin exactly that.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routers: Vec<RouterEntry>,
}

/// One configured stage router, as `/routes` reports it.
///
/// The tunables (`confidence_threshold`, dwell, TTL) are deliberately absent:
/// this endpoint answers "where can a request go", and a client that needs the
/// calibration reads the config. What it does report is the pair of ids the
/// router chooses between, because nothing else in this response is required
/// to name them: a router's targets need not have `[[routes]]` entries of
/// their own, though one that does still appears in `data` as itself.
#[derive(Debug, Serialize)]
pub struct RouterEntry {
    /// The advertised id clients request.
    pub model: String,
    pub capable_target: String,
    pub efficient_target: String,
    /// The tier `picker` falls back to when no signal decides a turn. A
    /// body-less caller resolving this id gets it for that reason, which is
    /// what makes it worth naming. It is not "where a session starts": a first
    /// turn that already carries decisive tool-result history is scored like
    /// any other and can land on the opposite tier.
    pub default_tier: &'static str,
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
    let response = snapshot(&state);
    tracing::info!(
        routes = response.data.len(),
        routers = response.routers.len(),
        "served GET /routes discovery"
    );
    Json(response)
}

/// Build the `/routes` payload from an already-refreshed state.
///
/// Shared with the admin surface's `GET /admin/api/routes`, which serves this
/// same view behind admin authentication. The duplication is deliberate rather
/// than a redirect: this route is discovery, deliberately unauthenticated so any
/// client can resolve a model against it, while everything under `/admin` is
/// gated by the admin credential. Pointing one at the other would tie the two
/// namespaces' authentication together -- either widening what the admin
/// credential gates or putting an auth challenge in front of discovery. Both
/// callers share this function so the table an operator reads cannot drift from
/// the one a client resolves against.
pub(crate) fn snapshot(state: &AppState) -> RoutesResponse {
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
    let routers: Vec<RouterEntry> = state
        .config
        .models
        .iter()
        .filter_map(|model| {
            let router = model.stage_router.as_ref()?;
            Some(RouterEntry {
                model: model.id.clone(),
                capable_target: router.capable_target.clone(),
                efficient_target: router.efficient_target.clone(),
                default_tier: match router.picker {
                    crate::config::StageRouterPicker::EfficientFirst => "efficient",
                    crate::config::StageRouterPicker::CapableFirst => "capable",
                },
            })
        })
        .collect();
    RoutesResponse { data, routers }
}

#[cfg(test)]
mod tests {
    use axum::extract::State;
    use serde_json::json;

    use crate::{
        config::{ModelConfig, RouteConfig},
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

    fn stage_router_model(id: &str, picker: crate::config::StageRouterPicker) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            display_name: Some("Auto".to_string()),
            upstream_model: None,
            stage_router: Some(crate::config::StageRouterConfig {
                capable_target: "claude-opus-4-8".to_string(),
                efficient_target: "claude-sonnet-4-6".to_string(),
                picker,
                confidence_threshold: crate::config::DEFAULT_CONFIDENCE_THRESHOLD,
                recent_turn_window: 3,
                min_dwell_turns: 3,
                deescalate_threshold: None,
                session_ttl_seconds: 3600,
            }),
        }
    }

    /// A configured router is reported with both destinations, because a
    /// router's targets need not have `[[routes]]` entries of their own — so
    /// `data` alone would not name them.
    #[tokio::test]
    async fn returns_configured_stage_routers() {
        let config = crate::config::Config {
            models: vec![
                stage_router_model(
                    "claude-auto",
                    crate::config::StageRouterPicker::EfficientFirst,
                ),
                ModelConfig {
                    id: "claude-plain".to_string(),
                    display_name: None,
                    upstream_model: None,
                    stage_router: None,
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
                "data": [],
                "routers": [{
                    "model": "claude-auto",
                    "capable_target": "claude-opus-4-8",
                    "efficient_target": "claude-sonnet-4-6",
                    "default_tier": "efficient"
                }]
            }),
            "a `[[models]]` entry without a router must not appear in `routers`"
        );
    }

    /// `default_tier` tracks `picker`, so it cannot be read as a constant.
    #[tokio::test]
    async fn default_tier_follows_the_configured_picker() {
        let config = crate::config::Config {
            models: vec![stage_router_model(
                "claude-auto",
                crate::config::StageRouterPicker::CapableFirst,
            )],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let body = serde_json::to_value(get(State(state)).await.0).unwrap();
        assert_eq!(body["routers"][0]["default_tier"], "capable");
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
