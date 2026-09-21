//! `/routes` response tests.
//!
//! Non-vacuity: drop the `routers` list from [`super::snapshot`] and every
//! router assertion here goes red; drop `skip_serializing_if` from
//! `RoutesResponse::routers` and `returns_configured_routes_with_optional_fields`
//! goes red on the byte-identical no-router response.

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
    let state = AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();

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

/// A custom `llm_classifier` reports its answer groups as `targets` and
/// the `judge` group as `judges` — never the other way round, because no
/// client turn is ever routed to a judge. The list is deduplicated:
/// upstream's own shape names every answer model in `any` as well as in
/// the group that selects it, and the same destination twice is not two
/// places a request can go.
#[tokio::test]
async fn a_custom_classifier_reports_its_groups_and_its_judge() {
    let router: crate::config::RouterConfig = toml::from_str(
        r#"
        type = "llm_classifier"
        mode = "custom"
        models = { judge = ["judge-alias"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
        default_target = "efficient"
        prompt = "Select exactly one target."
        response_schema = '{"type": "object", "properties": {"target": {"type": "string"}}}'
        policy = { type = "target_selector", selector = "/target" }
        "#,
    )
    .expect("the classifier table parses");
    let config = crate::config::Config {
        models: vec![
            ModelConfig {
                id: "claude-classified".to_string(),
                display_name: None,
                upstream_model: None,
                router: Some(router),
                stage_router: None,
                subagents: None,
            },
            // A judge may only resolve to a credential-injecting route.
            ModelConfig {
                id: "judge-alias".to_string(),
                display_name: None,
                upstream_model: Some(std::collections::BTreeMap::from([(
                    "codex".to_string(),
                    "gpt-5.2".to_string(),
                )])),
                router: None,
                stage_router: None,
                subagents: None,
            },
        ],
        ..crate::config::Config::default()
    };
    let state = AppState::new(config, reqwest::Client::new()).unwrap();

    let body = serde_json::to_value(get(State(state)).await.0).unwrap();

    assert_eq!(
        body["routers"][0],
        json!({
            "model": "claude-classified",
            "algorithm": "llm_classifier",
            "targets": ["claude-sonnet-4-6", "claude-opus-4-8"],
            "judges": ["judge-alias"]
        }),
        "the three stage keys must stay absent for an algorithm with no tier pair"
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
                classify_trigger: Default::default(),
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
