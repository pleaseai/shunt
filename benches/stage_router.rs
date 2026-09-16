//! Stage-router measurements (issue #554).
//!
//! The whole stage-router path is crate-private and neither public routing
//! entry point reaches it (both pass `stage: None`), so this file drives it
//! through `shunt::bench_support` and is gated on the same `bench` feature that
//! module is. Without the feature it builds and reports why it measured
//! nothing, rather than failing a plain `cargo bench`.
//!
//! What each group settles:
//!
//! * `extract_signals` — issue #553. Parameterized on **assistant turn count**,
//!   not body size: the claim is that pass 2 walks the whole `messages` array
//!   every turn, so per-request cost should grow linearly in turn count while a
//!   session's cumulative cost grows quadratically. A flat curve here would
//!   refute it.
//! * `store_turn_*` — issue #552. `existing_session` never grows the map and so
//!   never evicts; `new_session_at_capacity` is the saturated case the issue
//!   describes, where each previously unseen id triggers a full `retain` scan
//!   plus a `min_by_key` scan. Both run against a store pre-filled to
//!   `MAX_TRACKED_SESSIONS`, so the difference between them is the eviction
//!   cost and not the map size.
//! * `resolve_chain_*` — the whole router-backed request path, and the control
//!   it has to be read against: `resolve_chain_unrouted` sends the identical
//!   body to a model id with no `[models.stage_router]` table. That pair is the
//!   "non-router traffic pays only one `Option::is_none()`" claim, which is the
//!   assertion most worth protecting from regression.

fn main() {
    #[cfg(feature = "bench")]
    divan::main();
    #[cfg(not(feature = "bench"))]
    eprintln!(
        "stage-router benchmarks need the crate-private facade: \
         cargo bench --features bench --bench stage_router"
    );
}

#[cfg(feature = "bench")]
mod bench {
    use std::time::Instant;

    use serde_json::{json, Value};
    use shunt::{
        bench_support::{self, StageStore, MAX_TRACKED_SESSIONS},
        config::{Config, ModelConfig, RouteConfig, StageRouterConfig, StageRouterPicker},
    };

    /// Assistant turn counts. The top of the range is a long Claude Code
    /// session, not a synthetic extreme — the window only ever reads the last
    /// `recent_turn_window` of them, which is the asymmetry #553 is about.
    const TURN_COUNTS: [usize; 4] = [10, 50, 200, 800];

    const ROUTER_MODEL: &str = "claude-auto";
    const PLAIN_MODEL: &str = "claude-sonnet-4-6";

    fn router() -> StageRouterConfig {
        StageRouterConfig {
            capable_target: "claude-opus-4-8".to_string(),
            efficient_target: PLAIN_MODEL.to_string(),
            picker: StageRouterPicker::EfficientFirst,
            confidence_threshold: 0.5,
            recent_turn_window: 3,
            min_dwell_turns: 3,
            deescalate_threshold: None,
            session_ttl_seconds: 3600,
        }
    }

    /// Both ids resolve through the same `[[routes]]` entry, so the only
    /// difference between the routed and unrouted benchmarks is the
    /// `[models.stage_router]` table.
    fn config() -> Config {
        Config {
            models: vec![
                ModelConfig {
                    id: ROUTER_MODEL.to_string(),
                    display_name: Some("Auto (stage router)".to_string()),
                    upstream_model: None,
                    stage_router: Some(router()),
                },
                ModelConfig {
                    id: PLAIN_MODEL.to_string(),
                    display_name: None,
                    upstream_model: None,
                    stage_router: None,
                },
            ],
            routes: vec![
                RouteConfig {
                    model: PLAIN_MODEL.to_string(),
                    provider: "anthropic".to_string(),
                    upstream_model: None,
                    effort: None,
                    service_tier: None,
                },
                RouteConfig {
                    model: "claude-opus-4-8".to_string(),
                    provider: "anthropic".to_string(),
                    upstream_model: None,
                    effort: None,
                    service_tier: None,
                },
            ],
            ..Config::default()
        }
    }

    /// A transcript of `turns` assistant turns, each a `tool_use` answered by a
    /// `tool_result` in the following user message — the join `signals::extract`
    /// is built around. Every eighth result fails, so the scorer sees a mix
    /// rather than a degenerate all-clean history that could let a future
    /// short-circuit look fast for the wrong reason.
    fn transcript(turns: usize) -> Vec<Value> {
        let names = ["Read", "Edit", "Bash", "Grep", "TodoWrite", "Task"];
        let mut messages = Vec::with_capacity(turns * 2);
        for turn in 0..turns {
            messages.push(json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Inspecting the next module."},
                    {
                        "type": "tool_use",
                        "id": format!("toolu_{turn}"),
                        "name": names[turn % names.len()],
                        "input": {"path": format!("/repo/src/module_{turn}.rs")}
                    }
                ]
            }));
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("toolu_{turn}"),
                    "is_error": turn % 8 == 7,
                    "content": [{"type": "text", "text": "representative tool output"}]
                }]
            }));
        }
        messages
    }

    fn request(model: &str, turns: usize) -> Value {
        json!({
            "model": model,
            "max_tokens": 32000,
            "stream": true,
            "messages": transcript(turns),
        })
    }

    /// The denominator for every routed number below: what the proxy already
    /// spent on this body before routing runs at all.
    ///
    /// `src/proxy/failover.rs` hands routing a `serde_json::Value` it parsed
    /// from the buffered body, so extraction is a constant factor on top of
    /// this, not a new cost. (`perf_issues::route_parse_and_resolve` is not
    /// this measurement: it deserializes the narrow `RoutingView`, which skips
    /// every field it does not need.) Issue #553 rests this caveat on an
    /// unmeasured comparison; this arm is the comparison.
    #[divan::bench(args = TURN_COUNTS)]
    fn parse_body_to_value(bencher: divan::Bencher, turns: usize) {
        let body = serde_json::to_vec(&request(ROUTER_MODEL, turns)).unwrap();
        bencher
            .with_inputs(|| body.as_slice())
            .bench_refs(|body| divan::black_box(serde_json::from_slice::<Value>(body).unwrap()));
    }

    /// Issue #553: cost of one extraction against history length.
    #[divan::bench(args = TURN_COUNTS)]
    fn extract_signals(bencher: divan::Bencher, turns: usize) {
        let messages = Value::Array(transcript(turns));
        bencher.bench(|| divan::black_box(bench_support::extract_signals(&messages, 3)));
    }

    /// A store holding `MAX_TRACKED_SESSIONS` live pins — the saturated state
    /// both store benchmarks measure against.
    fn saturated_store(router: &StageRouterConfig, now: Instant) -> StageStore {
        let store = StageStore::new();
        for index in 0..MAX_TRACKED_SESSIONS {
            store.turn(ROUTER_MODEL, &format!("seed-{index}"), router, now);
        }
        store
    }

    /// Issue #552, control arm: an existing session's update never grows the map
    /// and so never reaches `evict`'s scans.
    #[divan::bench]
    fn store_turn_existing_session(bencher: divan::Bencher) {
        let router = router();
        let now = Instant::now();
        let store = saturated_store(&router, now);
        bencher.bench(|| {
            divan::black_box(store.turn(ROUTER_MODEL, "seed-0", &router, now));
        });
    }

    /// Issue #552, the case the issue describes: every iteration is a
    /// previously unseen session id against a full store, so every iteration
    /// pays a `retain` walk plus a `min_by_key` walk under the lock.
    #[divan::bench]
    fn store_turn_new_session_at_capacity(bencher: divan::Bencher) {
        let router = router();
        let now = Instant::now();
        let store = saturated_store(&router, now);
        let mut nonce = 0usize;
        bencher.bench_local(|| {
            nonce += 1;
            divan::black_box(store.turn(ROUTER_MODEL, &format!("fresh-{nonce}"), &router, now));
        });
    }

    /// The whole router-backed request path: score, hysteresis, commit.
    #[divan::bench(args = TURN_COUNTS)]
    fn resolve_chain_routed(bencher: divan::Bencher, turns: usize) {
        let config = config();
        let request = request(ROUTER_MODEL, turns);
        let store = StageStore::new();
        let now = Instant::now();
        bencher.bench(|| {
            divan::black_box(
                bench_support::resolve_chain(
                    &config,
                    &store,
                    &request,
                    Some("bench-session"),
                    false,
                    now,
                )
                .unwrap(),
            )
        });
    }

    /// The control for the arm above: the identical body against a model id
    /// with no router table. The gap between the two is what non-router traffic
    /// does *not* pay.
    #[divan::bench(args = TURN_COUNTS)]
    fn resolve_chain_unrouted(bencher: divan::Bencher, turns: usize) {
        let config = config();
        let request = request(PLAIN_MODEL, turns);
        let store = StageStore::new();
        let now = Instant::now();
        bencher.bench(|| {
            divan::black_box(
                bench_support::resolve_chain(
                    &config,
                    &store,
                    &request,
                    Some("bench-session"),
                    false,
                    now,
                )
                .unwrap(),
            )
        });
    }
}
