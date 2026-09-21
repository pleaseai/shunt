use axum::{extract::State, Json};
use serde::Serialize;

use crate::server::AppState;

#[derive(Debug, Serialize)]
pub struct RoutesResponse {
    pub data: Vec<RouteEntry>,
    /// The `[[models]]` entries carrying a `[models.router]` table.
    ///
    /// Omitted entirely when none is configured, so a deployment without a
    /// router serves the byte-identical response it served before routers
    /// existed — the two assertions in this module's tests pin exactly that.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routers: Vec<RouterEntry>,
}

/// One configured router, as `/routes` reports it.
///
/// The tunables (`confidence_threshold`, dwell, TTL, weights) are deliberately
/// absent: this endpoint answers "where can a request go", and a client that
/// needs the calibration reads the config. What it does report is every id the
/// router chooses between, because nothing else in this response is required
/// to name them: a router's targets need not have `[[routes]]` entries of
/// their own, though one that does still appears in `data` as itself.
///
/// The three stage fields stay `Option` with `skip_serializing_if` so a
/// `random` or `noop` entry does not report a tier pair it does not have —
/// a reader that finds `capable_target` present knows it is looking at a
/// two-tier algorithm.
#[derive(Debug, Serialize)]
pub struct RouterEntry {
    /// The advertised id clients request.
    pub model: String,
    /// The `[models.router] type` that decides this id.
    pub algorithm: String,
    /// Every target the router can name, in declared order. `[capable,
    /// efficient]` for a stage or auto entry, the configured list for a
    /// `random` one, and empty for `noop`, which answers as itself.
    pub targets: Vec<String>,
    /// Every id this router *consults* and never serves — today the
    /// `[models.router.classifier]` judge (ADR-0005 §7). Separate from
    /// `targets` because a reader resolving "where can a request go" must not
    /// find a judge there: no client turn is ever routed to one. Omitted
    /// entirely when there is none, so an entry with no judge answers exactly
    /// as it did before the key existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub judges: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capable_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efficient_target: Option<String>,
    /// The tier `picker` falls back to when no signal decides a turn. A
    /// body-less caller resolving this id gets it for that reason, which is
    /// what makes it worth naming. It is not "where a session starts": a first
    /// turn that already carries decisive tool-result history is scored like
    /// any other and can land on the opposite tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tier: Option<&'static str>,
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
            let router = model.router.as_ref()?;
            let stage = router.stage();
            Some(RouterEntry {
                model: model.id.clone(),
                algorithm: router.algorithm().to_string(),
                targets: router.targets().into_iter().map(str::to_string).collect(),
                judges: router.judges().into_iter().map(str::to_string).collect(),
                capable_target: stage.map(|stage| stage.capable_target.clone()),
                efficient_target: stage.map(|stage| stage.efficient_target.clone()),
                default_tier: stage.map(|stage| match stage.picker {
                    crate::config::StageRouterPicker::EfficientFirst => "efficient",
                    crate::config::StageRouterPicker::CapableFirst => "capable",
                }),
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
            router: Some(crate::config::RouterConfig::StageRouter(
                crate::config::StageRouterConfig {
                    capable_target: "claude-opus-4-8".to_string(),
                    efficient_target: "claude-sonnet-4-6".to_string(),
                    picker,
                    confidence_threshold: crate::config::DEFAULT_CONFIDENCE_THRESHOLD,
                    recent_turn_window: 3,
                    min_dwell_turns: 3,
                    deescalate_threshold: None,
                    session_ttl_seconds: 3600,
                    capable_hold_turns: 0,
                    tool_semantics: Default::default(),
                    handoff_notes: None,
                    classifier: None,
                    judge_timeout_ms: crate::config::DEFAULT_JUDGE_TIMEOUT_MS,
                    judge_max_response_bytes: crate::config::DEFAULT_JUDGE_MAX_RESPONSE_BYTES,
                    gated_max_bytes: crate::config::DEFAULT_GATED_MAX_BYTES,
                    gated_idle_ms: crate::config::DEFAULT_GATED_IDLE_MS,
                    gated_max_duration_ms: crate::config::DEFAULT_GATED_MAX_DURATION_MS,
                    max_judge_calls: crate::config::DEFAULT_MAX_JUDGE_CALLS,
                },
            )),
            stage_router: None,
            subagents: None,
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
                    router: None,
                    stage_router: None,
                    subagents: None,
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
                    "algorithm": "stage_router",
                    "targets": ["claude-opus-4-8", "claude-sonnet-4-6"],
                    "capable_target": "claude-opus-4-8",
                    "efficient_target": "claude-sonnet-4-6",
                    "default_tier": "efficient"
                }]
            }),
            "a `[[models]]` entry without a router must not appear in `routers`"
        );
    }

    /// A driven router names its judge too. `/routes` answers "where can a
    /// request for this id go", and a judge is a destination the gateway calls
    /// on its own credential — listing only the answer tiers would leave that
    /// call invisible. The test above is the other half: without a classifier
    /// the key is absent, not `[]`.
    #[tokio::test]
    async fn a_driven_router_reports_its_judge_beside_its_targets() {
        let mut model = stage_router_model(
            "claude-auto",
            crate::config::StageRouterPicker::EfficientFirst,
        );
        match model.router.as_mut().expect("the fixture carries a router") {
            crate::config::RouterConfig::StageRouter(stage) => {
                stage.classifier = Some(crate::config::StageClassifierConfig {
                    target: "judge-alias".to_string(),
                    base_threshold: 0.5,
                });
            }
            other => panic!("the fixture is a stage router, got {other:?}"),
        }
        let config = crate::config::Config {
            models: vec![
                model,
                // A judge may only resolve to a credential-injecting route, so
                // the entry has to name one or `validate` rejects the config
                // before `/routes` ever sees it.
                ModelConfig {
                    subagents: None,
                    id: "judge-alias".to_string(),
                    display_name: None,
                    upstream_model: Some(std::collections::BTreeMap::from([(
                        "codex".to_string(),
                        "gpt-5.2".to_string(),
                    )])),
                    router: None,
                    stage_router: None,
                },
            ],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let body = serde_json::to_value(get(State(state)).await.0).unwrap();

        assert_eq!(
            body["routers"][0],
            json!({
                "model": "claude-auto",
                "algorithm": "stage_router",
                "targets": ["claude-opus-4-8", "claude-sonnet-4-6"],
                "judges": ["judge-alias"],
                "capable_target": "claude-opus-4-8",
                "efficient_target": "claude-sonnet-4-6",
                "default_tier": "efficient"
            }),
            "the judge is listed separately from the answer targets"
        );
    }

    /// A `random` entry reports its whole target list and none of the stage
    /// fields, so a reader cannot mistake it for a two-tier algorithm.
    #[tokio::test]
    async fn a_random_router_reports_its_targets_without_a_tier_pair() {
        let config = crate::config::Config {
            models: vec![ModelConfig {
                id: "claude-canary".to_string(),
                display_name: None,
                upstream_model: None,
                router: Some(crate::config::RouterConfig::Random(
                    crate::config::RandomRouterConfig {
                        targets: vec!["claude-sonnet-4-6".to_string(), "gpt-5.6-terra".to_string()],
                        weights: Some(vec![9.0, 1.0]),
                        seed: None,
                        affinity: crate::config::RandomAffinity::Session,
                    },
                )),
                stage_router: None,
                subagents: None,
            }],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let body = serde_json::to_value(get(State(state)).await.0).unwrap();

        assert_eq!(
            body["routers"][0],
            json!({
                "model": "claude-canary",
                "algorithm": "random",
                "targets": ["claude-sonnet-4-6", "gpt-5.6-terra"]
            }),
            "the weights are calibration, not destinations, and the stage \
             fields must be absent rather than null"
        );
    }

    /// A `noop` entry names no destination at all, which is the one case where
    /// `targets` is legitimately empty.
    #[tokio::test]
    async fn a_noop_router_reports_an_empty_target_list() {
        let config = crate::config::Config {
            models: vec![ModelConfig {
                id: "claude-quiet".to_string(),
                display_name: None,
                upstream_model: None,
                router: Some(crate::config::RouterConfig::Noop {}),
                stage_router: None,
                subagents: None,
            }],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let body = serde_json::to_value(get(State(state)).await.0).unwrap();

        assert_eq!(
            body["routers"][0],
            json!({"model": "claude-quiet", "algorithm": "noop", "targets": []})
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
