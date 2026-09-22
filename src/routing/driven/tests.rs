//! Unit tests for the parts of the driven lane that have no upstream in them:
//! how libsy's evidence becomes a [`RouteSource`], what happens to a verdict
//! naming an id the entry cannot serve, and the budget's bound.
//!
//! `tests/driven_lane.rs` covers the whole request instead — these pin the
//! readings that a live test would only observe through one header.
//!
//! Non-vacuity: collapse `source_for`'s `fall_open` arm into the default and
//! `a_classifier_close_and_a_composite_fall_open_read_differently` goes red;
//! delete the `entry.targets` membership check in `decide` and
//! `a_verdict_outside_the_target_set_falls_open` goes red on the target as
//! well as the outcome label; drop the `used.clear()` in
//! `JudgeBudget::try_charge` and `the_budget_map_is_bounded` goes red on the
//! map's size; drop that method's `>= max` early return and
//! `the_budget_refuses_a_key_that_is_at_its_cap` goes red on its second call.

use switchyard_libsy::{DecisionSource, OutcomeMetadata, RoutingOutcome};
use switchyard_protocol::{ModelId, Request};

use super::budget::JudgeBudget;
use super::drive::{decide, drive_deadline, source_for, DriveNotes};
use super::{build, DrivenEntry};
use crate::config::{CallBounds, RouterConfig, SubagentsConfig};
use crate::routing::outcome::RouteSource;
use crate::routing::serve::JudgeFailure;
use crate::routing::stage::{StageSource, StageTier};

/// The ADR's capability example, reduced to the keys the readings below need.
fn capability_entry() -> DrivenEntry {
    let router: RouterConfig = toml::from_str(
        r#"
        type = "llm_classifier"
        mode = "capability"
        classifier_target = "judge-alias"
        strong_target = "strong-alias"
        weak_target = "weak-alias"
        base_threshold = 0.5
        "#,
    )
    .expect("the capability table parses");
    build::router_entry(&router)
        .expect("the capability algorithm builds")
        .expect("a driven router yields an entry")
}

fn composite_entry() -> DrivenEntry {
    let router: RouterConfig = toml::from_str(
        r#"
        type = "composite"
        [classifier]
        target = "judge-alias"
        base_threshold = 0.5
        classify_trigger = "user_turn"
        [stage]
        capable_target = "capable-alias"
        efficient_target = "efficient-alias"
        confidence_threshold = 0.5
        "#,
    )
    .expect("the composite table parses");
    build::router_entry(&router)
        .expect("the composite algorithm builds")
        .expect("a driven router yields an entry")
}

fn overlay_entry() -> DrivenEntry {
    let overlay: SubagentsConfig = toml::from_str(
        r#"
        type = "llm_classifier"
        mode = "custom"
        models = { judge = ["judge-alias"], capable = ["capable-alias"], any = ["capable-alias"] }
        default_target = "capable"
        prompt = "Select exactly one target."
        response_schema = '{"type": "object"}'
        policy = { type = "target_selector", selector = "/target" }
        "#,
    )
    .expect("the overlay parses");
    build::overlay_entry(overlay.classifier().expect("the classifier form"))
        .expect("the overlay algorithm builds")
}

/// One selection, with the evidence libsy would have stamped beside it.
fn outcome(selected: &str, evidence: Option<&str>) -> RoutingOutcome {
    let mut outcome = RoutingOutcome::route_to(ModelId::from(selected), vec![], Request::default());
    outcome.metadata = Some(OutcomeMetadata::new(
        "test".to_string(),
        evidence.map(|source| serde_json::json!({"source": source})),
    ));
    outcome
}

fn label(fallback: &'static str) -> &'static str {
    fallback
}

/// The table in the module docs, read back arm by arm. The two columns are the
/// point: a classifier entry has no tier to report, a composite one is served
/// by libsy's stage router and must keep landing in the stage series.
#[test]
fn the_evidence_table_maps_every_source_libsy_writes() {
    let classifier = capability_entry();
    for (evidence, expected) in [
        (Some("retained"), RouteSource::DrivenRetained),
        (Some("fail_open"), RouteSource::DrivenFailOpen),
        (Some("fall_open"), RouteSource::DrivenFailOpen),
        (Some("llm-classifier"), RouteSource::Driven),
        (Some("dimensions"), RouteSource::Driven),
        (
            Some("a-label-this-build-does-not-know"),
            RouteSource::Driven,
        ),
        (None, RouteSource::Driven),
    ] {
        assert_eq!(
            source_for(&classifier, evidence, "weak-alias"),
            expected,
            "classifier evidence={evidence:?}"
        );
    }

    let composite = composite_entry();
    for (evidence, selected, expected) in [
        (
            Some("dimensions"),
            "capable-alias",
            RouteSource::Stage(
                StageTier::Capable,
                StageSource::Scorer(DecisionSource::Dimensions),
            ),
        ),
        (
            Some("llm-classifier"),
            "efficient-alias",
            RouteSource::Stage(
                StageTier::Efficient,
                StageSource::Scorer(DecisionSource::LlmClassifier),
            ),
        ),
        (
            Some("capable_hold"),
            "capable-alias",
            RouteSource::Stage(
                StageTier::Capable,
                StageSource::Scorer(DecisionSource::CapableHold),
            ),
        ),
        // Not a `DecisionSource`, so it stays the classifier reading even here.
        (
            Some("retained"),
            "capable-alias",
            RouteSource::DrivenRetained,
        ),
        (
            Some("fail_open"),
            "efficient-alias",
            RouteSource::DrivenFailOpen,
        ),
    ] {
        assert_eq!(
            source_for(&composite, evidence, selected),
            expected,
            "composite evidence={evidence:?}"
        );
    }
}

/// The one string that means two things. `fall_open` closes a classifier
/// cascade (no verdict) and is libsy's stage picker default (a tier), so the
/// same evidence must not produce the same source on both shapes.
#[test]
fn a_classifier_close_and_a_composite_fall_open_read_differently() {
    assert_eq!(
        source_for(&capability_entry(), Some("fall_open"), "strong-alias"),
        RouteSource::DrivenFailOpen
    );
    assert_eq!(
        source_for(&composite_entry(), Some("fall_open"), "efficient-alias"),
        RouteSource::Stage(
            StageTier::Efficient,
            StageSource::Scorer(DecisionSource::FallOpen)
        )
    );
}

/// A verdict naming an id the entry cannot serve is no verdict at all: routing
/// to it would resolve through `server.default_provider` as a literal upstream
/// model name, which is a silently wrong destination.
#[test]
fn a_verdict_outside_the_target_set_falls_open() {
    let entry = capability_entry();
    let decision = decide(
        &entry,
        &outcome("some-other-model", Some("llm-classifier")),
        &label,
    );

    assert_eq!(
        decision.target, "strong-alias",
        "capability falls open strong"
    );
    assert_eq!(decision.source, RouteSource::DrivenFailOpen);
    assert_eq!(decision.judge_outcome, "invalid_reply");
}

/// The overlay form reads the same way, against its own group list.
#[test]
fn an_overlay_verdict_inside_its_groups_is_served() {
    let entry = overlay_entry();
    let decision = decide(
        &entry,
        &outcome("capable-alias", Some("llm-classifier")),
        &label,
    );

    assert_eq!(decision.target, "capable-alias");
    assert_eq!(decision.source, RouteSource::Driven);
    assert_eq!(decision.judge_outcome, "decided");
}

/// A retained turn made no call, and the outcome label must say so rather than
/// counting as a decision the judge was paid for.
#[test]
fn a_retained_turn_reports_retained() {
    let entry = capability_entry();
    let decision = decide(&entry, &outcome("weak-alias", Some("retained")), &label);

    assert_eq!(decision.target, "weak-alias");
    assert_eq!(decision.source, RouteSource::DrivenRetained);
    assert_eq!(decision.judge_outcome, "retained");
}

/// A session id is what makes a budget key; without one the turn is untracked
/// and gets the per-request allowance.
#[test]
fn the_budget_keys_on_session_and_agent() {
    let parent = JudgeBudget::key(Some("session-a"), None).expect("a keyed request");
    let child = JudgeBudget::key(Some("session-a"), Some("agent-1")).expect("a keyed child");
    let sibling = JudgeBudget::key(Some("session-a"), Some("agent-2")).expect("a keyed sibling");

    assert_ne!(parent, child, "a child does not share its parent's budget");
    assert_ne!(child, sibling, "two children do not share a budget");
    assert_eq!(JudgeBudget::key(None, Some("agent-1")), None);
    assert_eq!(JudgeBudget::key(Some("   "), None), None);

    let budget = JudgeBudget::new();
    assert_eq!(budget.used(Some(&child)), 0);
    assert!(budget.try_charge(Some(&child), 8));
    assert_eq!(budget.used(Some(&child)), 1);
    assert_eq!(budget.used(Some(&sibling)), 0, "the charge is scoped");
    assert!(
        budget.try_charge(None, 8),
        "an untracked turn is always admitted"
    );
    assert_eq!(budget.used(None), 0, "an untracked turn writes nothing");
}

/// The reservation refuses once the key is at its cap, which is the half of
/// `max_judge_calls` a single drive can observe: an algorithm that chains a
/// second judge inside one drive is told no rather than charged after the fact.
///
/// Non-vacuity: drop the `>= max` early return in `try_charge` and this goes
/// red on the second call.
///
/// The concurrent arm below is a **guard, not a reproduction**. The race the
/// atomic reservation closes spans the drive — the old code read `used()` in
/// `drive()` and charged only once libsy invoked the call closure, with the
/// decode and the algorithm's own setup in between — so it is not observable
/// from two adjacent calls to this method. A non-atomic `try_charge` was
/// measured to keep this arm green, so it is here to pin the invariant against
/// a future rewrite that widens the window, and it is deliberately not claimed
/// as the test that would have caught the original defect.
#[test]
fn the_budget_refuses_a_key_that_is_at_its_cap() {
    const THREADS: usize = 16;
    const MAX: u32 = 4;

    let budget = JudgeBudget::new();
    let key = JudgeBudget::key(Some("session-a"), Some("agent-1")).expect("a keyed child");

    assert!(budget.try_charge(Some(&key), 1), "the first call fits");
    assert!(
        !budget.try_charge(Some(&key), 1),
        "the second call is past the cap and must be refused"
    );
    assert_eq!(
        budget.used(Some(&key)),
        1,
        "a refused reservation writes nothing"
    );

    let racing = JudgeBudget::new();
    let barrier = std::sync::Barrier::new(THREADS);
    let admitted = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                barrier.wait();
                if racing.try_charge(Some(&key), MAX) {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(
        admitted.load(std::sync::atomic::Ordering::Relaxed),
        MAX as usize,
        "{THREADS} racing turns must not spend more than the cap"
    );
    assert_eq!(racing.used(Some(&key)), MAX, "and the count agrees");
}

/// The outer guard on a drive is sized to the calls the budget can admit, each
/// at its own deadline, so a chaining algorithm's second call is not cut short
/// because the first spent most of one per-call allowance (ADR-0005 §3: the
/// bounds are per call). Replace the multiplication with the bare per-call
/// timeout and the first assertion goes red; drop the slack and the second does.
#[test]
fn the_drive_deadline_covers_every_admissible_call() {
    let bounds = CallBounds {
        judge_timeout: std::time::Duration::from_secs(30),
        judge_max_response_bytes: 1,
        gated_max_bytes: 1,
        gated_idle: std::time::Duration::from_secs(1),
        gated_max_duration: std::time::Duration::from_secs(1),
        max_judge_calls: 3,
    };
    let deadline = drive_deadline(bounds);
    assert!(
        deadline >= std::time::Duration::from_secs(90),
        "three calls at 30s each must fit inside the drive: {deadline:?}"
    );
    assert_eq!(
        deadline,
        std::time::Duration::from_secs(91),
        "and only a second of slack sits on top"
    );
    let ceiling = CallBounds {
        judge_timeout: std::time::Duration::MAX,
        max_judge_calls: u32::MAX,
        ..bounds
    };
    assert_eq!(
        drive_deadline(ceiling),
        std::time::Duration::MAX,
        "bounds at their ceiling saturate rather than overflow"
    );
}

/// The cap is the property: an unbounded map keyed by caller-supplied ids is a
/// memory-growth surface the client controls. The eviction *policy* is not
/// asserted here — it is documented as arbitrary — only that the map never
/// grows past the cap.
#[test]
fn the_budget_map_is_bounded() {
    let budget = JudgeBudget::new();
    for index in 0..JudgeBudget::hard_cap() + 16 {
        let key = JudgeBudget::key(Some(&format!("session-{index}")), None).expect("keyed");
        assert!(budget.try_charge(Some(&key), 1), "each key is fresh");
        assert!(
            budget.len() <= JudgeBudget::hard_cap(),
            "the map grew past the cap at {index}"
        );
    }
    assert!(budget.len() < JudgeBudget::hard_cap() + 16);
}

/// A refused reservation is not the judge's fault. libsy folds a refused
/// `CallModel` into the same "no verdict" a malformed reply produces, so
/// without the drive's own note the deterministic refusal — a chaining
/// algorithm's second call on a budget down to one, or two turns of a session
/// racing for the last one — would be counted as `invalid_reply` and
/// `budget_exhausted` would be reachable only from the pre-drive fast path.
///
/// Non-vacuity: drop the store in `DriveNotes::refuse_budget` and the second
/// assertion goes red; read the flag ahead of the recorded failure instead of
/// behind it and the third does.
#[test]
fn a_refused_reservation_labels_the_drive_budget_exhausted() {
    let notes = DriveNotes::default();
    assert_eq!(
        notes.label("invalid_reply"),
        "invalid_reply",
        "a drive that refused nothing keeps the caller's fallback"
    );

    notes.refuse_budget();
    assert_eq!(notes.label("invalid_reply"), "budget_exhausted");

    // A call that *was* made and failed keeps its own label: that is the more
    // specific fact, and it is the one an operator can act on.
    *notes
        .failure()
        .lock()
        .expect("the drive-notes failure slot is uncontended") = Some(JudgeFailure::Timeout);
    assert_eq!(notes.label("invalid_reply"), "timeout");
}

/// And the label reaches the decision: every terminal branch of a drive reads
/// its outcome through the same closure, so a refusal inside `decide` reports
/// the budget rather than the judge.
#[test]
fn a_refused_reservation_reaches_the_decision_outcome() {
    let entry = capability_entry();
    let notes = DriveNotes::default();
    notes.refuse_budget();
    let decision = decide(
        &entry,
        &outcome("some-other-model", Some("llm-classifier")),
        &|fallback| notes.label(fallback),
    );

    assert_eq!(decision.source, RouteSource::DrivenFailOpen);
    assert_eq!(decision.judge_outcome, "budget_exhausted");
}
