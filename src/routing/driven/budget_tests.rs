//! The judge budget's two driven-lane facts that need no upstream: which drive
//! `max_judge_calls` names as exhausted (issue #648), and which budget a
//! delegated turn with no agent id draws on (issue #649).
//!
//! Split from `tests.rs` only to keep each file under the repo's 500-line
//! ceiling; each test carries its own non-vacuity note.

use super::budget::JudgeBudget;
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
