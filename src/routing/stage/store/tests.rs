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
//! `a_reconfigured_router_abandons_its_pins` goes red; drop the confidence gate
//! from the `Capable` arm and `a_de_escalation_without_a_confidence_score_is_held`
//! goes red.
//!
//! `an_expired_entry_behind_a_live_one_is_still_the_one_evicted` pins the second
//! index: sweep the front of the recency order instead of the expiry order —
//! which is what this code did before the `BTreeSet` was added — and it goes
//! red, because the live entry in front stops the sweep and is then evicted in
//! the expired entry's place. It is the only test that separates the two
//! orders, so it needs two routers with different `session_ttl_seconds`; under
//! one TTL every other eviction test would pass either way.
//!
//! `a_refreshed_session_is_not_evicted_as_the_oldest` pins the recency index's
//! one invariant: stop retiring a replaced entry's `seq` slot in
//! `Entries::insert` and it goes red, because the stale slot still names the key
//! and the next eviction pops it — deleting a session one turn old while the
//! entry it should have taken stays. `the_store_evicts_the_oldest_session_once_it_is_full`
//! is its twin: an index that retired *every* slot, or none, fails there instead
//! of satisfying both.
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

/// The scorer reason these tests construct by hand. Naming it through
/// `StageSource` keeps the hand-built estimates on the same type the real
/// scorer stamps, so a renamed or removed upstream variant fails to compile
/// here instead of quietly no longer matching what `decide` produces — which is
/// how the removal of libsy's `TestsPassed` surfaced.
const DIMENSIONS: StageSource = StageSource::Scorer(DecisionSource::Dimensions);

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

/// The confidence gate admits no exemption. libsy used to have one
/// de-escalation that reported no confidence at all — the `tests_passed`
/// shortcut, which skipped the scorer — and the gate carried an exemption for
/// it; upstream dropped that rule, so every source `is_signal_evidence` admits
/// now carries a confidence and a `None` must be held rather than trusted.
#[test]
fn a_de_escalation_without_a_confidence_score_is_held() {
    let store = StageRouterStore::new();
    let router = router();
    pin(&store, &router, capable(), 5);

    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        StageDecision {
            tier: StageTier::Efficient,
            source: DIMENSIONS,
            confidence: None,
        },
        false,
        Instant::now(),
    );

    assert_eq!(
        held.tier,
        StageTier::Capable,
        "a de-escalation that reports no confidence cannot clear the floor"
    );
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
        StageSource::Scorer(DecisionSource::CapableHold),
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
    let key = session_key("claude-auto", SESSION, None);
    assert!(
        !store
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key),
        "the least recently decided session is the one dropped"
    );
}

/// A session that keeps being seen must not be evicted as the oldest merely
/// because it was seen *first*.
///
/// The recency index is keyed by `seq`, and a re-pinned session takes a fresh
/// one — so the slot it used to occupy has to be retired with it. Leave the
/// stale slot behind and it still names the key, so the very next eviction pops
/// it and deletes a session that is one turn old while the entry it was meant
/// to drop stays.
#[test]
fn a_refreshed_session_is_not_evicted_as_the_oldest() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    // Seen first, so this turn holds the lowest `seq` in the store.
    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    // Fill to exactly the cap, every filler decided after that first turn and
    // all of them well inside the TTL, so recency alone decides the victim.
    for index in 0..MAX_TRACKED_SESSIONS - 1 {
        let session = format!("filler-{index}");
        store.apply_now(
            "claude-auto",
            Some(&session),
            &router,
            capable(),
            false,
            start + Duration::from_millis(index as u64 + 1),
        );
    }
    // Touch the original again. It is now the *newest* entry, not the oldest.
    let refreshed_at = start + Duration::from_millis(MAX_TRACKED_SESSIONS as u64);
    store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        refreshed_at,
    );
    // One previously unseen id tips the store over the cap.
    store.apply_now(
        "claude-auto",
        Some("overflow"),
        &router,
        capable(),
        false,
        refreshed_at + Duration::from_millis(1),
    );

    let key = session_key("claude-auto", SESSION, None);
    assert!(
        store
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key),
        "the refreshed session was the newest entry, so eviction must not take it"
    );
    assert_eq!(store.len(), MAX_TRACKED_SESSIONS);
}

/// An expired entry is dropped even when a *live* entry sits in front of it.
///
/// Recency and expiry are only the same order under a single TTL. Give one
/// router a one-second window and another an hour, and the store can hold the
/// live hour-long pin as its oldest entry with an expired one-second entry
/// behind it. A trim that swept the front of the recency order would stop at
/// the live pin, leave the expired entry in place, and then evict that live pin
/// to get back under the cap — dropping the entry it had just decided to keep.
#[test]
fn an_expired_entry_behind_a_live_one_is_still_the_one_evicted() {
    let store = StageRouterStore::new();
    let long = router();
    let short = StageRouterConfig {
        session_ttl_seconds: 1,
        ..router()
    };
    let start = Instant::now();

    // Decided first, so it is the oldest entry in recency order — and it is
    // still live an hour from now.
    store.apply_now("claude-auto", Some(SESSION), &long, capable(), false, start);
    // Decided second, so it sits *behind* the live pin in recency order, and
    // its one-second window has elapsed by the time the overflow lands.
    store.apply_now(
        "claude-cheap",
        Some("stale"),
        &short,
        capable(),
        false,
        start + Duration::from_millis(1),
    );
    // Long-TTL fillers up to exactly the cap: live, so only the pair above can
    // decide what the sweep takes.
    for index in 0..MAX_TRACKED_SESSIONS - 2 {
        let session = format!("filler-{index}");
        store.apply_now(
            "claude-auto",
            Some(&session),
            &long,
            capable(),
            false,
            start + Duration::from_millis(index as u64 + 2),
        );
    }

    // One previously unseen id tips the store over the cap, two seconds in —
    // past the short window, nowhere near the long one.
    store.apply_now(
        "claude-auto",
        Some("overflow"),
        &long,
        capable(),
        false,
        start + Duration::from_secs(2),
    );

    let entries = store
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !entries.contains_key(&session_key("claude-cheap", "stale", None)),
        "the expired entry must be dropped even though a live entry precedes it"
    );
    assert!(
        entries.contains_key(&session_key("claude-auto", SESSION, None)),
        "an hour-long pin two seconds old must survive, oldest or not"
    );
    assert_eq!(entries.len(), MAX_TRACKED_SESSIONS);
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

    let key = session_key("claude-auto", SESSION, None);
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
    } = store.apply_session("claude-auto", Some(SESSION), &router, capable(), false, now);
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
        store.apply_session("claude-auto", Some(SESSION), &router, capable(), true, now);
    assert!(probe.is_none(), "a count_tokens probe earns no pin");

    let StageApplied { pin: stateless, .. } =
        store.apply_session("claude-auto", None, &router, capable(), false, now);
    assert!(stateless.is_none(), "a sessionless turn earns no pin");

    let StageApplied { pin: blank, .. } =
        store.apply_session("claude-auto", Some(""), &router, capable(), false, now);
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
    let StageApplied { pin: older, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    let StageApplied { pin: newer, .. } = store.apply_session(
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

    let StageApplied { pin: first, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    store.commit(first.expect("the first turn earns a pin"), start);

    let StageApplied { pin: second, .. } = store.apply_session(
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

/// The judge budget is a property of the pin, so it has to survive the
/// decide/commit split: `apply` reports what the session had already spent and
/// `commit` adds this turn's call to whatever the live entry holds. Stop
/// charging in `commit` and the budget never rises, so `max_judge_calls` is
/// unreachable and every turn of a session consults.
#[test]
fn judge_calls_accumulate_across_commits() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    let StageApplied {
        pin,
        judge_calls_used,
        ..
    } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    assert_eq!(judge_calls_used, 0, "a first turn has spent nothing");
    let mut pin = pin.expect("a session-bearing turn earns a pin");
    pin.record_judge_call();
    store.commit(pin, start);

    let StageApplied {
        pin,
        judge_calls_used,
        ..
    } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(1),
    );
    assert_eq!(judge_calls_used, 1, "the first turn's call is charged");
    let mut pin = pin.expect("a session-bearing turn earns a pin");
    pin.record_judge_call();
    store.commit(pin, start + Duration::from_secs(1));

    let StageApplied {
        judge_calls_used, ..
    } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(2),
    );
    assert_eq!(judge_calls_used, 2, "calls add up rather than replacing");
}

/// A turn whose pin lost the supersession race made its judge call, but the pin
/// it was made against is no longer the session's — and the surviving pin
/// carries its own count. Charging it anyway would let two concurrent turns
/// spend a one-call budget twice over.
#[test]
fn a_superseded_commit_does_not_charge_the_budget() {
    let store = StageRouterStore::new();
    let router = router();
    let start = Instant::now();

    let StageApplied { pin: older, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    let StageApplied { pin: newer, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(1),
    );

    let mut newer = newer.expect("the newer turn earns a pin");
    newer.record_judge_call();
    store.commit(newer, start);

    let mut older = older.expect("the older turn earns a pin");
    older.record_judge_call();
    store.commit(older, start);

    let StageApplied {
        judge_calls_used, ..
    } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(2),
    );
    assert_eq!(
        judge_calls_used, 1,
        "only the surviving pin's call is charged"
    );
}

/// `min_dwell_turns` counts turns *served at this tier*, so a verdict that moves
/// the tier has to restart the window exactly as `apply`'s own flip does.
/// Carrying the inherited count over would let the very next turn move again.
///
/// The probe is an efficient estimate against a judge-set capable pin: with the
/// count carried (3 turns, window of 2) it de-escalates, and with the count
/// restarted it is held. Delete the `dwell_turns = 1` line in `set_tier` and
/// this goes red.
#[test]
fn a_judge_verdict_that_moves_the_tier_restarts_dwell() {
    let router = StageRouterConfig {
        min_dwell_turns: 2,
        ..router()
    };
    let store = StageRouterStore::new();
    let start = Instant::now();

    // Three turns at efficient, so the pin has more dwell than the window.
    for turn in 0..3 {
        let StageApplied { pin, .. } = store.apply_session(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            start + Duration::from_secs(turn),
        );
        let mut pin = pin.expect("a session-bearing turn earns a pin");
        if turn == 2 {
            // The judge disagrees with the estimate on the last of them.
            pin.set_tier(StageTier::Capable);
        }
        store.commit(pin, start + Duration::from_secs(turn));
    }

    let held = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.99),
        true,
        start + Duration::from_secs(3),
    );
    assert_eq!(
        held.tier,
        StageTier::Capable,
        "the judge's tier is one turn old, so the window has not reopened"
    );
}

/// The twin: a verdict that agrees with the estimate is not a move, so it must
/// not reset a window the session had already earned. Without this, a `set_tier`
/// that reset unconditionally would satisfy the test above.
#[test]
fn a_judge_verdict_that_agrees_keeps_the_dwell_count() {
    let router = StageRouterConfig {
        min_dwell_turns: 2,
        ..router()
    };
    let store = StageRouterStore::new();
    let start = Instant::now();

    for turn in 0..3 {
        let StageApplied { pin, .. } = store.apply_session(
            "claude-auto",
            Some(SESSION),
            &router,
            capable(),
            false,
            start + Duration::from_secs(turn),
        );
        let mut pin = pin.expect("a session-bearing turn earns a pin");
        if turn == 2 {
            pin.set_tier(StageTier::Capable);
        }
        store.commit(pin, start + Duration::from_secs(turn));
    }

    let released = store.apply_now(
        "claude-auto",
        Some(SESSION),
        &router,
        efficient(0.99),
        true,
        start + Duration::from_secs(3),
    );
    assert_eq!(
        released.tier,
        StageTier::Efficient,
        "the window was earned over three turns and the verdict changed nothing"
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
    let StageApplied { pin: stale, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &before,
        decision(StageTier::Efficient, DIMENSIONS, 0.9),
        false,
        start,
    );
    let StageApplied { pin: fresh, .. } = store.apply_session(
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
    let StageApplied { pin: fresh, .. } = store.apply_session(
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
    let estimate = super::super::decide(&router, Some(&messages), false);
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

    let StageApplied { pin, .. } = store.apply_session(
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
    let StageApplied { pin: first, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start + Duration::from_secs(1),
    );
    let StageApplied { pin: second, .. } = store.apply_session(
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

    let StageApplied { pin: older, .. } = store.apply_session(
        "claude-auto",
        Some(SESSION),
        &router,
        capable(),
        false,
        start,
    );
    let StageApplied { pin: newer, .. } = store.apply_session(
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
    let StageApplied { pin, .. } = store.apply_session(
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

/// The committed transition is what `[models.router.handoff_notes]` gates on,
/// so pin which turns produce one: not a session's first turn, not a signal
/// that only re-confirms the pinned tier, only a turn that moves it.
///
/// Gating on the *commit* rather than on the decision is what makes the
/// concurrent case in `a_later_decision_is_not_overwritten_by_an_older_one`
/// safe as well — two turns that both decide the same move commit one
/// transition between them, not two.
///
/// Non-vacuity: return the decision's own `changed` instead of the committed
/// transition and the first-turn assertion goes red; report a transition
/// whenever a pin is written and the confirming-turn assertion goes red.
#[test]
fn only_a_turn_that_moves_an_existing_pin_commits_a_transition() {
    let router = router();
    let store = StageRouterStore::new();
    let start = Instant::now();

    let turn = |estimate, at| {
        let applied =
            store.apply_session("claude-auto", Some(SESSION), &router, estimate, false, at);
        let flip = applied.pin.and_then(|pin| store.commit(pin, at));
        (applied.decision.source, flip)
    };

    let (_, first) = turn(efficient(0.9), start);
    assert_eq!(
        first, None,
        "the first turn of a session moves nothing: no earlier turn was served at another tier"
    );

    let (confirming_source, confirming) = turn(efficient(0.9), start + Duration::from_secs(1));
    assert_eq!(
        confirming_source, DIMENSIONS,
        "a confirming signal keeps its scorer source, which is why the source alone cannot gate the note"
    );
    assert_eq!(
        confirming, None,
        "the signals re-confirmed the tier already pinned, so the write moved nothing"
    );

    let (_, escalated) = turn(capable(), start + Duration::from_secs(2));
    assert_eq!(
        escalated,
        Some((StageTier::Efficient, StageTier::Capable)),
        "the turn that took the session off its efficient pin is the handoff"
    );
}

/// `capable_hold_turns` tests.
///
/// The regression guard for the whole feature is that every test above runs at
/// the shunt default of `0` and is unchanged by its existence. These four pin
/// what a non-zero window does.
///
/// Non-vacuity: drop the hold branch from `resolve` and
/// `a_capable_hold_refuses_a_convincing_de_escalation` goes red; make the
/// counter never decrement and `a_capable_hold_expires_after_its_configured_turns`
/// goes red; consume the hold on a read-only turn and
/// `a_probe_neither_sets_nor_consumes_the_hold` goes red.
mod capable_hold {
    use super::*;

    fn held_router(turns: u32) -> StageRouterConfig {
        StageRouterConfig {
            // No dwell floor, and a de-escalation threshold the estimate below
            // clears: without the hold this config de-escalates on turn two, so
            // the hold is the only thing the test can be measuring.
            min_dwell_turns: 0,
            deescalate_threshold: Some(0.5),
            capable_hold_turns: turns,
            ..router()
        }
    }

    /// A de-escalation that clears both shipped gates is still refused while
    /// the window is open, and the refusal is stamped as libsy's own
    /// `capable_hold` rather than as a sticky pin.
    #[test]
    fn a_capable_hold_refuses_a_convincing_de_escalation() {
        let router = held_router(2);
        let store = StageRouterStore::new();
        let now = Instant::now();

        store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
        let decision = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );

        assert_eq!(decision.tier, StageTier::Capable);
        assert_eq!(
            decision.source,
            StageSource::Scorer(DecisionSource::CapableHold)
        );
        assert_eq!(
            decision.source.as_label(),
            "capable_hold",
            "the held turn reports libsy's own label"
        );
        assert!(
            !decision.source.is_signal_evidence(),
            "a held turn is not evidence and must not be able to move a pin"
        );
    }

    /// The window is exactly `capable_hold_turns` long: the same estimate that
    /// was refused N times is honoured on the turn after.
    #[test]
    fn a_capable_hold_expires_after_its_configured_turns() {
        const HOLD: u32 = 3;
        let router = held_router(HOLD);
        let store = StageRouterStore::new();
        let now = Instant::now();

        store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
        for turn in 0..HOLD {
            let decision = store.apply_now(
                "claude-auto",
                Some(SESSION),
                &router,
                efficient(0.99),
                false,
                now,
            );
            assert_eq!(
                decision.tier,
                StageTier::Capable,
                "turn {turn} is still inside the {HOLD}-turn window"
            );
        }

        let decision = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );
        assert_eq!(
            decision.tier,
            StageTier::Efficient,
            "the window is spent, so the shipped gates decide again"
        );
    }

    /// A signal that re-earns the capable tier *inside* an open window does not
    /// extend it: the window is `capable_hold_turns` long from the escalation,
    /// full stop.
    ///
    /// The expiry test above feeds de-escalating estimates through the window,
    /// so it cannot see this case. Pinned separately because the alternative —
    /// re-arming on every confirming turn — turns the hold into a latch that
    /// never closes for a session that keeps scoring capable, and the two are
    /// indistinguishable unless the estimates inside the window say `capable`.
    #[test]
    fn a_confirming_signal_inside_the_window_does_not_extend_it() {
        const HOLD: u32 = 2;
        let router = held_router(HOLD);
        let store = StageRouterStore::new();
        let now = Instant::now();

        store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
        for turn in 0..HOLD {
            let decision =
                store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
            assert_eq!(
                decision.source,
                StageSource::Scorer(DecisionSource::CapableHold),
                "turn {turn} is inside the window, so the hold answers it"
            );
        }

        let decision = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );
        assert_eq!(
            decision.tier,
            StageTier::Efficient,
            "the window is spent after exactly {HOLD} turns however often the signals re-confirmed capable"
        );
    }

    /// A `count_tokens` probe records nothing, so it must neither open a window
    /// nor spend a turn of one. Spending one would let a client shorten
    /// another's hold by probing.
    #[test]
    fn a_probe_neither_sets_nor_consumes_the_hold() {
        let router = held_router(1);
        let store = StageRouterStore::new();
        let now = Instant::now();

        store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
        // A probe covered by the window is held, and spends nothing.
        let probe = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            true,
            now,
        );
        assert_eq!(probe.tier, StageTier::Capable);

        // So the *next* real turn is still the first one the window covers.
        let held = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );
        assert_eq!(
            held.tier,
            StageTier::Capable,
            "the probe must not have spent the one held turn"
        );
        let released = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );
        assert_eq!(released.tier, StageTier::Efficient);
    }

    /// The default: with `capable_hold_turns = 0` the window never opens, which
    /// is what leaves every other test in this file describing the shipped
    /// hysteresis and nothing else.
    #[test]
    fn the_shunt_default_opens_no_window() {
        let router = StageRouterConfig {
            min_dwell_turns: 0,
            deescalate_threshold: Some(0.5),
            ..router()
        };
        assert_eq!(router.capable_hold_turns, 0);
        let store = StageRouterStore::new();
        let now = Instant::now();

        store.apply_now("claude-auto", Some(SESSION), &router, capable(), false, now);
        let decision = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );

        assert_eq!(
            decision.tier,
            StageTier::Efficient,
            "no hold, so the convincing de-escalation lands immediately"
        );
    }

    /// The window only opens on a *signal-driven* move. A picker default that
    /// happens to land capable is not an escalation and must not arm a hold.
    #[test]
    fn a_picker_default_does_not_open_a_window() {
        let router = held_router(2);
        let store = StageRouterStore::new();
        let now = Instant::now();

        store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            StageDecision {
                tier: StageTier::Capable,
                source: StageSource::NoSignal,
                confidence: None,
            },
            false,
            now,
        );
        let decision = store.apply_now(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.99),
            false,
            now,
        );

        assert_eq!(
            decision.tier,
            StageTier::Efficient,
            "a default is not an escalation, so it arms no hold"
        );
    }
}
