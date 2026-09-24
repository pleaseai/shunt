//! The stage router's judge budget (ADR-0005 §3, issue #634).
//!
//! The count lives in the store's [`JudgeBudget`] beside the pins rather than
//! on them, is reserved at the moment a call is dispatched
//! ([`StageRouterStore::try_charge_judge`]), is never refunded, and publishes
//! no tier. These pin each of those, plus the lifetime the count kept when it
//! moved off the pin.
//!
//! Non-vacuity: drop the `>= max` refusal in `JudgeBudget::try_charge_within`
//! and `judge_calls_accumulate_until_the_ceiling` goes red on its third turn;
//! refund a superseded turn's call in `commit` — the net effect of the count
//! the pin used to carry — and `a_superseded_commit_does_not_refund_its_call`
//! goes red on the third turn's reservation; write a judged turn's pending pin
//! into the pin map when it is decided, which is what the reverted attempt
//! (the parent of `b6520560`) did to have something to reserve against, and
//! `a_reservation_publishes_no_tier` goes red;
//! drop the `touch` in `commit` and `a_served_session_keeps_its_budget_alive`
//! goes red on its refusal; hash the `fingerprint` out of `stage_key` and
//! `a_reload_that_changes_the_table_restarts_the_budget` goes red; key an
//! unnamed delegate as the parent and
//! `an_unnamed_delegate_does_not_spend_its_parents_budget` goes red.

use super::tests::{capable, efficient, router, SESSION};
use super::*;
use crate::config::StageClassifierConfig;
use crate::routing::context::RequestClass;

const MODEL: &str = "claude-auto";

/// The shared `router()` with a judge and a ceiling of `max`.
fn judged(max: u32) -> StageRouterConfig {
    StageRouterConfig {
        classifier: Some(StageClassifierConfig {
            target: "claude-haiku-4-5".to_string(),
            base_threshold: 0.5,
            classify_trigger: Default::default(),
        }),
        max_judge_calls: max,
        ..router()
    }
}

fn parent() -> RouterContext<'static> {
    RouterContext::session(Some(SESSION))
}

fn delegated(agent_id: Option<&'static str>) -> RouterContext<'static> {
    RouterContext {
        session_id: Some(SESSION),
        agent_id,
        request_class: Some(RequestClass::Subagent),
        ..RouterContext::default()
    }
}

/// `apply` for `hints` with a ready estimate, not read-only.
fn apply(
    store: &StageRouterStore,
    router: &StageRouterConfig,
    hints: &RouterContext<'_>,
    estimate: StageDecision,
    now: Instant,
) -> StageApplied {
    store.apply(MODEL, hints, router, |_| estimate, false, now)
}

/// Reserve one call for the turn `applied` describes.
fn charge(
    store: &StageRouterStore,
    router: &StageRouterConfig,
    applied: &StageApplied,
    now: Instant,
) -> bool {
    store.try_charge_judge(applied.judge_budget.as_ref(), router.max_judge_calls, now)
}

/// Sequential turns add up, and the turn past the ceiling is refused — the
/// sequential property `the_call_budget_is_spent_once_per_session` pins
/// end-to-end, stated against the table the count now lives in.
#[test]
fn judge_calls_accumulate_until_the_ceiling() {
    let store = StageRouterStore::new();
    let router = judged(2);
    let start = Instant::now();

    let admitted: Vec<bool> = (0..3)
        .map(|turn| {
            let now = start + Duration::from_secs(turn);
            let applied = apply(&store, &router, &parent(), capable(), now);
            let admitted = charge(&store, &router, &applied, now);
            store.commit(applied.pin.expect("a session turn earns a pin"), now);
            admitted
        })
        .collect();

    assert_eq!(
        admitted,
        [true, true, false],
        "two calls fit, the third does not"
    );
    let budget = apply(&store, &router, &parent(), capable(), start)
        .judge_budget
        .expect("a judged router counts a session's calls");
    assert_eq!(
        store.judge_calls_at(&budget, start + Duration::from_secs(3)),
        2,
        "a refused reservation writes nothing"
    );
}

/// Issue #634, constraint 2: a turn whose pin loses the `seq` race in `commit`
/// still made its judge call, so the call stays charged. Refunding it — which
/// the pin-held count did, by adding a superseded turn's call to nothing —
/// leaves the budget below the calls actually made.
#[test]
fn a_superseded_commit_does_not_refund_its_call() {
    let store = StageRouterStore::new();
    let router = judged(2);
    let start = Instant::now();

    let older = apply(&store, &router, &parent(), capable(), start);
    let newer = apply(&store, &router, &parent(), capable(), start);
    assert!(
        charge(&store, &router, &older, start),
        "the first call fits"
    );
    assert!(charge(&store, &router, &newer, start), "so does the second");

    store.commit(newer.pin.expect("the newer turn earns a pin"), start);
    assert_eq!(
        store.commit(older.pin.expect("the older turn earns a pin"), start),
        None,
        "the older pin lost the race"
    );

    let third = apply(&store, &router, &parent(), capable(), start);
    assert!(
        !charge(&store, &router, &third, start),
        "both calls were made, so a budget of two is spent"
    );
}

/// Issue #634, constraint 3: a reservation is invisible to routing. A session's
/// first turn reserves its judge call *before* any pin exists — that is what
/// lets concurrent first turns admit one call — and a concurrent turn resolving
/// its tier meanwhile must still see no pin: not the provisional tier the
/// reserving turn chose before its judge answered, and no dwell window on it.
/// Nor may the reservation read back as a flip when the pins are committed.
#[test]
fn a_reservation_publishes_no_tier() {
    let store = StageRouterStore::new();
    let router = judged(1);
    let start = Instant::now();

    // The picker's capable default, before the judge answers.
    let reserving = apply(&store, &router, &parent(), capable(), start);
    assert!(charge(&store, &router, &reserving, start));
    assert_eq!(store.len(), 0, "a reservation writes no pin");

    // A decisive efficient turn of the same session, resolved meanwhile. With
    // a provisional capable pin in place the dwell gate would hold it there.
    let concurrent = apply(&store, &router, &parent(), efficient(0.9), start);
    assert_eq!(
        concurrent.decision.tier,
        StageTier::Efficient,
        "the concurrent turn decided on its own signals"
    );
    assert!(
        !charge(&store, &router, &concurrent, start),
        "and shares the session's one call"
    );

    assert_eq!(
        store.commit(reserving.pin.expect("a pin"), start),
        None,
        "the session's first pin displaced nothing"
    );
    assert_eq!(
        store.commit(concurrent.pin.expect("a pin"), start),
        Some((StageTier::Capable, StageTier::Efficient)),
        "the one real move is the committed one"
    );
}

/// The count kept the pin's lifetime when it moved off the pin: it lives for
/// `session_ttl_seconds` after the session was last *served*, not after its
/// last judge call — a session that keeps working keeps its spent budget — and
/// once the session goes quiet past that window the budget starts over.
#[test]
fn a_served_session_keeps_its_budget_alive() {
    let store = StageRouterStore::new();
    let router = judged(1);
    let ttl = Duration::from_secs(router.session_ttl_seconds);
    let start = Instant::now();

    let first = apply(&store, &router, &parent(), capable(), start);
    assert!(charge(&store, &router, &first, start));
    store.commit(first.pin.expect("a pin"), start);

    // A turn that makes no call, most of a window later.
    let served = start + ttl - Duration::from_secs(1);
    let quiet = apply(&store, &router, &parent(), capable(), served);
    store.commit(quiet.pin.expect("a pin"), served);

    // Past the first call's window, inside the served turn's.
    let later = start + ttl + Duration::from_secs(1);
    let again = apply(&store, &router, &parent(), capable(), later);
    assert!(
        !charge(&store, &router, &again, later),
        "the session was served since its call, so the budget is still spent"
    );

    let resumed = served + ttl + Duration::from_secs(1);
    let fresh = apply(&store, &router, &parent(), capable(), resumed);
    assert!(
        charge(&store, &router, &fresh, resumed),
        "a session quiet past its window gets its budget back, as it gets a fresh pin"
    );
}

/// A reload that changes the router table starts every session's count over,
/// exactly as it abandons their pins.
#[test]
fn a_reload_that_changes_the_table_restarts_the_budget() {
    let store = StageRouterStore::new();
    let before = judged(1);
    let after = StageRouterConfig {
        min_dwell_turns: before.min_dwell_turns + 1,
        ..judged(1)
    };
    let now = Instant::now();

    let spent = apply(&store, &before, &parent(), capable(), now);
    assert!(charge(&store, &before, &spent, now));
    let refused = apply(&store, &before, &parent(), capable(), now);
    assert!(
        !charge(&store, &before, &refused, now),
        "the old table's budget is spent"
    );

    let reloaded = apply(&store, &after, &parent(), capable(), now);
    assert!(
        charge(&store, &after, &reloaded, now),
        "the new table counts from zero"
    );
}

/// Issue #649 on this lane: a delegated turn that sent no agent id draws on a
/// budget of its own — shared by every such turn of the session, and bounded —
/// rather than on its parent's, and a blank id is the same as none.
#[test]
fn an_unnamed_delegate_does_not_spend_its_parents_budget() {
    let store = StageRouterStore::new();
    let router = judged(1);
    let now = Instant::now();
    let charged = |hints: &RouterContext<'_>| {
        let applied = apply(&store, &router, hints, capable(), now);
        charge(&store, &router, &applied, now)
    };

    assert!(charged(&delegated(None)), "the unnamed delegates' one call");
    assert!(
        !charged(&delegated(Some("  "))),
        "a blank id is no id: the same bucket, now spent"
    );
    assert!(
        charged(&parent()),
        "the parent's own budget is untouched by its delegates"
    );
    assert!(
        charged(&delegated(Some("agent-1"))),
        "a named child has its own"
    );
}
