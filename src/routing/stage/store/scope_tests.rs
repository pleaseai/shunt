//! Pin-scope and compaction-latch tests — ADR-0005 §8, PR 1's definition of
//! done, one test per clause.
//!
//! * *Behaviour-preserving without the hints*:
//!   `a_hint_less_request_pins_exactly_as_before` — a `RouterContext` carrying
//!   only a session id keys to the parent scope and reads the pin a
//!   session-only caller wrote.
//! * *Child errors leave the parent pin untouched*:
//!   `a_child_error_leaves_the_parent_pin_untouched` and its twin
//!   `a_child_does_not_inherit_the_parent_tier`. Non-vacuity: drop the agent
//!   digest from `session_key` — key on `(model, session)` again — and both go
//!   red, because the child's escalation lands on the parent's entry.
//! * *Child fan-out at capacity cannot evict an idle capable parent*:
//!   `child_fan_out_at_capacity_cannot_evict_an_idle_capable_parent`.
//!   Non-vacuity: trim both scopes against one shared recency order and it
//!   goes red — the parent is the least recently decided entry and is popped
//!   first. `children_still_evict_against_their_own_budget` is its twin, so a
//!   trim that stopped evicting altogether fails there instead of passing
//!   both.
//! * *A `context-compacted` turn escalates and the next turn of that session
//!   still reads `compacted = true`*:
//!   `a_compacted_turn_escalates_and_the_next_turn_still_reads_compacted`.
//!   Non-vacuity: stop writing `compacted` into the pin and the second turn's
//!   closure sees `false`; stop reading it back and the same. The three
//!   narrowing tests pin the latch's edges: it clears with the pin's TTL, a
//!   probe reads it without writing it, and a sessionless request latches for
//!   that one turn only.
//! * *The latch survives two turns of one pin overlapping*:
//!   `a_racing_ordinary_turn_does_not_clear_the_latch` and
//!   `a_superseded_compacted_turn_still_latches` — the same interleaving with
//!   the commits either way round, because the header is one-shot and neither
//!   commit order may lose it. `an_inherited_latch_does_not_outlive_the_ttl`
//!   bounds that: surviving a lost race is the header's privilege, not an
//!   inherited flag's, or the latch would renew itself past every expiry.
//!
//! `main_with_an_agent_id_pins_as_the_parent` covers the class-authoritative
//! branch §5 calls for and the live capture never observed.

use std::time::{Duration, Instant};

use super::entries::{session_key, MAX_TRACKED_CHILD_PINS, MAX_TRACKED_SESSIONS};
use super::*;
use crate::config::StageRouterPicker;
use crate::routing::context::RequestClass;
use switchyard_libsy::DecisionSource;

const SESSION: &str = "0f0a2cc3-d5f1-4200-b9c8-f56a081194ce";
const CHILD: &str = "a7a11c2e22e29e67a";
const MODEL: &str = "claude-auto";
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

fn capable() -> StageDecision {
    StageDecision {
        tier: StageTier::Capable,
        source: DIMENSIONS,
        confidence: Some(0.76),
    }
}

/// Confident enough to escalate, not enough to de-escalate a capable pin.
fn efficient() -> StageDecision {
    StageDecision {
        tier: StageTier::Efficient,
        source: DIMENSIONS,
        confidence: Some(0.6),
    }
}

/// The parent's own turn: session id only, the way every turn looked before
/// the hints existed and the way every `main` turn looks with the gate off.
fn parent() -> RouterContext<'static> {
    RouterContext {
        session_id: Some(SESSION),
        ..RouterContext::default()
    }
}

/// A `Task` child's turn as captured on the wire: the parent's session id, its
/// own agent id, the class the gate adds when it is on.
fn child(agent_id: &str) -> RouterContext<'_> {
    RouterContext {
        session_id: Some(SESSION),
        agent_id: Some(agent_id),
        request_class: Some(RequestClass::Subagent),
        agent_type: Some("Explore"),
        context_compacted: false,
    }
}

/// `apply` + `commit` for one turn, returning the decision.
fn turn(
    store: &StageRouterStore,
    hints: &RouterContext<'_>,
    estimate: StageDecision,
    now: Instant,
) -> StageDecision {
    let applied = store.apply(MODEL, hints, &router(), |_| estimate, false, now);
    if let Some(pin) = applied.pin {
        store.commit(pin, now);
    }
    applied.decision
}

/// `apply` + `commit` for one turn, returning what the estimate closure was
/// told about the compaction latch.
fn turn_reads_compacted(
    store: &StageRouterStore,
    hints: &RouterContext<'_>,
    read_only: bool,
    now: Instant,
) -> bool {
    let mut seen = None;
    let applied = store.apply(
        MODEL,
        hints,
        &router(),
        |compacted| {
            seen = Some(compacted);
            efficient()
        },
        read_only,
        now,
    );
    if let Some(pin) = applied.pin {
        store.commit(pin, now);
    }
    seen.expect("the estimate closure is always called")
}

#[test]
fn a_hint_less_request_pins_exactly_as_before() {
    let store = StageRouterStore::new();
    let now = Instant::now();

    // Written through the pre-hints shape …
    store.apply_now(MODEL, Some(SESSION), &router(), capable(), false, now);
    // … and read through the hint-carrying one, with nothing but the session id.
    let held = turn(&store, &parent(), efficient(), now);

    assert_eq!(
        held.tier,
        StageTier::Capable,
        "the same session, the same pin"
    );
    assert_eq!(held.source, StageSource::Sticky);
    assert_eq!(store.len(), 1, "one key, not one per call shape");
}

#[test]
fn a_child_error_leaves_the_parent_pin_untouched() {
    let store = StageRouterStore::new();
    let now = Instant::now();

    // The parent is settled on the efficient tier.
    turn(&store, &parent(), efficient(), now);
    // Its child collapses and escalates — on its own history.
    let escalated = turn(&store, &child(CHILD), capable(), now);
    assert_eq!(
        escalated.tier,
        StageTier::Capable,
        "the child itself escalates"
    );

    // The parent's next turn reads the parent's pin, not the child's.
    let next = turn(&store, &parent(), efficient(), now);
    assert_eq!(
        next.tier,
        StageTier::Efficient,
        "the child's escalation must not move the parent"
    );
    assert_eq!(store.len(), 2, "parent and child hold separate entries");

    let entries = store
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let parent_pin = entries
        .get(&session_key(MODEL, SESSION, None))
        .expect("the parent's pin");
    assert_eq!(parent_pin.tier, StageTier::Efficient);
    assert_eq!(
        parent_pin.dwell_turns, 2,
        "only the parent's own two turns advance its dwell"
    );
}

/// The other half of "never reads": a child is not sticky to its parent's tier.
#[test]
fn a_child_does_not_inherit_the_parent_tier() {
    let store = StageRouterStore::new();
    let now = Instant::now();

    turn(&store, &parent(), capable(), now);
    let first = turn(&store, &child(CHILD), efficient(), now);

    assert_eq!(
        first.tier,
        StageTier::Efficient,
        "an unpinned child answers from its own estimate, not the parent's capable pin"
    );
    assert_eq!(first.source, DIMENSIONS);
}

#[test]
fn two_children_of_one_session_keep_independent_pins() {
    let store = StageRouterStore::new();
    let now = Instant::now();

    turn(&store, &child("acf659811cf929e90"), capable(), now);
    let other = turn(&store, &child("a6544583269e5f238"), efficient(), now);

    assert_eq!(other.tier, StageTier::Efficient);
    assert_eq!(store.len(), 2);
}

/// §5: `main` with an agent id is main traffic. Never observed on the wire;
/// pinned here so the class stays ahead of the id if a client ever sends both.
#[test]
fn main_with_an_agent_id_pins_as_the_parent() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let main_with_id = RouterContext {
        request_class: Some(RequestClass::Main),
        agent_id: Some(CHILD),
        ..parent()
    };

    turn(&store, &parent(), capable(), now);
    let held = turn(&store, &main_with_id, efficient(), now);

    assert_eq!(held.tier, StageTier::Capable, "it read the parent's pin");
    assert_eq!(store.len(), 1, "and wrote to it, not beside it");
}

#[test]
fn child_fan_out_at_capacity_cannot_evict_an_idle_capable_parent() {
    let store = StageRouterStore::new();
    let start = Instant::now();

    // The parent escalated, then went idle waiting on its children: it is
    // the least recently decided entry for the rest of the test.
    turn(&store, &parent(), capable(), start);

    // A fan-out wider than the child budget, every turn newer than the parent's
    // and well inside the TTL, so the cap — not expiry — does the evicting.
    for index in 0..MAX_TRACKED_CHILD_PINS + 8 {
        let agent_id = format!("child-{index}");
        let now = start + Duration::from_millis(index as u64 + 1);
        turn(&store, &child(&agent_id), efficient(), now);
    }

    let entries = store
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        entries.len_of(entries::PinScope::Child),
        MAX_TRACKED_CHILD_PINS,
        "the child budget is enforced"
    );
    assert!(
        entries.contains_key(&session_key(MODEL, SESSION, None)),
        "the idle parent survives the fan-out"
    );
    drop(entries);

    // And it resumes on its pin, dwell gate consulted — not on the picker default.
    let resumed = turn(
        &store,
        &parent(),
        efficient(),
        start + Duration::from_secs(1),
    );
    assert_eq!(resumed.tier, StageTier::Capable);
    assert_eq!(resumed.source, StageSource::Sticky);
}

/// The twin: the child budget is a real cap, not an exemption from eviction.
#[test]
fn children_still_evict_against_their_own_budget() {
    let store = StageRouterStore::new();
    let start = Instant::now();

    turn(&store, &child("child-first"), capable(), start);
    for index in 0..MAX_TRACKED_CHILD_PINS {
        let agent_id = format!("child-{index}");
        let now = start + Duration::from_millis(index as u64 + 1);
        turn(&store, &child(&agent_id), efficient(), now);
    }

    let entries = store
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        entries.len_of(entries::PinScope::Child),
        MAX_TRACKED_CHILD_PINS
    );
    assert!(
        !entries.contains_key(&session_key(MODEL, SESSION, Some("child-first"))),
        "the least recently decided child is the one dropped"
    );
    assert_eq!(
        entries.len_of(entries::PinScope::Parent),
        0,
        "no parent was involved"
    );
}

/// The mirror: a parent fan-in past the parent budget evicts parents only.
#[test]
fn parents_at_capacity_do_not_evict_children() {
    let store = StageRouterStore::new();
    let start = Instant::now();

    turn(&store, &child(CHILD), capable(), start);
    for index in 0..MAX_TRACKED_SESSIONS + 1 {
        let session = format!("session-{index}");
        let hints = RouterContext {
            session_id: Some(&session),
            ..RouterContext::default()
        };
        let now = start + Duration::from_millis(index as u64 + 1);
        turn(&store, &hints, efficient(), now);
    }

    let entries = store
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        entries.len_of(entries::PinScope::Parent),
        MAX_TRACKED_SESSIONS
    );
    assert!(
        entries.contains_key(&session_key(MODEL, SESSION, Some(CHILD))),
        "the oldest entry is a child, and the parent trim must not reach it"
    );
}

#[test]
fn a_compacted_turn_escalates_and_the_next_turn_still_reads_compacted() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    // The turn that carries the one-shot header: the closure is told, and the
    // scorer's compaction override — an `Override` decision — moves the pin.
    let mut seen = None;
    let applied = store.apply(
        MODEL,
        &compacted_turn,
        &router(),
        |compacted| {
            seen = Some(compacted);
            StageDecision {
                tier: StageTier::Capable,
                source: StageSource::Scorer(DecisionSource::Override),
                confidence: Some(1.0),
            }
        },
        false,
        now,
    );
    assert_eq!(seen, Some(true), "the header reaches the estimate");
    assert_eq!(
        applied.decision.tier,
        StageTier::Capable,
        "and the turn escalates"
    );
    store.commit(applied.pin.expect("a session turn earns a pin"), now);

    // The next turn of the session sends no header — the client consumed it —
    // and still reads the latch.
    assert!(
        turn_reads_compacted(&store, &parent(), false, now),
        "the latch outlives the one-shot header"
    );
    assert!(
        turn_reads_compacted(&store, &parent(), false, now),
        "and the turn after that, too"
    );
}

#[test]
fn the_latch_clears_with_the_pin_ttl() {
    let store = StageRouterStore::new();
    let start = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    turn_reads_compacted(&store, &compacted_turn, false, start);
    let expired = start + Duration::from_secs(router().session_ttl_seconds + 1);

    assert!(
        !turn_reads_compacted(&store, &parent(), false, expired),
        "a resumed session past its TTL starts uncompacted"
    );
}

#[test]
fn a_probe_reads_the_latch_without_writing_it() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    // A `count_tokens` probe carrying the header must not latch the session …
    assert!(turn_reads_compacted(&store, &compacted_turn, true, now));
    assert!(
        !turn_reads_compacted(&store, &parent(), false, now),
        "a read-only turn writes no latch"
    );
    // … but a probe after a real compacted turn reads the latch like any turn.
    turn_reads_compacted(&store, &compacted_turn, false, now);
    assert!(turn_reads_compacted(&store, &parent(), true, now));
}

#[test]
fn a_sessionless_request_latches_for_that_turn_only() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let sessionless = RouterContext {
        context_compacted: true,
        ..RouterContext::default()
    };

    assert!(turn_reads_compacted(&store, &sessionless, false, now));
    assert!(
        !turn_reads_compacted(&store, &RouterContext::default(), false, now),
        "nothing was stored to read back"
    );
    assert_eq!(store.len(), 0);
}

/// A child's latch is its own: the parent's compaction does not reach it.
#[test]
fn the_latch_is_scoped_with_the_pin() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    turn_reads_compacted(&store, &compacted_turn, false, now);

    assert!(
        !turn_reads_compacted(&store, &child(CHILD), false, now),
        "a child spawned after the compaction starts with its own history"
    );
}

/// Two turns of one pin overlap: the ordinary turn decides while the compacted
/// turn is still between `apply` and `commit`, then commits after it. The
/// latch must survive that later write.
///
/// Non-vacuity: drop the `session.compacted |=` merge in `commit` and this goes
/// red — the ordinary turn writes the `false` it read from a snapshot taken
/// before the header ever arrived.
#[test]
fn a_racing_ordinary_turn_does_not_clear_the_latch() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    // Both decide against the same empty store, in this order, so the ordinary
    // turn takes the higher `seq` and cannot be superseded below.
    let compacted = store.apply(
        MODEL,
        &compacted_turn,
        &router(),
        |_| efficient(),
        false,
        now,
    );
    let ordinary = store.apply(MODEL, &parent(), &router(), |_| efficient(), false, now);

    store.commit(compacted.pin.expect("the compacted turn pins"), now);
    store.commit(ordinary.pin.expect("the ordinary turn pins"), now);

    assert!(
        turn_reads_compacted(&store, &parent(), false, now),
        "an older snapshot must not clear a latch it never saw"
    );
}

/// The same interleaving with the commits the other way round: the ordinary
/// turn wins the `seq` comparison, so the compacted turn's write arrives
/// superseded. The flag must still land — the header is one-shot, so no later
/// turn can resend it.
///
/// Non-vacuity: remove the `latch_compacted` call from the superseded branch
/// and this goes red — `commit` returns early and the flag dies with the pin.
#[test]
fn a_superseded_compacted_turn_still_latches() {
    let store = StageRouterStore::new();
    let now = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    let compacted = store.apply(
        MODEL,
        &compacted_turn,
        &router(),
        |_| efficient(),
        false,
        now,
    );
    let ordinary = store.apply(MODEL, &parent(), &router(), |_| efficient(), false, now);

    // The later decision commits first, so the compacted one is superseded.
    store.commit(ordinary.pin.expect("the ordinary turn pins"), now);
    store.commit(compacted.pin.expect("the compacted turn pins"), now);

    assert!(
        turn_reads_compacted(&store, &parent(), false, now),
        "a served compacted turn must latch even when its pin loses the seq race"
    );
}
/// The superseded-path latch is the *header's* privilege, not an inherited
/// flag's. A turn that merely read `compacted = true` can still be in flight
/// when the pin it read expires; re-latching from it would write a dead latch
/// onto the fresh entry that replaced it, for another full TTL, and could
/// repeat for as long as turns keep overlapping.
///
/// Non-vacuity: key the superseded branch on `pin.session.compacted` instead of
/// `pin.observed_compaction` and this goes red — the stalled turn revives a
/// latch that `the_latch_clears_with_the_pin_ttl` just established must be gone.
#[test]
fn an_inherited_latch_does_not_outlive_the_ttl() {
    let store = StageRouterStore::new();
    let start = Instant::now();
    let compacted_turn = RouterContext {
        context_compacted: true,
        ..parent()
    };

    // The real compaction, and a turn that inherits its latch without ever
    // seeing the header.
    turn_reads_compacted(&store, &compacted_turn, false, start);
    let stalled = store.apply(MODEL, &parent(), &router(), |_| efficient(), false, start);

    // The pin expires while that turn is still between `apply` and `commit`,
    // and a later turn rebuilds the entry from scratch.
    let expired = start + Duration::from_secs(router().session_ttl_seconds + 1);
    let resumed = store.apply(MODEL, &parent(), &router(), |_| efficient(), false, expired);
    store.commit(resumed.pin.expect("the resumed turn pins"), expired);

    // Now the stalled turn lands, superseded by the entry built after expiry.
    store.commit(stalled.pin.expect("the stalled turn pins"), expired);

    assert!(
        !turn_reads_compacted(&store, &parent(), false, expired),
        "an inherited latch must not be revived onto a post-expiry pin"
    );
}
