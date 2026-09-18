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
//!   describes, where each previously unseen id pays a full `retain` scan — one
//!   scan, not two: the pass that drops expired entries also remembers the
//!   oldest survivor, so the `min_by_key` fallback is never reached. Both run
//!   against a store pre-filled to `MAX_TRACKED_SESSIONS`, so the difference
//!   between them is the eviction cost and not the map size.
//! * `resolve_chain_*` — the whole router-backed request path, and the control
//!   it has to be read against. Both arms send the *same body* naming the *same
//!   model id* through configs that differ only in whether that id's
//!   `[[models]]` entry carries a `[models.stage_router]` table. That pair is
//!   the "non-router traffic pays only one `Option::is_none()`" claim, which is
//!   the assertion most worth protecting from regression.
//!   `resolve_chain_routed_delegated` is the routed arm with a `Task` child's
//!   headers — agent id, class, type — so the ADR-0005 PR 1 additions (header
//!   parsing, the agent-scoped key, the latch read) are priced against the
//!   parent turn, not hidden inside it.
//! * `store_turn_new_child_at_capacity` — the child budget's eviction cost,
//!   the twin of `store_turn_new_session_at_capacity` for the second scope.
//! * `parse_body_to_value` — the denominator. It benchmarks
//!   `RequestBody::parse(body.to_vec())`, the whole expression
//!   `src/proxy/failover.rs` evaluates: not `serde_json::from_slice`, whose
//!   visitor skips the duplicate-key rejection, and not the parse alone, which
//!   would drop the linear copy the buffered request pays on the way in. Both
//!   omissions shrink the denominator, and this arm exists only to be one.

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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use axum::http::HeaderMap;
    use serde_json::{json, Value};
    use shunt::{
        bench_support::{self, StageStore, MAX_TRACKED_CHILD_PINS, MAX_TRACKED_SESSIONS},
        config::{Config, ModelConfig, RouteConfig, StageRouterConfig, StageRouterPicker},
    };

    /// Assistant turn counts. The top of the range is a long Claude Code
    /// session, not a synthetic extreme — the window only ever reads the last
    /// `recent_turn_window` of them, which is the asymmetry #553 is about.
    const TURN_COUNTS: [usize; 4] = [10, 50, 200, 800];

    /// The one id both `resolve_chain_*` arms request. They must differ only by
    /// whether its `[[models]]` entry carries a `[models.stage_router]` table:
    /// two different ids would also differ in string length, in position within
    /// `config.models`, and in which lookup arm matches — none of which is the
    /// property under test.
    const ROUTER_MODEL: &str = "claude-auto";
    const EFFICIENT_TARGET: &str = "claude-sonnet-4-6";

    fn router() -> StageRouterConfig {
        StageRouterConfig {
            capable_target: "claude-opus-4-8".to_string(),
            efficient_target: EFFICIENT_TARGET.to_string(),
            picker: StageRouterPicker::EfficientFirst,
            confidence_threshold: 0.5,
            recent_turn_window: 3,
            min_dwell_turns: 3,
            deescalate_threshold: None,
            session_ttl_seconds: 3600,
        }
    }

    /// Two configs identical but for `stage_router`, so the `resolve_chain_*`
    /// pair isolates the router and nothing else.
    ///
    /// `ROUTER_MODEL` carries a `[[routes]]` entry in both. The routed config
    /// never consults it — `resolve_chain` matches `[[models]]` first — and it
    /// is what lets the unrouted config resolve the same id, so the two stay
    /// structurally identical rather than differing in their route tables too.
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
                id: ROUTER_MODEL.to_string(),
                display_name: Some("Auto (stage router)".to_string()),
                upstream_model: None,
                stage_router: with_router.then(router),
            }],
            routes: vec![
                route(ROUTER_MODEL),
                route(EFFICIENT_TARGET),
                route("claude-opus-4-8"),
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

    /// A parent turn's headers as Claude Code sends them behind a gateway with
    /// the hint gate off: the session id and nothing else.
    fn session_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "0f0a2cc3-d5f1-4200-b9c8-f56a081194ce".parse().unwrap(),
        );
        headers
    }

    /// A `Task` child's turn with the hint gate on — the captured shape from
    /// `docs/notes/adr-0005-routing-live-captures.md`, fact (a).
    fn child_headers() -> HeaderMap {
        let mut headers = session_headers();
        headers.insert(
            "x-claude-code-agent-id",
            "a7a11c2e22e29e67a".parse().unwrap(),
        );
        headers.insert("x-claude-code-agent-type", "Explore".parse().unwrap());
        headers.insert("x-claude-code-request-class", "subagent".parse().unwrap());
        headers
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
        // `.to_vec()` is inside the timed closure because it is inside the
        // production call too: `failover.rs` writes
        // `RequestBody::parse(body.to_vec())`, so the buffered request pays that
        // linear copy before the parser sees it. Generating the owned `Vec` in
        // an untimed `with_inputs` would hand the denominator a discount the
        // real path never gets, and this arm is only useful as the denominator.
        bencher.bench(|| {
            divan::black_box(
                bench_support::parse_request_body(divan::black_box(&body).to_vec()).unwrap(),
            )
        });
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
            store.turn(ROUTER_MODEL, &format!("seed-{index}"), None, router, now);
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
            divan::black_box(store.turn(ROUTER_MODEL, "seed-0", None, &router, now));
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
        // The id is built in `with_inputs`, which divan does not time, so neither
        // the `format!` nor its allocation is charged to the eviction this arm
        // exists to measure. The counter is atomic because divan requires the
        // generator to be `Fn + Sync`; cycling a precomputed list instead would
        // repeat ids, and a repeat is an *existing* session, which never evicts.
        let nonce = AtomicUsize::new(0);
        bencher
            .with_inputs(|| format!("fresh-{}", nonce.fetch_add(1, Ordering::Relaxed)))
            .bench_values(|session_id| {
                divan::black_box(store.turn(ROUTER_MODEL, &session_id, None, &router, now));
            });
    }

    /// The child budget's twin of the arm above: one parent, and every
    /// iteration a previously unseen child of it against a full child budget.
    #[divan::bench]
    fn store_turn_new_child_at_capacity(bencher: divan::Bencher) {
        let router = router();
        let now = Instant::now();
        let store = StageStore::new();
        store.turn(ROUTER_MODEL, "parent", None, &router, now);
        for index in 0..MAX_TRACKED_CHILD_PINS {
            store.turn(
                ROUTER_MODEL,
                "parent",
                Some(&format!("seed-{index}")),
                &router,
                now,
            );
        }
        let nonce = AtomicUsize::new(0);
        bencher
            .with_inputs(|| format!("fresh-{}", nonce.fetch_add(1, Ordering::Relaxed)))
            .bench_values(|agent_id| {
                divan::black_box(store.turn(ROUTER_MODEL, "parent", Some(&agent_id), &router, now));
            });
    }

    /// The whole router-backed request path: score, hysteresis, commit.
    #[divan::bench(args = TURN_COUNTS)]
    fn resolve_chain_routed(bencher: divan::Bencher, turns: usize) {
        let config = config(true);
        let request = request(ROUTER_MODEL, turns);
        let headers = session_headers();
        let store = StageStore::new();
        let now = Instant::now();
        bencher.bench(|| {
            divan::black_box(
                bench_support::resolve_chain(&config, &store, &request, &headers, false, now)
                    .unwrap(),
            )
        });
    }

    /// The control for the arm above: the *identical* body and model id, against
    /// a config whose only difference is the absent `[models.stage_router]`
    /// table. The gap between the two is what non-router traffic does not pay.
    #[divan::bench(args = TURN_COUNTS)]
    fn resolve_chain_unrouted(bencher: divan::Bencher, turns: usize) {
        let config = config(false);
        let request = request(ROUTER_MODEL, turns);
        let headers = session_headers();
        let store = StageStore::new();
        let now = Instant::now();
        bencher.bench(|| {
            divan::black_box(
                bench_support::resolve_chain(&config, &store, &request, &headers, false, now)
                    .unwrap(),
            )
        });
    }

    /// The routed arm again, as a `Task` child sends it: the same body, plus the
    /// agent id, class, and type headers. Reads against `resolve_chain_routed`
    /// to price the hint parsing, the agent digest, and the latch read.
    #[divan::bench(args = TURN_COUNTS)]
    fn resolve_chain_routed_delegated(bencher: divan::Bencher, turns: usize) {
        let config = config(true);
        let request = request(ROUTER_MODEL, turns);
        let headers = child_headers();
        let store = StageStore::new();
        let now = Instant::now();
        bencher.bench(|| {
            divan::black_box(
                bench_support::resolve_chain(&config, &store, &request, &headers, false, now)
                    .unwrap(),
            )
        });
    }
}
