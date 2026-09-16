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
//!
//! `a_decided_turn_records_nothing_until_it_is_committed` is the one that pins
//! the decide/commit split: write inside `apply` again — as the code did before
//! the store write moved past request admission — and its `store.len() == 0`
//! assertion goes red. `a_read_only_or_sessionless_turn_earns_no_pin` asserts an
//! absence, so its mutation is the narrower one: return a pin for the read-only
//! or sessionless arm rather than `None`. Drop `commit`'s `seq` guard so it
//! inserts unconditionally and `a_late_commit_does_not_overwrite_a_newer_decision`
//! goes red; `an_in_order_commit_still_replaces_the_pin` is its twin, so a guard
//! that swallowed *every* write would go red there instead of passing both.
//! Scope that same guard to a matching `fingerprint` and
//! `a_pre_reload_commit_does_not_overwrite_a_post_reload_pin` goes red, with
//! `a_post_reload_decision_still_replaces_an_old_table_pin` as its twin. Both
//! reload tests turn on which tier each turn decides: an unpinned read answers
//! from its own estimate, so the probe has to be one a surviving pin would
//! *change* — a capable pin holding back an efficient probe — or the two
//! outcomes are indistinguishable and the test passes either way.
//!
//! The four flip tests pin what `commit` reports, which is the flip counter's
//! only input. Return `None` unconditionally and
//! `a_committed_escalation_is_reported_as_a_flip` goes red (with
//! `two_concurrent_turns_that_agree_report_one_flip` alongside it, so a `commit`
//! that stopped reporting entirely cannot satisfy either). The other three pin
//! the narrowing clauses, one apiece: drop the `previous != tier` filter and
//! `two_concurrent_turns_that_agree_report_one_flip` goes red with
//! `Some((Capable, Capable))`; report from the superseded path instead of
//! returning early and `a_superseded_commit_reports_no_flip` goes red; drop the
//! `is_live` filter on the entry being replaced and
//! `a_pin_that_expired_is_not_flipped_away_from` goes red.

use super::*;
use crate::config::StageRouterPicker;
use switchyard_libsy::DecisionSource;

const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

/// The two scorer reasons these tests construct by hand. Naming them through
/// `StageSource` keeps the hand-built estimates on the same type the real
/// scorer stamps, so a renamed or removed upstream variant fails to compile
/// here instead of quietly no longer matching what `decide` produces.
const DIMENSIONS: StageSource = StageSource::Scorer(DecisionSource::Dimensions);
const TESTS_PASSED: StageSource = StageSource::Scorer(DecisionSource::TestsPassed);

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

fn decision(tier: StageTier, source: StageSource, confidence: f64) -> StageDecision {
    StageDecision {
        tier,
        source,
        confidence: Some(confidence),
    }
}

fn capable() -> StageDecision {
    decision(StageTier::Capable, DIMENSIONS, 0.76)
}

fn efficient(confidence: f64) -> StageDecision {
    decision(StageTier::Efficient, DIMENSIONS, confidence)
}

/// Serve `turns` turns at whatever the estimate says, so a pin accrues dwell.
fn pin(store: &StageRouterStore, router: &StageRouterConfig, estimate: StageDecision, turns: u32) {
    let now = Instant::now();
    for _ in 0..turns {
        store.apply_now("claude-auto", Some(SESSION), router, estimate, false, now);
    }
}

#[test]
fn a_request_without_a_session_id_is_decided_statelessly() {
    // A bare `curl` sends no session header. It must still route, and must
    // not take a slot that a real session could use.
    let store = StageRouterStore::new();
    let router = router();

    let decision = store.apply_now(
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

    let probe = store.apply_now("claude-auto", Some(SESSION), &router, capable(), true, now);

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

    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        now,
    );
    let escalated = store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);

    assert_eq!(escalated.tier, StageTier::Capable);
    assert_eq!(escalated.source, DIMENSIONS);
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

    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.6),
        false,
        Instant::now(),
    );

    assert_eq!(held.tier, StageTier::Capable);
    assert_eq!(held.source, StageSource::Sticky);
}

#[test]
fn a_confident_estimate_de_escalates_once_the_dwell_window_has_passed() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    let dropped = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.8),
        false,
        Instant::now(),
    );

    assert_eq!(dropped.tier, StageTier::Efficient);
    assert_eq!(dropped.source, DIMENSIONS);
}

/// Confidence alone is not enough: the tier must also have been held long
/// enough that the forfeited prompt cache was worth building.
#[test]
fn a_de_escalation_also_needs_the_dwell_window() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 1);

    let held = store.apply_now(
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
    assert_eq!(held.source, StageSource::Sticky);
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

    let dropped = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        StageDecision {
            tier: StageTier::Efficient,
            source: TESTS_PASSED,
            confidence: None,
        },
        false,
        Instant::now(),
    );

    assert_eq!(dropped.tier, StageTier::Efficient);
    assert_eq!(dropped.source, TESTS_PASSED);
}

/// It still waits out the dwell window: that gate prices the forfeited
/// prompt cache, which costs the same however strong the evidence is.
#[test]
fn even_a_passing_test_suite_waits_out_the_dwell_window() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 1);

    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        StageDecision {
            tier: StageTier::Efficient,
            source: TESTS_PASSED,
            confidence: None,
        },
        false,
        Instant::now(),
    );

    assert_eq!(held.tier, StageTier::Capable);
    assert_eq!(held.source, StageSource::Sticky);
}

/// A fall-open is the picker's default, not evidence. It must not be able to
/// unpin a tier the signals chose — in either direction.
#[test]
fn a_fall_open_estimate_cannot_move_a_pinned_tier() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    // Every source that is not evidence, enumerated over the closed type. A
    // new upstream variant lands in `is_signal_evidence`'s match first, so it
    // cannot reach here unclassified.
    for source in [
        StageSource::Scorer(DecisionSource::FallOpen),
        StageSource::Scorer(DecisionSource::Ambiguous),
        StageSource::Scorer(DecisionSource::LlmClassifier),
        StageSource::NoSignal,
        StageSource::Sticky,
    ] {
        let held = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            decision(StageTier::Efficient, source, 0.9),
            false,
            Instant::now(),
        );
        assert_eq!(held.tier, StageTier::Capable, "{source:?} must not unpin");
        assert_eq!(held.source, StageSource::Sticky);
    }
}

#[test]
fn a_reconfigured_router_abandons_its_pins() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    let mut reloaded = router.clone();
    reloaded.efficient_target = "claude-haiku-4-5".to_string();
    let decision = store.apply_now(
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
    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );

    let later = start + Duration::from_secs(61);
    let decision = store.apply_now(
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

    store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
    let other = store.apply_now(
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
    store.apply_now(
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
        store.apply_now(
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

    let decision = store.apply_now(
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
        store.apply_now(
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
    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &long,
        capable(),
        false,
        pinned_at,
    );

    // One more short-TTL request tips the store over the cap, so eviction
    // runs on a call whose router has the one-second window.
    store.apply_now(
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

/// The write is deferred past admission, so deciding alone must leave the store
/// exactly as it was. Without the split there is nothing to hold back: `apply`
/// wrote before returning, and a request rejected a moment later had already
/// pinned the session.
#[test]
fn a_decided_turn_records_nothing_until_it_is_committed() {
    let store = StageRouterStore::new();
    let router = router();
    let now = Instant::now();

    let StageApplied {
        decision: decided,
        pin,
        ..
    } = store.apply("claude-auto", Some(SESSION), &router, capable(), false, now);
    assert_eq!(decided.tier, StageTier::Capable);
    assert_eq!(store.len(), 0, "apply alone must not write");

    let pin = pin.expect("a session-bearing, non-read-only turn earns a pin");
    store.commit(pin, now);
    assert_eq!(store.len(), 1, "commit is what writes");

    // And the pin that landed is the one that was decided: a following weak
    // estimate is held at capable rather than de-escalating immediately.
    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        false,
        now,
    );
    assert_eq!(
        held.tier,
        StageTier::Capable,
        "the committed pin must be the one the next turn reads"
    );
    let _ = decided;
}

/// The two turns that earn no pin at all, so a caller has nothing to commit and
/// cannot be made to write by calling it.
#[test]
fn a_read_only_or_sessionless_turn_earns_no_pin() {
    let store = StageRouterStore::new();
    let router = router();
    let now = Instant::now();

    let StageApplied { pin: probe, .. } =
        store.apply("claude-auto", Some(SESSION), &router, capable(), true, now);
    assert!(probe.is_none(), "a count_tokens probe earns no pin");

    let StageApplied { pin: stateless, .. } =
        store.apply("claude-auto", None, &router, capable(), false, now);
    assert!(stateless.is_none(), "a sessionless turn earns no pin");

    let StageApplied { pin: blank, .. } =
        store.apply("claude-auto", Some(""), &router, capable(), false, now);
    assert!(blank.is_none(), "a blank session header earns no pin");

    assert_eq!(store.len(), 0);
}

/// Two turns of one session decide against the same pin — the lock is released
/// between deciding and committing — and may then commit in either order. The
/// later *decision* has to win, not the later *write*, or a slow request would
/// reinstate a tier chosen from staler history.
#[test]
fn a_late_commit_does_not_overwrite_a_newer_decision() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    // Both read an empty store, so both are first-turn decisions.
    let StageApplied { pin: older, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    let StageApplied { pin: newer, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        false,
        start + Duration::from_secs(1),
    );

    // The newer turn is admitted first; the older one only afterwards.
    store.commit(newer.expect("the newer turn earns a pin"), start);
    store.commit(older.expect("the older turn earns a pin"), start);

    // Probed with an *efficient* estimate, because that is the one that reads
    // the pin rather than overriding it: escalation is immediate, so a capable
    // estimate would answer `Capable` whichever pin were there. Against an
    // efficient pin this agrees and returns `Efficient`; against a capable pin
    // the dwell window (1 turn of 3) holds it at `Capable`.
    let after = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        true,
        start + Duration::from_secs(2),
    );
    assert_eq!(
        after.tier,
        StageTier::Efficient,
        "the newer decision must survive a later-arriving older commit"
    );
}

/// The mirror: commits that do arrive in order still take effect, so the guard
/// above cannot be satisfied by a `commit` that has stopped writing entirely.
#[test]
fn an_in_order_commit_still_replaces_the_pin() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    let StageApplied { pin: first, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    store.commit(first.expect("the first turn earns a pin"), start);

    let StageApplied { pin: second, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(1),
    );
    store.commit(second.expect("the second turn earns a pin"), start);

    // The second turn's dwell increment is what proves its write landed: with
    // `min_dwell_turns = 3`, two recorded turns still hold the tier, and a third
    // would be needed to release it.
    assert_eq!(store.len(), 1);
    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        true,
        start + Duration::from_secs(2),
    );
    assert_eq!(
        held.tier,
        StageTier::Capable,
        "the pin is still capable, so the second commit did not vanish"
    );
}

/// The supersession guard compares `seq` and nothing else. Scoping it to a
/// matching `fingerprint` looks harmless — a reconfigured table should be able
/// to replace an old-table pin — but two requests straddling a hot reload hold
/// different fingerprints, so the scoped check would not fire and the older
/// request would overwrite the newer table's pin with an entry every later
/// request rejects as stale.
#[test]
fn a_pre_reload_commit_does_not_overwrite_a_post_reload_pin() {
    let store = StageRouterStore::new();
    let before = router();
    let mut after = router();
    // A real edit to the table, so the two decisions hash to different
    // fingerprints exactly as a hot reload would produce.
    after.min_dwell_turns = before.min_dwell_turns + 1;
    let start = Instant::now();

    // The tiers are chosen so the probe below can tell the two outcomes apart.
    // A surviving *capable* post-reload pin holds an efficient probe back; a
    // pre-reload pin that overwrote it is rejected on fingerprint, leaving the
    // probe unpinned and free to answer from its own estimate.
    let StageApplied { pin: stale, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &before,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        false,
        start,
    );
    let StageApplied { pin: fresh, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &after,
        capable(),
        false,
        start + Duration::from_secs(1),
    );

    store.commit(fresh.expect("the post-reload turn earns a pin"), start);
    store.commit(stale.expect("the pre-reload turn earns a pin"), start);

    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &after,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        true,
        start + Duration::from_secs(2),
    );
    assert_eq!(
        held.tier,
        StageTier::Capable,
        "the post-reload pin must survive a later-arriving pre-reload commit"
    );
}

/// The mirror the test above needs: a genuinely later decision under a new table
/// still replaces an old-table entry, so dropping the fingerprint clause did not
/// simply freeze the first pin in place.
#[test]
fn a_post_reload_decision_still_replaces_an_old_table_pin() {
    let store = StageRouterStore::new();
    let before = router();
    let mut after = router();
    after.min_dwell_turns = before.min_dwell_turns + 1;
    let start = Instant::now();

    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &before,
        capable(),
        false,
        start,
    );
    let StageApplied { pin: fresh, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &after,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        false,
        start + Duration::from_secs(1),
    );
    store.commit(fresh.expect("the post-reload turn earns a pin"), start);

    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &after,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        true,
        start + Duration::from_secs(2),
    );
    assert_eq!(held.tier, StageTier::Efficient);
    assert_eq!(store.len(), 1, "the new table's pin replaced, not added to");
}

/// The one link the hand-built estimates in this file cannot check: a source
/// the *real* scorer stamped, carried through `apply` and accepted as evidence.
///
/// Every other test here constructs `StageDecision` by hand, so if `decide`
/// started producing a source that `is_signal_evidence` does not count — a
/// renamed upstream variant, a new one — those tests would keep passing while
/// live pins silently stopped moving. This drives the real `decide` and
/// requires its output to move a pinned tier.
///
/// This fixture's two trailing failures are scored as critical, so the real
/// source here is `Override` rather than `Dimensions` — which is exactly why
/// the test drives `decide` instead of naming a variant it assumed.
///
/// Non-vacuity: make `is_signal_evidence` return `false` for
/// `DecisionSource::Override` and the escalation stops, so the final tier
/// assertion goes red.
#[test]
fn a_real_scorer_decision_is_evidence_that_moves_a_pin() {
    let store = StageRouterStore::new();
    let router = router();
    pin(
        &store,
        &router,
        decision(StageTier::Efficient, DIMENSIONS, 0.76),
        5,
    );

    // Two failed investigative turns: enough for the scorer to decide, which
    // is what makes this an escalation rather than a fall-open.
    let messages = serde_json::json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "Grep"}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "b", "is_error": true}]},
    ]);
    let estimate = super::super::decide(&router, Some(&messages));
    assert_eq!(
        estimate.tier,
        StageTier::Capable,
        "fixture must escalate, got {estimate:?}"
    );
    assert!(
        estimate.source.is_signal_evidence(),
        "the scorer decided this turn, so its source must count as evidence: {:?}",
        estimate.source
    );

    let decided = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        estimate,
        false,
        Instant::now(),
    );
    assert_eq!(decided.tier, StageTier::Capable);
    assert_eq!(decided.source, estimate.source);
}

/// The flip counter's input: `commit` reports the tier change its own write
/// made, so what is counted is what the session actually did.
#[test]
fn a_committed_escalation_is_reported_as_a_flip() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        start,
    );

    let StageApplied { pin, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(1),
    );
    let flip = store.commit(
        pin.expect("the escalating turn earns a pin"),
        start + Duration::from_secs(1),
    );

    assert_eq!(flip, Some((StageTier::Efficient, StageTier::Capable)));
}

/// Why the flip is decided at the write and not from the tier the deciding
/// request read: the store lock is released in between, so two turns of one
/// session both see `efficient`, both choose `capable`, and a flip derived from
/// that shared snapshot would be counted twice for a session that moved once.
///
/// Non-vacuity: report the pre-decision tier instead — `apply` handing back the
/// `pinned` tier and `commit` returning it unconditionally — and the second
/// assertion goes red with `Some((Efficient, Capable))`.
#[test]
fn two_concurrent_turns_that_agree_report_one_flip() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        start,
    );

    // Both decide against the same `efficient` pin: neither has committed yet.
    let StageApplied { pin: first, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(1),
    );
    let StageApplied { pin: second, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(2),
    );

    let first = store.commit(
        first.expect("the first turn earns a pin"),
        start + Duration::from_secs(2),
    );
    let second = store.commit(
        second.expect("the second turn earns a pin"),
        start + Duration::from_secs(2),
    );

    assert_eq!(
        first,
        Some((StageTier::Efficient, StageTier::Capable)),
        "the turn that displaced the efficient pin moved the session"
    );
    assert_eq!(
        second, None,
        "the second turn found capable already pinned, so it moved nothing"
    );
}

/// A write the `seq` guard drops changed nothing, so it flipped nothing — even
/// though the tier it carries differs from the tier now stored.
#[test]
fn a_superseded_commit_reports_no_flip() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    let StageApplied { pin: older, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    let StageApplied { pin: newer, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        start + Duration::from_secs(1),
    );

    store.commit(newer.expect("the newer turn earns a pin"), start);
    let flip = store.commit(older.expect("the older turn earns a pin"), start);

    assert_eq!(flip, None);
}

/// `commit` reads the entry it replaces through the same liveness filter
/// `apply` reads pins through. Without that, resuming a session after its TTL
/// lapsed would report a flip away from a tier no request was served at — the
/// entry was already invisible to the turn that decided.
#[test]
fn a_pin_that_expired_is_not_flipped_away_from() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.9),
        false,
        start,
    );

    // Past the table's `session_ttl_seconds`, so the entry is stale to `apply`.
    let resumed = start + Duration::from_secs(router.session_ttl_seconds + 1);
    let StageApplied { pin, .. } = store.apply(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        resumed,
    );
    let flip = store.commit(pin.expect("the resumed turn earns a pin"), resumed);

    assert_eq!(flip, None);
}
