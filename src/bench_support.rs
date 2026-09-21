//! Benchmark-only entry points into the stage router (issue #554).
//!
//! `benches/` compile as separate crates against the library, so they reach
//! only `pub` items — and the whole stage-router path is `pub(crate)`:
//! [`crate::routing::resolve_request_chain_value`], `StageContext`,
//! `StageRouterStore`, and `signals::extract`. The two public routing entry
//! points ([`crate::routing::resolve`] and [`crate::routing::resolve_model`])
//! both pass `stage: None`, so no benchmark could reach a live routing decision
//! at all. That is why the planned `stage_router_resolve` benchmark never
//! shipped.
//!
//! This module is a **facade, not a widening**. It exposes benchmark-shaped
//! functions rather than the private types behind them, so `StageContext`'s
//! `Cell` write/read protocol, `PendingPin`, and `StageDecision` stay
//! crate-private in every build — including this one. It is gated on
//! `--features bench`, so a normal build's public API is byte-for-byte what it
//! was before.
//!
//! Nothing here may add work the production path does not do: a benchmark that
//! measures its own harness measures nothing. Each function below is the
//! shortest path from a caller's inputs to the crate-private call it exists to
//! expose.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use serde_json::Value;

pub use switchyard_libsy::ToolSignals;

/// The `messages` walk the driven `prefill_router` lane pays per request, for
/// `benches/stage_router.rs`. Gated with the router it belongs to: without the
/// feature there is no lane to measure and no `Message` type to name.
#[cfg(feature = "prefill-router")]
pub use crate::routing::prefill::messages_from_body;

use crate::config::{Config, StageRouterConfig, ToolSemanticsConfig};
use crate::error::ShuntError;
use crate::routing::context::RouterContext;
use crate::routing::stage::store::{
    MAX_TRACKED_CHILD_PINS as CHILD_CAP, MAX_TRACKED_SESSIONS as STORE_CAP,
};
use crate::routing::stage::{self, signals, StageContext, StageRouterStore};
use crate::routing::{self, Route};

/// The parent-session cap `StageRouterStore` evicts against — the point past
/// which eviction stops being O(1) (issue #552).
pub const MAX_TRACKED_SESSIONS: usize = STORE_CAP;

/// The separate cap for delegated-turn pins, evicted only against each other
/// (ADR-0005 §5).
pub const MAX_TRACKED_CHILD_PINS: usize = CHILD_CAP;

/// Parse a request body exactly as the proxy does.
///
/// `src/proxy/failover.rs` does not call `serde_json::from_slice`: it calls
/// [`crate::request::RequestBody::parse`], whose visitor rejects duplicate
/// top-level keys so the gateway and the upstream cannot read one request
/// differently. That check is real work, and benchmarking the plain parser
/// instead would understate the cost the request has already paid before
/// routing — which is the denominator every routed number here is read against.
///
/// Returns the parsed tree so the parse cannot be optimised away.
pub fn parse_request_body(raw: Vec<u8>) -> Result<Arc<Value>, serde_json::Error> {
    crate::request::RequestBody::parse(raw).map(|body| body.json_arc())
}

/// Extract tool-activity signals from a request's `messages` array.
///
/// The subject of issue #553: pass 2 walks the whole history on every turn, so
/// a session's total extraction cost is quadratic in its turn count even though
/// each individual call is linear.
pub fn extract_signals(messages: &Value, recent_turn_window: usize) -> Option<ToolSignals> {
    // The default (empty) semantics table, which is what an entry without
    // `[models.router.tool_semantics]` passes: the benchmark measures the walk,
    // not an operator's lookup list.
    signals::extract(
        messages,
        recent_turn_window,
        &ToolSemanticsConfig::default(),
    )
}

/// Per-session tier pins, as they live on `AppState`.
#[derive(Default)]
pub struct StageStore(StageRouterStore);

impl StageStore {
    pub fn new() -> Self {
        Self(StageRouterStore::new())
    }

    /// Drive one turn's hysteresis with no conversation to score: `apply`
    /// followed by the `commit` that admission triggers.
    ///
    /// Scoring is skipped deliberately — the estimate is the picker default,
    /// which [`stage::decide`] reaches without reading any `messages` — so what
    /// this measures is the mutex-held get/resolve/insert/evict of issue #552
    /// and nothing else. Use [`resolve_chain`] for the whole request path.
    ///
    /// `agent_id` keys the turn as a delegated child of `session_id`, the way
    /// a `Task` child's `x-claude-code-agent-id` does; `None` is the parent.
    pub fn turn(
        &self,
        model: &str,
        session_id: &str,
        agent_id: Option<&str>,
        router: &StageRouterConfig,
        now: Instant,
    ) -> bool {
        let hints = RouterContext {
            session_id: Some(session_id),
            agent_id,
            ..RouterContext::default()
        };
        let applied = self.0.apply(
            model,
            &hints,
            router,
            |compacted| stage::decide(router, None, compacted),
            false,
            now,
        );
        applied
            .pin
            .is_some_and(|pin| self.0.commit(pin, now).is_some())
    }
}

/// Resolve one live request through the stage router, exactly as
/// `src/proxy/failover.rs` does: hand routing the inbound headers, and — only
/// for a router-backed id — read the request hints off them, score the
/// conversation, apply hysteresis, then commit the pin once the request is
/// admitted.
///
/// This is the only path that reaches `resolve_request_chain_value` with a
/// `StageContext`, and so the only one that measures what a router-backed
/// request actually pays.
pub fn resolve_chain(
    config: &Config,
    store: &StageStore,
    request: &Value,
    headers: &HeaderMap,
    read_only: bool,
    now: Instant,
) -> Result<Vec<Route>, ShuntError> {
    let stage = StageContext {
        store: &store.0,
        request,
        headers,
        read_only,
        now,
        pending: Cell::new(None),
        decided: Cell::new(None),
        consult: Cell::new(None),
        prefill: None,
    };
    let (routes, _model) = routing::resolve_request_chain_value(config, request, Some(&stage))?;
    // The commit `proxy::failover` performs once the request is admitted: take
    // the parked pin and write it. Spelled out here rather than hidden behind a
    // helper because production spells it out too — the driven lane may rewrite
    // the pin's tier between these two lines.
    if let Some(pin) = stage.pending.take() {
        store.0.commit(pin, now);
    }
    Ok(routes)
}

/// Every route admission must consider for one requested id (ADR-0005 §3).
///
/// The driven lane's added cost on the admission path, measured against the
/// routed arms above: a driven entry gates against this list instead of against
/// the chain its router picked, so the extra work is one `resolve_model_chain`
/// per target and judge. `dependency_envelope` is `pub(crate)`, which is why it
/// needs a facade at all.
pub fn dependency_envelope(config: &Config, model: &str) -> Vec<Route> {
    routing::envelope::dependency_envelope(config, model)
}

/// Facade tests.
///
/// These pin the facade to the production path it claims to expose, which is
/// the property a benchmark cannot check for itself: it compiles, prints a
/// number, and says nothing about whether the number is the one being claimed.
/// Every test here failed at some point during this module's review.
///
/// Non-vacuity: point [`parse_request_body`] at `serde_json::from_slice` and
/// `the_parser_rejects_a_duplicate_top_level_key` goes red — that swap is the
/// exact defect three review engines found. Make [`extract_signals`] return
/// `None` unconditionally and `signals_are_extracted_from_a_completed_call`
/// goes red. Drop the `stage_router` check in [`resolve_chain`]'s config and
/// `an_unrouted_id_resolves_without_the_router` and its routed twin report the
/// same `upstream_model`, collapsing the benchmark's control arm.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, RouteConfig, StageRouterPicker};
    use serde_json::json;

    fn router() -> StageRouterConfig {
        StageRouterConfig {
            capable_target: "capable-model".to_string(),
            efficient_target: "efficient-model".to_string(),
            picker: StageRouterPicker::EfficientFirst,
            confidence_threshold: 0.5,
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
        }
    }

    fn config(with_router: bool) -> Config {
        let route = |model: &str| RouteConfig {
            model: model.to_string(),
            provider: "anthropic".to_string(),
            upstream_model: None,
            effort: None,
            service_tier: None,
        };
        Config {
            models: vec![ModelConfig {
                id: "router-model".to_string(),
                display_name: None,
                upstream_model: None,
                router: with_router
                    .then(router)
                    .map(crate::config::RouterConfig::StageRouter),
                stage_router: None,
                subagents: None,
            }],
            routes: vec![
                route("router-model"),
                route("efficient-model"),
                route("capable-model"),
            ],
            ..Config::default()
        }
    }

    fn session_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-session-id", "session".parse().unwrap());
        headers
    }

    fn request() -> Value {
        json!({
            "model": "router-model",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": false}
                ]}
            ]
        })
    }

    /// The parse this module exposes is the proxy's, not `serde_json`'s. A
    /// duplicate top-level key is the one input that tells them apart: stock
    /// `Value` deserialization takes the last value, and `RequestBody::parse`
    /// refuses the request outright.
    #[test]
    fn the_parser_rejects_a_duplicate_top_level_key() {
        let duplicated = br#"{"model":"first","model":"second"}"#.to_vec();
        assert!(
            parse_request_body(duplicated).is_err(),
            "a plain serde_json parse would accept this and keep the last value"
        );

        let single = br#"{"model":"only"}"#.to_vec();
        let parsed = parse_request_body(single).expect("a well-formed body parses");
        assert_eq!(parsed.get("model").and_then(Value::as_str), Some("only"));
    }

    #[test]
    fn signals_are_extracted_from_a_completed_call() {
        let request = request();
        let messages = request.get("messages").expect("the fixture has messages");
        assert!(
            extract_signals(messages, 3).is_some(),
            "one tool_use joined to its tool_result is a completed call"
        );
        assert!(
            extract_signals(&json!([]), 3).is_none(),
            "an empty conversation carries no stage to estimate"
        );
    }

    /// The routed half of the benchmark's control pair: the same id, resolved
    /// through a config that does carry the router, reaches a tier target.
    ///
    /// The tier shows up in `upstream_model`, not in `model` — `model` is
    /// re-stamped to the id the client asked for, so a routed turn still reports
    /// itself as the router (issue #172, `docs/stage-router.md` §5). Asserting
    /// both pins the selection and the re-stamp at once.
    #[test]
    fn a_routed_id_resolves_to_a_tier_target() {
        let store = StageStore::new();
        let routes = resolve_chain(
            &config(true),
            &store,
            &request(),
            &session_headers(),
            false,
            Instant::now(),
        )
        .expect("the fixture names a configured model");
        assert_eq!(
            routes[0].upstream_model, "efficient-model",
            "the picker default sends the turn to the efficient tier"
        );
        assert_eq!(
            routes[0].model, "router-model",
            "the client-facing id stays the one that was requested"
        );
    }

    /// The unrouted half. Same id, same body, same store — only the
    /// `[models.router]` table is absent, and the id then resolves as
    /// itself. Without this the benchmark's flat `resolve_chain_unrouted` row
    /// would prove nothing about the router.
    #[test]
    fn an_unrouted_id_resolves_without_the_router() {
        let store = StageStore::new();
        let routes = resolve_chain(
            &config(false),
            &store,
            &request(),
            &session_headers(),
            false,
            Instant::now(),
        )
        .expect("the fixture names a configured model");
        assert_eq!(
            routes[0].upstream_model, "router-model",
            "with no router the id is its own upstream, not a tier target"
        );
    }

    /// `turn` reports whether its own write displaced a different tier. A first
    /// turn displaces nothing, so the store must not report a flip for it.
    #[test]
    fn a_first_turn_displaces_no_tier() {
        let store = StageStore::new();
        let now = Instant::now();
        assert!(!store.turn("router-model", "session", None, &router(), now));
        assert!(
            !store.turn("router-model", "session", None, &router(), now),
            "a second turn landing on the pinned tier is not a flip either"
        );
    }

    /// The facade's `agent_id` reaches the store as a child key: a child turn
    /// must not land on — or count as — the parent's pin.
    #[test]
    fn a_child_turn_pins_apart_from_its_parent() {
        let store = StageStore::new();
        let now = Instant::now();
        store.turn("router-model", "session", None, &router(), now);
        store.turn("router-model", "session", Some("child-1"), &router(), now);
        assert_eq!(store.0.len(), 2, "parent and child hold one pin each");
    }

    #[test]
    fn the_caps_match_the_store() {
        assert_eq!(MAX_TRACKED_SESSIONS, 4096);
        assert_eq!(MAX_TRACKED_CHILD_PINS, 4096);
    }
}
