//! The judge budget's driven-lane facts that need no upstream: which drive
//! `max_judge_calls` names as exhausted (issue #648), which budget a
//! delegated turn with no agent id draws on (issue #649), and which count the
//! table evicts at its cap.
//!
//! Split from `tests.rs` only to keep each file under the repo's 500-line
//! ceiling; each test carries its own non-vacuity note.

use std::time::{Duration, Instant};

use super::budget::{JudgeBudget, Window};
use super::drive::DriveNotes;

/// A drive that asked for a call and was refused before making any is
/// `budget_exhausted` whatever libsy's cascade reported — a composite closes a
/// refused judge on its retained tier, which `decide` alone would label
/// `retained` — while a drive that made a call keeps its own reading (issue
/// #648).
///
/// Non-vacuity: drop the `charged` read from `DriveNotes::exhausted` and the
/// last assertion goes red; drop the `budget_refused` read and the second does.
#[test]
fn only_a_drive_refused_before_any_call_is_exhausted() {
    let notes = DriveNotes::default();
    assert_eq!(notes.exhausted(), None, "a drive that asked for nothing");

    notes.refuse_budget();
    assert_eq!(notes.exhausted(), Some("budget_exhausted"));

    let chained = DriveNotes::default();
    chained.charge();
    chained.refuse_budget();
    assert_eq!(
        chained.exhausted(),
        None,
        "a call was made, so the drive's own outcome stands"
    );
}

/// Issue #649: a delegated turn — request class `subagent` or `workflow` —
/// that sent no non-blank agent id keys to a budget of its own, one per
/// session, instead of its parent's. `(session, None)` and `(session, "")`
/// were the same digest as the parent's, so a fan-out of such turns could
/// spend the parent's whole budget.
///
/// Non-vacuity: map `BudgetScope::UnnamedDelegate` to `Parent` in
/// `BudgetScope::of` and the first assertion goes red.
#[test]
fn an_unnamed_delegate_keys_apart_from_its_parent() {
    use crate::routing::context::{RequestClass, RouterContext};

    let delegated = |class, agent_id| RouterContext {
        session_id: Some("session-a"),
        agent_id,
        request_class: Some(class),
        ..RouterContext::default()
    };
    let parent = JudgeBudget::key_for(&RouterContext::session(Some("session-a")));
    let unnamed = JudgeBudget::key_for(&delegated(RequestClass::Subagent, None));

    assert_ne!(unnamed, parent, "an unnamed delegate is not the parent");
    for agent_id in [Some(""), Some("   ")] {
        assert_eq!(
            JudgeBudget::key_for(&delegated(RequestClass::Subagent, agent_id)),
            unnamed,
            "a blank id is no id"
        );
    }
    assert_eq!(
        JudgeBudget::key_for(&delegated(RequestClass::Workflow, None)),
        unnamed,
        "one bucket per session for every unnamed delegate"
    );
    let named = JudgeBudget::key_for(&delegated(RequestClass::Subagent, Some("agent-1")));
    assert_ne!(named, unnamed, "a named child keeps its own");
    assert_eq!(
        named,
        JudgeBudget::key(Some("session-a"), Some("agent-1")),
        "and the same key it always had"
    );
    assert_eq!(
        parent,
        JudgeBudget::key(Some("session-a"), None),
        "as does the parent"
    );
    assert_eq!(
        JudgeBudget::key_for(&RouterContext {
            session_id: None,
            ..delegated(RequestClass::Subagent, None)
        }),
        None,
        "a sessionless turn is still untracked"
    );
}

/// At the cap, an unseen key evicts the least recently charged or touched key
/// of its class — and only that one. Every other live count, including one
/// that has spent its whole budget, stays spent.
///
/// Non-vacuity: put the old `used.clear()` back in place of `make_room` and
/// the exhausted key charges again, so the second assertion goes red; evict
/// an arbitrary entry instead of the least recent and the fourth does (or the
/// second, when the arbitrary pick is the exhausted key).
#[test]
fn eviction_at_the_cap_removes_only_the_least_recent_key() {
    let budget = JudgeBudget::new();
    let start = Instant::now();
    let ttl = Duration::from_secs(24 * 60 * 60);
    let at = |offset: usize| Window {
        now: start + Duration::from_millis(offset as u64),
        ttl,
    };
    let cap = JudgeBudget::hard_cap();
    let keys: Vec<_> = (0..cap)
        .map(|index| JudgeBudget::key(Some(&format!("session-{index}")), None).expect("keyed"))
        .collect();
    for (index, key) in keys.iter().enumerate() {
        assert!(budget.try_charge_within(Some(key), 1, Some(at(index))));
    }
    // Session 0 is served again, so session 1 is now the idlest.
    budget.touch(&keys[0], at(cap));

    let unseen = JudgeBudget::key(Some("session-new"), None).expect("keyed");
    assert!(budget.try_charge_within(Some(&unseen), 1, Some(at(cap + 1))));
    assert!(
        !budget.try_charge_within(Some(&keys[0]), 1, Some(at(cap + 2))),
        "a live, exhausted count stays exhausted after an eviction"
    );
    assert_eq!(budget.len(), cap, "the map holds exactly the cap");
    assert_eq!(
        budget.used_at(&keys[1], at(cap + 2).now),
        0,
        "the idlest went"
    );
    assert!(
        budget.try_charge_within(Some(&keys[1]), 1, Some(at(cap + 3))),
        "the evicted key charges fresh"
    );
    assert_eq!(budget.len(), cap, "and re-entering evicted one more");
    assert!(
        !budget.try_charge_within(Some(&keys[3]), 1, Some(at(cap + 4))),
        "every other live count is still spent"
    );
}

/// Parents and delegated agents are capped apart: a fan-out of more fresh
/// agent ids than the cap evicts delegated counts only, so the parent's spent
/// budget survives it. This is the driven lane's shape — no window, so no
/// entry ever expires.
///
/// Non-vacuity: put the old `used.clear()` back and the parent charges again,
/// so the first assertion goes red; count both classes against one cap and
/// the parent is the least recent entry, so it does as well.
#[test]
fn a_delegated_fan_out_cannot_evict_a_parent() {
    let budget = JudgeBudget::new();
    let parent = JudgeBudget::key(Some("session-a"), None).expect("keyed");
    assert!(budget.try_charge(Some(&parent), 1));

    let cap = JudgeBudget::hard_cap();
    for index in 0..cap + 16 {
        let child =
            JudgeBudget::key(Some("session-a"), Some(&format!("agent-{index}"))).expect("keyed");
        assert!(budget.try_charge(Some(&child), 1), "each agent is fresh");
    }
    assert!(
        !budget.try_charge(Some(&parent), 1),
        "the parent's spent budget survives the fan-out"
    );
    assert_eq!(
        budget.len(),
        cap + 1,
        "a full delegated class and the parent"
    );
}
