//! Selection tests for `type = "random"`.
//!
//! Non-vacuity: hash the blank session id instead of falling through to the RNG
//! and `sessionless_requests_follow_the_configured_weights` goes red with a
//! 100/0 split. Drop the `seed` from [`session_hash`]'s input and
//! `the_seed_moves_at_least_one_session` goes red. Read the RNG instead of the
//! hash under session affinity and `a_session_id_pins_one_arm` goes red.

use std::cell::Cell;
use std::time::Instant;

use axum::http::HeaderMap;
use serde_json::json;

use super::*;
use crate::routing::stage::StageRouterStore;

fn config(targets: &[&str], weights: Option<Vec<f64>>, seed: Option<u64>) -> RandomRouterConfig {
    RandomRouterConfig {
        targets: targets.iter().map(|target| target.to_string()).collect(),
        weights,
        seed,
        affinity: RandomAffinity::Session,
    }
}

fn headers(session_id: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(session_id) = session_id {
        headers.insert("x-claude-code-session-id", session_id.parse().unwrap());
    }
    headers
}

/// One selection through the live context shape, so the tests exercise the same
/// path `resolve_chain` does rather than a reimplementation of it.
fn pick<'a>(
    router: &'a RandomRouterConfig,
    store: &StageRouterStore,
    session_id: Option<&str>,
) -> (&'a str, RouteSource) {
    let request = json!({"model": "claude-canary"});
    let headers = headers(session_id);
    let context = StageContext {
        store,
        request: &request,
        headers: &headers,
        read_only: false,
        now: Instant::now(),
        pending: Cell::new(None),
        decided: Cell::new(None),
        consult: Cell::new(None),
        prefill: None,
    };
    select(router, "claude-canary", Some(&context))
}

/// The clause ADR-0005 §8 PR 2 names: a caller that sends no session id must
/// still see the configured split, not one shared arm.
#[test]
fn sessionless_requests_follow_the_configured_weights() {
    let router = config(&["major", "minor"], Some(vec![9.0, 1.0]), Some(7));
    let store = StageRouterStore::new();

    let mut minority = 0usize;
    const DRAWS: usize = 2000;
    for _ in 0..DRAWS {
        let (target, source) = pick(&router, &store, None);
        assert_eq!(
            source,
            RouteSource::Random,
            "a sessionless turn is a fresh draw, not a hash-pinned arm"
        );
        if target == "minor" {
            minority += 1;
        }
    }

    // A generous band: the point is that both arms are reachable and roughly in
    // proportion, not that 2000 draws hit 10.0%.
    let share = minority as f64 / DRAWS as f64;
    assert!(
        (0.03..=0.25).contains(&share),
        "a 9:1 split must land the minority arm near 10%, got {share}"
    );
    assert!(minority > 0 && minority < DRAWS, "both arms must be drawn");
}

/// The twin: with a session id the arm is a hash, so it is the same every time
/// and does not depend on how many draws the store has served.
#[test]
fn a_session_id_pins_one_arm() {
    let router = config(&["a", "b"], Some(vec![1.0, 1.0]), None);
    let store = StageRouterStore::new();

    let (first, source) = pick(&router, &store, Some("session-1"));
    assert_eq!(source, RouteSource::RandomSession);
    // Advance the store's RNG between the two reads: if the hash path ever fell
    // through to it, these would differ.
    for _ in 0..16 {
        pick(&router, &store, None);
    }
    let (second, _) = pick(&router, &store, Some("session-1"));

    assert_eq!(first, second, "the arm is a pure hash, not store state");
    // A fresh store must agree, which is what "survives a restart" means.
    let (third, _) = pick(&router, &StageRouterStore::new(), Some("session-1"));
    assert_eq!(first, third);
}

/// Session affinity is a split, not a constant: two ids under equal weights
/// must be able to land on different arms, or the hash is doing nothing.
#[test]
fn two_session_ids_can_land_on_different_arms() {
    let router = config(&["a", "b"], Some(vec![1.0, 1.0]), None);
    let store = StageRouterStore::new();

    let arms: Vec<&str> = (0..32)
        .map(|index| pick(&router, &store, Some(&format!("session-{index}"))).0)
        .collect();

    assert!(
        arms.contains(&"a") && arms.contains(&"b"),
        "32 equally weighted session ids must reach both arms, got {arms:?}"
    );
}

/// `seed` is the hash salt, so changing it must move at least one session.
#[test]
fn the_seed_moves_at_least_one_session() {
    let unseeded = config(&["a", "b"], Some(vec![1.0, 1.0]), None);
    let seeded = config(&["a", "b"], Some(vec![1.0, 1.0]), Some(99));
    let store = StageRouterStore::new();

    let moved = (0..32).any(|index| {
        let session = format!("session-{index}");
        pick(&unseeded, &store, Some(&session)).0 != pick(&seeded, &store, Some(&session)).0
    });

    assert!(moved, "a different salt must repartition some session");
}

/// `affinity = "request"` keeps upstream's meaning: with a seed, the selection
/// *sequence* reproduces — which is what makes a canary run repeatable.
#[test]
fn request_affinity_reproduces_its_sequence_across_stores() {
    let router = RandomRouterConfig {
        affinity: RandomAffinity::Request,
        ..config(&["a", "b", "c"], Some(vec![3.0, 2.0, 1.0]), Some(42))
    };

    let sequence = |store: &StageRouterStore| -> Vec<&str> {
        // A session id is sent on every turn and must be ignored under request
        // affinity, which is the half of the contract a same-id run pins.
        (0..24)
            .map(|_| pick(&router, store, Some("session-1")).0)
            .collect()
    };

    assert_eq!(
        sequence(&StageRouterStore::new()),
        sequence(&StageRouterStore::new())
    );
}

/// A zero weight disables a target without renumbering the list, so it is never
/// drawn and is not the body-less answer either.
#[test]
fn a_zero_weight_disables_its_target() {
    let router = config(&["off", "on"], Some(vec![0.0, 1.0]), Some(1));
    let store = StageRouterStore::new();

    for _ in 0..64 {
        assert_eq!(pick(&router, &store, None).0, "on");
    }
    for index in 0..16 {
        assert_eq!(pick(&router, &store, Some(&format!("s{index}"))).0, "on");
    }
    assert_eq!(
        select(&router, "claude-canary", None).0,
        "on",
        "a body-less resolution must not advertise a disabled target"
    );
}

/// The body-less surfaces (`/routes`, discovery, `resolve_model`) must answer
/// the same thing twice.
#[test]
fn a_body_less_resolution_is_deterministic() {
    let router = config(&["a", "b"], Some(vec![1.0, 1.0]), None);

    assert_eq!(select(&router, "claude-canary", None).0, "a");
    assert_eq!(select(&router, "claude-canary", None).0, "a");
}

/// A reload that changes the weights must reseed rather than continue a stream
/// drawn against the old distribution.
#[test]
fn changing_the_selection_inputs_reseeds_the_stream() {
    let first = config(&["a", "b"], Some(vec![1.0, 1.0]), Some(5));
    let second = config(&["a", "b"], Some(vec![1.0, 3.0]), Some(5));

    assert_ne!(
        fingerprint(&first),
        fingerprint(&second),
        "different weights must key different streams"
    );
    let affinity_only = RandomRouterConfig {
        affinity: RandomAffinity::Request,
        ..first.clone()
    };
    assert_eq!(
        fingerprint(&first),
        fingerprint(&affinity_only),
        "affinity does not change what a draw means"
    );
}
