//! Resolution-level tests for the overlay: what `resolve_chain` does with a
//! `[models.subagents]` table on a live request, as opposed to what
//! [`super::select`] answers in isolation.
//!
//! Non-vacuity: delete the overlay arm at the top of
//! `crate::routing::resolve_chain` and
//! `a_delegated_turn_takes_the_overlay_without_touching_the_store` goes red
//! on the upstream model; make that arm run without checking `stage` and
//! `a_parent_turn_on_an_overlaid_entry_still_takes_the_router` goes red on
//! the body-less assertion.

use std::{collections::BTreeMap, time::Instant};

use axum::http::HeaderMap;
use serde_json::json;

use crate::{
    config::{
        Config, ModelConfig, RouterConfig, StageRouterConfig, StageRouterPicker, SubagentsConfig,
    },
    routing::{
        resolve_model, resolve_request_chain_value,
        stage::{StageContext, StageRouterStore},
    },
};

const ROUTER_ID: &str = "claude-auto";

fn router() -> StageRouterConfig {
    StageRouterConfig {
        capable_target: "capable-alias".to_string(),
        efficient_target: "efficient-alias".to_string(),
        picker: StageRouterPicker::EfficientFirst,
        confidence_threshold: crate::config::DEFAULT_CONFIDENCE_THRESHOLD,
        recent_turn_window: 3,
        min_dwell_turns: 3,
        deescalate_threshold: None,
        session_ttl_seconds: 3600,
        capable_hold_turns: 0,
        tool_semantics: Default::default(),
        handoff_notes: None,
        // The overlay tests live on the pure lane: no judge is configured, so
        // the overlay is decided without a model call either way.
        classifier: None,
        judge_timeout_ms: crate::config::DEFAULT_JUDGE_TIMEOUT_MS,
        judge_max_response_bytes: crate::config::DEFAULT_JUDGE_MAX_RESPONSE_BYTES,
        gated_max_bytes: crate::config::DEFAULT_GATED_MAX_BYTES,
        gated_idle_ms: crate::config::DEFAULT_GATED_IDLE_MS,
        gated_max_duration_ms: crate::config::DEFAULT_GATED_MAX_DURATION_MS,
        max_judge_calls: crate::config::DEFAULT_MAX_JUDGE_CALLS,
    }
}

fn mapped(id: &str, upstream_model: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            "codex".to_string(),
            upstream_model.to_string(),
        )])),
        router: None,
        subagents: None,
        stage_router: None,
    }
}

/// The §11 overlay: a `by_type` hit for `Explore`, `target` for the rest.
fn overlay() -> SubagentsConfig {
    toml::from_str(
        r#"
        type = "passthrough"
        target = "efficient-alias"
        by_type = { Explore = "child-alias" }
        "#,
    )
    .expect("the overlay parses")
}

/// A router-backed entry carrying the overlay, plus its three targets.
fn config() -> Config {
    Config {
        models: vec![
            ModelConfig {
                id: ROUTER_ID.to_string(),
                display_name: None,
                upstream_model: None,
                router: Some(RouterConfig::StageRouter(router())),
                subagents: Some(overlay()),
                stage_router: None,
            },
            mapped("capable-alias", "upstream-capable"),
            mapped("efficient-alias", "upstream-efficient"),
            mapped("child-alias", "upstream-child"),
        ],
        ..Config::default()
    }
}

/// Two failed investigative turns — enough for the scorer to escalate the
/// parent, which is what makes "never scored" observable for the child.
fn erroring_request() -> serde_json::Value {
    json!({
        "model": ROUTER_ID,
        "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "Grep"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "b", "is_error": true}]},
        ]
    })
}

fn session_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-claude-code-session-id", "session-a".parse().unwrap());
    headers
}

fn child_headers(class: &str, agent_type: &str) -> HeaderMap {
    let mut headers = session_headers();
    headers.insert(
        "x-claude-code-agent-id",
        "a7a11c2e22e29e67a".parse().unwrap(),
    );
    headers.insert("x-claude-code-request-class", class.parse().unwrap());
    headers.insert("x-claude-code-agent-type", agent_type.parse().unwrap());
    headers
}

fn context<'a>(
    store: &'a StageRouterStore,
    request: &'a serde_json::Value,
    headers: &'a HeaderMap,
) -> StageContext<'a> {
    StageContext {
        store,
        request,
        headers,
        read_only: false,
        now: Instant::now(),
        pending: std::cell::Cell::new(None),
        decided: std::cell::Cell::new(None),
        consult: std::cell::Cell::new(None),
        prefill: None,
    }
}

/// A delegated turn on an entry that carries both a router and an overlay
/// takes the overlay: the erroring transcript that would escalate the parent
/// is never scored, no pin is parked, and the outcome names the overlay rather
/// than the router — the "no store access" clause.
#[test]
fn a_delegated_turn_takes_the_overlay_without_touching_the_store() {
    let config = config();
    let store = StageRouterStore::new();
    let request = erroring_request();
    let headers = child_headers("subagent", "Explore");
    let context = context(&store, &request, &headers);

    let (routes, requested) = resolve_request_chain_value(&config, &request, Some(&context))
        .expect("an overlaid id resolves");

    assert_eq!(requested, ROUTER_ID);
    assert_eq!(
        routes[0].upstream_model, "upstream-child",
        "the by_type target"
    );
    assert_eq!(
        routes[0].model, ROUTER_ID,
        "the client is told the id it asked for"
    );
    assert_eq!(
        routes[0].provider, "codex",
        "resolved through the ordinary ladder"
    );
    assert!(
        context.pending.take().is_none(),
        "the overlay parks no pin: the store was never consulted"
    );
    let outcome = context.decided.take().expect("an outcome is stamped");
    assert_eq!(outcome.algorithm, "subagents");
    assert_eq!(outcome.target, "child-alias");
    assert_eq!(outcome.source.as_label(), "subagent_type");
}

/// The parent's own turn on the same entry sees no overlay: the router scores
/// the transcript and pins as it did before the table existed. And the
/// body-less surfaces report the parent's destination — the overlay is
/// invisible to discovery, `/routes`, and `shunt check`.
#[test]
fn a_parent_turn_on_an_overlaid_entry_still_takes_the_router() {
    let config = config();
    let store = StageRouterStore::new();
    let request = erroring_request();
    let headers = session_headers();
    let context = context(&store, &request, &headers);

    let (routes, _) = resolve_request_chain_value(&config, &request, Some(&context))
        .expect("a router-backed id resolves");

    assert_eq!(routes[0].upstream_model, "upstream-capable");
    assert!(
        context.pending.take().is_some(),
        "the router pins the parent"
    );
    assert_eq!(context.decided.take().unwrap().algorithm, "stage_router");
    assert_eq!(
        resolve_model(&config, ROUTER_ID).upstream_model,
        "upstream-efficient"
    );
}

/// The overlay on a fixed entry — upstream's "passthrough with subagents". A
/// `main` turn carrying an agent id is main traffic (the class is
/// authoritative), and harness maintenance is never diverted; both land on
/// the entry's own map with no outcome stamped.
#[test]
fn an_overlaid_fixed_entry_diverts_only_delegated_work() {
    let mut config = config();
    config.models[0] = mapped(ROUTER_ID, "upstream-parent");
    config.models[0].subagents = Some(overlay());
    let store = StageRouterStore::new();
    let request = erroring_request();
    let resolve = |headers: &HeaderMap| {
        let context = context(&store, &request, headers);
        let (routes, _) = resolve_request_chain_value(&config, &request, Some(&context))
            .expect("the id resolves");
        (routes[0].upstream_model.clone(), context.decided.take())
    };

    let (upstream, outcome) = resolve(&child_headers("workflow", "general-purpose"));
    assert_eq!(upstream, "upstream-efficient", "no by_type entry: `target`");
    assert_eq!(outcome.expect("stamped").source.as_label(), "subagent");

    let (upstream, outcome) = resolve(&child_headers("main", "Explore"));
    assert_eq!(
        upstream, "upstream-parent",
        "main with an agent id is main traffic"
    );
    assert!(
        outcome.is_none(),
        "a fixed entry stamps no outcome for the parent"
    );

    let (upstream, outcome) = resolve(&child_headers("auxiliary", "Explore"));
    assert_eq!(
        upstream, "upstream-parent",
        "harness maintenance is never diverted"
    );
    assert!(outcome.is_none());
}
