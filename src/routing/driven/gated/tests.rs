//! The gated evidence map and the entries it applies to.
//!
//! `tests/buffer_replay.rs` observes each row through one header on a live
//! request; this pins the whole table, including the rows no mock can reach
//! (an unknown evidence string, a latch with no call).
//!
//! Non-vacuity: collapse any arm of [`read_evidence`] into its neighbour and
//! `every_row_of_the_evidence_map_reads_as_documented` goes red on that row;
//! set `gated: None` on the escalation or advisor build and
//! `only_escalation_and_advisor_entries_are_gated` goes red; drop the gated
//! term from [`gated_deadline`] and `the_outer_deadline_covers_the_gated_turn`
//! goes red; build any of [`decide`]'s early failures with the budget refusal
//! alone and `every_decide_failure_carries_the_failed_judge_outcome` goes red
//! on that row.

use std::time::Duration;

use axum::http::HeaderMap;
use serde_json::json;
use switchyard_libsy::{OutcomeMetadata, RoutingOutcome};
use switchyard_protocol::{text_response, LlmResponse, ModelId, Request, Response};

use super::{decide, gated_deadline, read_evidence, GatedDecision, JudgeReading};
use crate::config::{CallBounds, RouterConfig};
use crate::routing::context::RouterContext;
use crate::routing::driven::drive::DriveNotes;
use crate::routing::driven::{build, GatedKind};
use crate::routing::outcome::RouteSource;
use crate::routing::serve::JudgeFailure;

#[test]
fn every_row_of_the_evidence_map_reads_as_documented() {
    use GatedKind::{Advisor, Escalation};
    use JudgeReading::{Decided, FromNotes, Retained, Unrecorded};
    let rows = [
        // Escalation, the weak turn served.
        (
            Escalation,
            true,
            Some(json!({"source": "fail_open"})),
            RouteSource::DrivenFailOpen,
            FromNotes,
        ),
        (
            Escalation,
            true,
            Some(json!({"source": "escalation", "verdict": "continue"})),
            RouteSource::EscalationWeak,
            Decided,
        ),
        (
            Escalation,
            true,
            Some(json!({"source": "escalation", "verdict": "pending"})),
            RouteSource::EscalationWeak,
            Decided,
        ),
        // Escalation, the strong tier dispatched live.
        (
            Escalation,
            false,
            Some(json!({"source": "escalation", "verdict": "latched"})),
            RouteSource::EscalationLatch,
            Retained,
        ),
        (
            Escalation,
            false,
            Some(json!({"source": "escalation", "verdict": "escalate"})),
            RouteSource::EscalationLatch,
            Decided,
        ),
        (
            Escalation,
            false,
            Some(json!({"source": "fallback", "reason_code": "transport"})),
            RouteSource::EscalationFallback,
            Unrecorded,
        ),
        (
            Escalation,
            false,
            Some(json!({"source": "fallback", "reason_code": "context_window"})),
            RouteSource::EscalationFallback,
            Unrecorded,
        ),
        (
            Escalation,
            false,
            None,
            RouteSource::EscalationLatch,
            Unrecorded,
        ),
        // Advisor, the executor turn served.
        (
            Advisor,
            true,
            Some(json!({"source": "advisor", "verdict": "approve"})),
            RouteSource::AdvisorApprove,
            Decided,
        ),
        (
            Advisor,
            true,
            Some(json!({"source": "advisor", "verdict": "fail_open"})),
            RouteSource::AdvisorFailOpen,
            FromNotes,
        ),
        (Advisor, true, None, RouteSource::AdvisorPass, Unrecorded),
        // Advisor, a live dispatch.
        (
            Advisor,
            false,
            Some(json!({"source": "advisor", "verdict": "redo"})),
            RouteSource::AdvisorRedo,
            Decided,
        ),
        (
            Advisor,
            false,
            None,
            RouteSource::AdvisorExhausted,
            Unrecorded,
        ),
    ];
    for (kind, served, evidence, source, reading) in rows {
        assert_eq!(
            read_evidence(kind, served, evidence.as_ref()),
            (source, reading),
            "{kind:?}, served={served}, evidence={evidence:?}"
        );
    }
}

/// The labels the header and the decision counter carry. A closed set, and
/// none of them a stage label.
#[test]
fn the_gated_sources_have_their_documented_labels_and_no_tier() {
    for (source, label) in [
        (RouteSource::EscalationWeak, "escalation_weak"),
        (RouteSource::EscalationLatch, "escalation_latch"),
        (RouteSource::EscalationFallback, "escalation_fallback"),
        (RouteSource::AdvisorApprove, "advisor_approve"),
        (RouteSource::AdvisorFailOpen, "advisor_fail_open"),
        (RouteSource::AdvisorPass, "advisor_pass"),
        (RouteSource::AdvisorRedo, "advisor_redo"),
        (RouteSource::AdvisorExhausted, "advisor_exhausted"),
        (RouteSource::GatedError, "gated_error"),
    ] {
        assert_eq!(source.as_label(), label);
        assert_eq!(source.stage_labels(), None, "{label} has no tier");
    }
}

fn entry(toml: &str) -> crate::routing::driven::DrivenEntry {
    let router: RouterConfig = toml::from_str(toml).expect("the table parses");
    build::router_entry(&router)
        .expect("the algorithm builds")
        .expect("a driven router yields an entry")
}

#[test]
fn only_escalation_and_advisor_entries_are_gated() {
    let escalation = entry(
        r#"
        type = "llm_classifier"
        mode = "escalation"
        classifier_target = "judge"
        strong_target = "strong"
        weak_target = "weak"
        "#,
    );
    assert_eq!(escalation.gated, Some(GatedKind::Escalation));
    assert_eq!(escalation.fail_open, "weak");
    assert_eq!(escalation.targets, vec!["strong", "weak"]);

    let advisor = entry(
        r#"
        type = "advisor"
        executor_target = "executor"
        advisor_target = "advisor"
        "#,
    );
    assert_eq!(advisor.gated, Some(GatedKind::Advisor));
    assert_eq!(
        advisor.targets,
        vec!["executor"],
        "the advisor never serves"
    );
    assert_eq!(advisor.algorithm_label(), "advisor");

    let capability = entry(
        r#"
        type = "llm_classifier"
        mode = "capability"
        classifier_target = "judge"
        strong_target = "strong"
        weak_target = "weak"
        base_threshold = 0.5
        "#,
    );
    assert!(
        !capability.is_gated(),
        "PR 5 entries keep the ungated drive"
    );
}

#[test]
fn the_outer_deadline_covers_the_gated_turn() {
    let bounds = CallBounds {
        judge_timeout: Duration::from_secs(2),
        judge_max_response_bytes: 1,
        gated_max_bytes: 1,
        gated_idle: Duration::from_secs(1),
        gated_max_duration: Duration::from_secs(10),
        max_judge_calls: 3,
    };
    assert_eq!(gated_deadline(bounds), Duration::from_secs(10 + 2 * 3 + 1));
    let unbounded = CallBounds {
        judge_timeout: Duration::MAX,
        gated_max_duration: Duration::MAX,
        max_judge_calls: u32::MAX,
        ..bounds
    };
    assert_eq!(gated_deadline(unbounded), Duration::MAX, "saturates");
}

/// An advisor outcome selecting `selected` (or nothing), with `evidence`, and
/// a response when `served` — libsy's "serve the retained turn" signal.
fn advisor_outcome(
    selected: Option<&str>,
    served: bool,
    evidence: Option<serde_json::Value>,
) -> RoutingOutcome {
    let mut outcome =
        RoutingOutcome::route_to(ModelId::from("executor"), vec![], Request::default());
    outcome.selected_model_ids = selected.into_iter().map(ModelId::from).collect();
    outcome.response = served.then(|| Response {
        llm_response: LlmResponse::Agg(text_response(None, "held")),
        metadata: None,
        upstream_headers: HeaderMap::new(),
    });
    outcome.metadata = Some(OutcomeMetadata::new("test".to_string(), evidence));
    outcome
}

/// Every early failure in [`decide`] carries the drive's failed judge call, as
/// `refuse` does, so a made-and-failed review is still recorded (issue #686).
/// The twin — the same rows with nothing recorded — reads `None`, so a row
/// cannot pass on an outcome the notes did not supply.
#[test]
fn every_decide_failure_carries_the_failed_judge_outcome() {
    let entry = entry(
        r#"
        type = "advisor"
        executor_target = "executor"
        advisor_target = "advisor"
        "#,
    );
    let headers = HeaderMap::new();
    let hints = RouterContext::from_headers(&headers);
    let failed = DriveNotes::default();
    *failed.failure().lock().unwrap() = Some(JudgeFailure::UpstreamStatus);
    let clean = DriveNotes::default();
    let approve = json!({"source": "advisor", "verdict": "approve"});
    let redo = json!({"source": "advisor", "verdict": "redo"});
    let rows = [
        (None, false, None, "gated routing selected no target"),
        (
            Some("elsewhere"),
            false,
            None,
            "gated routing selected a target outside its set",
        ),
        // Served, but nothing was captured to serve.
        (
            Some("executor"),
            true,
            Some(approve),
            "gated routing kept a turn that was not retained",
        ),
        // `Request::default()` has no messages, so there is no REDO tail.
        (
            Some("executor"),
            false,
            Some(redo),
            "the advisor's REDO could not be encoded",
        ),
    ];
    for (selected, served, evidence, expected) in rows {
        for (notes, recorded) in [(&failed, Some("upstream_error")), (&clean, None)] {
            let outcome = advisor_outcome(selected, served, evidence.clone());
            let GatedDecision::Fail {
                message,
                source,
                judge_outcome,
                ..
            } = decide(&entry, GatedKind::Advisor, outcome, None, notes, &hints)
            else {
                panic!("{expected}: decide did not fail the turn");
            };
            assert_eq!(message, expected);
            assert_eq!(source, RouteSource::GatedError, "{expected}");
            assert_eq!(judge_outcome, recorded, "{expected}");
        }
    }
}
