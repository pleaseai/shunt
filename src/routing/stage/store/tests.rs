//! Hysteresis tests.
//!
//! These pin the asymmetry and the invalidation rules, not libsy's scoring —
//! every estimate here is constructed by hand so a test states exactly the
//! scorer output it is reasoning about. Non-vacuity: drop the `Capable` arm's
//! dwell/threshold gate and `a_pinned_capable_tier_holds_through_a_weak_estimate`
//! plus `a_de_escalation_also_needs_the_dwell_window` go red; drop the
//! `Efficient` arm's immediate flip and `an_escalation_needs_no_dwell` goes red;
//! stop recording in `read_only` mode and `a_count_tokens_probe_never_moves_the_pin`
//! goes red; make the fingerprint constant and
//! `a_reconfigured_router_abandons_its_pins` goes red; drop the `tests_passed`
//! exemption from the confidence gate and
//! `a_passing_test_suite_de_escalates_without_a_confidence_score` goes red.
//!
//! Two of these pin an absence, so a stub satisfies them and the mutation has to
//! be the narrower one: expire entries against the *caller's* TTL rather than
//! each entry's own and `a_short_ttl_router_does_not_expire_another_models_pin`
//! goes red; stop filtering an empty session id and
//! `an_empty_session_header_is_not_a_session` goes red.

use super::*;
use crate::config::StageRouterPicker;

const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

fn router() -> StageRouterConfig {
    StageRouterConfig {
        capable_target: "claude-opus-4-8".to_string(),
        efficient_target: "claude-sonnet-4-6".to_string(),
        picker: StageRouterPicker::EfficientFirst,
        confidence_threshold: crate::config::DEFAULT_CONFIDENCE_THRESHOLD,
        recent_turn_window: 3,
        min_dwell_turns: 3,
        deescalate_threshold: None,
        session_ttl_seconds: 3600,
    }
}

fn decision(tier: StageTier, source: &'static str, confidence: f64) -> StageDecision {
    StageDecision {
        tier,
        source,
        confidence: Some(confidence),
    }
}

fn capable() -> StageDecision {
    decision(StageTier::Capable, "dimensions", 0.76)
}

fn efficient(confidence: f64) -> StageDecision {
    decision(StageTier::Efficient, "dimensions", confidence)
}

/// Serve `turns` turns at whatever the estimate says, so a pin accrues dwell.
fn pin(store: &StageRouterStore, router: &StageRouterConfig, estimate: StageDecision, turns: u32) {
    let now = Instant::now();
    for _ in 0..turns {
        store.apply("claude-auto", Some(SESSION), router, estimate, false, now);
    }
}

#[test]
fn a_request_without_a_session_id_is_decided_statelessly() {
    // A bare `curl` sends no session header. It must still route, and must
    // not take a slot that a real session could use.
    let store = StageRouterStore::new();
    let router = router();

    let decision = store.apply(
        "claude-auto",
        None,
        &router,
        capable(),
        false,
        Instant::now(),
    );

    assert_eq!(decision.tier, StageTier::Capable);
    assert_eq!(store.len(), 0);
}

#[test]
fn a_count_tokens_probe_never_moves_the_pin() {
    // Claude Code sends count_tokens with a history one turn behind. A probe
    // that committed would let the stale history drive the pin.
    let store = StageRouterStore::new();
    let router = router();
    let now = Instant::now();

    let probe = store.apply("claude-auto", Some(SESSION), &router, capable(), true, now);

    assert_eq!(probe.tier, StageTier::Capable, "a probe still gets a tier");
    assert_eq!(store.len(), 0, "but it leaves no trace");
}

/// The asymmetry, upward half: a turn the efficient tier cannot serve costs
/// more than the cache prefix an immediate flip forfeits.
#[test]
fn an_escalation_needs_no_dwell() {
    let store = StageRouterStore::new();
    let router = router();
    let now = Instant::now();

    store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        now,
    );
    let escalated = store.apply("claude-auto", Some(SESSION), &router, capable(), false, now);

    assert_eq!(escalated.tier, StageTier::Capable);
    assert_eq!(escalated.source, "dimensions");
}

/// The asymmetry, downward half: 0.6 clears the escalate threshold (0.5) but
/// not the de-escalate one (0.75), so the pin holds.
#[test]
fn a_pinned_capable_tier_holds_through_a_weak_estimate() {
    let store = StageRouterStore::new();
    let router = router();
    assert_eq!(
        router.deescalate_threshold(),
        crate::config::DEFAULT_DEESCALATE_THRESHOLD
    );
    pin(&store, &router, capable(), 5);

    let held = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.6),
        false,
        Instant::now(),
    );

    assert_eq!(held.tier, StageTier::Capable);
    assert_eq!(held.source, "sticky");
}

#[test]
fn a_confident_estimate_de_escalates_once_the_dwell_window_has_passed() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    let dropped = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.8),
        false,
        Instant::now(),
    );

    assert_eq!(dropped.tier, StageTier::Efficient);
    assert_eq!(dropped.source, "dimensions");
}

/// Confidence alone is not enough: the tier must also have been held long
/// enough that the forfeited prompt cache was worth building.
#[test]
fn a_de_escalation_also_needs_the_dwell_window() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 1);

    let held = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        Instant::now(),
    );

    assert_eq!(
        held.tier,
        StageTier::Capable,
        "one turn is not a dwell window"
    );
    assert_eq!(held.source, "sticky");
}

/// libsy's hard de-escalation shortcut skips the scorer and reports no
/// confidence at all (`resolved(Efficient, TestsPassed, 0.0, None)`), so a
/// bare `confidence >= threshold` gate would make the single strongest
/// reason to go cheap the one reason that can never fire.
#[test]
fn a_passing_test_suite_de_escalates_without_a_confidence_score() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    let dropped = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        StageDecision {
            tier: StageTier::Efficient,
            source: "tests_passed",
            confidence: None,
        },
        false,
        Instant::now(),
    );

    assert_eq!(dropped.tier, StageTier::Efficient);
    assert_eq!(dropped.source, "tests_passed");
}

/// It still waits out the dwell window: that gate prices the forfeited
/// prompt cache, which costs the same however strong the evidence is.
#[test]
fn even_a_passing_test_suite_waits_out_the_dwell_window() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 1);

    let held = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        StageDecision {
            tier: StageTier::Efficient,
            source: "tests_passed",
            confidence: None,
        },
        false,
        Instant::now(),
    );

    assert_eq!(held.tier, StageTier::Capable);
    assert_eq!(held.source, "sticky");
}

/// A fall-open is the picker's default, not evidence. It must not be able to
/// unpin a tier the signals chose — in either direction.
#[test]
fn a_fall_open_estimate_cannot_move_a_pinned_tier() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    for source in ["fall_open", "no_signal", "ambiguous"] {
        let held = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            decision(StageTier::Efficient, source, 0.9),
            false,
            Instant::now(),
        );
        assert_eq!(held.tier, StageTier::Capable, "{source} must not unpin");
        assert_eq!(held.source, "sticky");
    }
}

#[test]
fn a_reconfigured_router_abandons_its_pins() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    let mut reloaded = router.clone();
    reloaded.efficient_target = "claude-haiku-4-5".to_string();
    let decision = store.apply(
        "claude-auto",
        Some(SESSION),
        &reloaded,
        efficient(0.6),
        false,
        Instant::now(),
    );

    assert_eq!(
        decision.tier,
        StageTier::Efficient,
        "a pin made under a different table must not bind the new one"
    );
}

#[test]
fn a_pin_expires_once_the_session_goes_quiet() {
    let store = StageRouterStore::new();
    let mut router = router();
    router.session_ttl_seconds = 60;
    let start = Instant::now();
    store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );

    let later = start + Duration::from_secs(61);
    let decision = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.6),
        false,
        later,
    );

    assert_eq!(
        decision.tier,
        StageTier::Efficient,
        "an expired pin must not bind, even below the de-escalate threshold"
    );
}

#[test]
fn two_router_models_in_one_session_keep_independent_tiers() {
    let store = StageRouterStore::new();
    let router = router();
    let now = Instant::now();

    store.apply("claude-auto", Some(SESSION), &router, capable(), false, now);
    let other = store.apply(
        "claude-cheap",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        now,
    );

    assert_eq!(other.tier, StageTier::Efficient);
    assert_eq!(store.len(), 2);
}

#[test]
fn the_store_evicts_the_oldest_session_once_it_is_full() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    // The oldest session is inserted first and never touched again.
    store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    for index in 0..MAX_TRACKED_SESSIONS {
        let session = format!("session-{index}");
        // Milliseconds apart, so every filler stays well inside the TTL and
        // the cap — not expiry — is what does the evicting here.
        let now = start + Duration::from_millis(index as u64 + 1);
        store.apply(
            "claude-auto",
            Some(&session),
            &router,
            capable(),
            false,
            now,
        );
    }

    assert_eq!(store.len(), MAX_TRACKED_SESSIONS);
    let key = session_key("claude-auto", SESSION);
    assert!(
        !store
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key),
        "the least recently seen session is the one dropped"
    );
}

#[test]
fn an_empty_session_header_is_not_a_session() {
    // `x-claude-code-session-id:` with no value reaches the store as
    // `Some("")`. Hashing that gives every client sending it the same key,
    // so they would all share one tier pin for the model.
    let store = StageRouterStore::new();
    let router = router();

    let decision = store.apply(
        "claude-auto",
        Some(""),
        &router,
        capable(),
        false,
        Instant::now(),
    );

    assert_eq!(decision.tier, StageTier::Capable, "it must still route");
    assert_eq!(store.len(), 0, "but an empty id takes no slot");
}

/// The store is shared by every router-backed model, but `evict` runs on
/// whichever request happened to overflow the cap. Expiring against that
/// caller's TTL let a one-second router delete a one-hour router's live pin,
/// and an unpinned session may de-escalate on its very next turn — the flip
/// the dwell window exists to prevent.
#[test]
fn a_short_ttl_router_does_not_expire_another_models_pin() {
    let store = StageRouterStore::new();
    let long = router();
    let short = StageRouterConfig {
        session_ttl_seconds: 1,
        ..router()
    };
    let start = Instant::now();

    // Most of the cap, held by a router whose entries will have aged out of
    // their own one-second window by the time the overflow lands.
    for index in 0..MAX_TRACKED_SESSIONS - 1 {
        let session = format!("short-{index}");
        store.apply(
            "claude-cheap",
            Some(&session),
            &short,
            capable(),
            false,
            start,
        );
    }
    // The long-TTL pin is recorded later, so it is not the oldest entry
    // either: the capacity trim is not what this test is about.
    let pinned_at = start + Duration::from_secs(2);
    store.apply(
        "claude-auto",
        Some(SESSION),
        &long,
        capable(),
        false,
        pinned_at,
    );

    // One more short-TTL request tips the store over the cap, so eviction
    // runs on a call whose router has the one-second window.
    store.apply(
        "claude-cheap",
        Some("overflow"),
        &short,
        capable(),
        false,
        start + Duration::from_secs(4),
    );

    let key = session_key("claude-auto", SESSION);
    assert!(
        store
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key),
        "a one-hour pin two seconds old must survive a one-second router's eviction"
    );
}
