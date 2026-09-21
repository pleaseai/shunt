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
//! well as the outcome label; drop the `used.clear()` in `JudgeBudget::charge`
//! and `the_budget_map_is_bounded` goes red on the map's size.

use switchyard_libsy::{DecisionSource, OutcomeMetadata, RoutingOutcome};
use switchyard_protocol::{ModelId, Request};

use super::budget::JudgeBudget;
use super::drive::{decide, source_for};
use super::{build, DrivenEntry};
use crate::config::{RouterConfig, SubagentsConfig};
use crate::routing::outcome::RouteSource;
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
    budget.charge(Some(&child));
    assert_eq!(budget.used(Some(&child)), 1);
    assert_eq!(budget.used(Some(&sibling)), 0, "the charge is scoped");
    budget.charge(None);
    assert_eq!(budget.used(None), 0, "an untracked turn writes nothing");
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
        budget.charge(Some(&key));
        assert!(
            budget.len() <= JudgeBudget::hard_cap(),
            "the map grew past the cap at {index}"
        );
    }
    assert!(budget.len() < JudgeBudget::hard_cap() + 16);
}
