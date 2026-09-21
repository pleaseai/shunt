//! Decision tests.
//!
//! These pin shunt's wiring — the fall-open path, the picker default, and the
//! direction signals push — not libsy's calibration, which is the dependency's
//! own contract. Non-vacuity: make [`decide`] always return `Efficient` and
//! `an_erroring_session_escalates` goes red; make it always return the picker
//! default and that test plus `confidence_is_reported_when_the_scorer_decided`
//! go red.

use super::*;
use serde_json::json;

fn router(picker: StageRouterPicker) -> StageRouterConfig {
    StageRouterConfig {
        capable_target: "claude-opus-4-8".to_string(),
        efficient_target: "claude-sonnet-4-6".to_string(),
        picker,
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

/// A conversation whose recent turns are failing investigation.
fn erroring() -> Value {
    json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "Grep"}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "b", "is_error": true}]},
    ])
}

#[test]
fn a_request_without_messages_lands_on_the_picker_default() {
    // The body-less entry points (/routes, discovery, resolve_model) must
    // still resolve, and must not score silence as agreement.
    for (picker, expected) in [
        (StageRouterPicker::EfficientFirst, StageTier::Efficient),
        (StageRouterPicker::CapableFirst, StageTier::Capable),
    ] {
        let decision = decide(&router(picker), None, false);
        assert_eq!(decision.tier, expected);
        assert_eq!(decision.source, StageSource::NoSignal);
        assert_eq!(decision.confidence, None);
    }
}

#[test]
fn a_conversation_without_tool_activity_lands_on_the_picker_default() {
    let messages = json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]);
    let decision = decide(
        &router(StageRouterPicker::EfficientFirst),
        Some(&messages),
        false,
    );

    assert_eq!(decision.tier, StageTier::Efficient);
    assert_eq!(decision.source, StageSource::NoSignal);
}

/// The whole point of the router: repeated failures move the turn up.
#[test]
fn an_erroring_session_escalates() {
    let messages = erroring();
    let decision = decide(
        &router(StageRouterPicker::EfficientFirst),
        Some(&messages),
        false,
    );

    assert_eq!(
        decision.tier,
        StageTier::Capable,
        "two failed investigative turns must escalate (source {:?}, confidence {:?})",
        decision.source,
        decision.confidence
    );
}

#[test]
fn confidence_is_reported_when_the_scorer_decided() {
    let messages = erroring();
    let decision = decide(
        &router(StageRouterPicker::EfficientFirst),
        Some(&messages),
        false,
    );

    assert!(
        matches!(
            decision.source,
            StageSource::Scorer(DecisionSource::Dimensions | DecisionSource::Override)
        ),
        "an escalation must name its evidence, got {:?}",
        decision.source
    );
}

/// libsy asks for an LLM judge when the signals are weak. shunt runs none,
/// so that request must resolve to the picker default rather than error.
#[test]
fn weak_signals_fall_open_to_the_picker_default() {
    // One clean read: real activity, but far too little to decide.
    let messages = json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a"}]},
    ]);

    for (picker, expected) in [
        (StageRouterPicker::EfficientFirst, StageTier::Efficient),
        (StageRouterPicker::CapableFirst, StageTier::Capable),
    ] {
        let decision = decide(&router(picker), Some(&messages), false);
        assert_eq!(
            decision.tier, expected,
            "a weak signal must land on the picker default, got source {:?}",
            decision.source
        );
        assert_eq!(
            decision.source,
            StageSource::Scorer(DecisionSource::FallOpen)
        );
        // The picker chose this tier, not the scorer, so there is no
        // confidence *in it* to report — see the arm in `decide`.
        assert_eq!(
            decision.confidence, None,
            "a fall-open decision must not look scored"
        );
    }
}

#[test]
fn each_tier_resolves_to_its_configured_target() {
    let router = router(StageRouterPicker::EfficientFirst);

    assert_eq!(StageTier::Capable.target(&router), "claude-opus-4-8");
    assert_eq!(StageTier::Efficient.target(&router), "claude-sonnet-4-6");
}

/// ADR-0005 §11: the compaction latch is fed to the scorer even when the
/// post-compaction history — typically the summary alone — has no tool
/// activity to extract. libsy's override fires on it, and the decision is
/// evidence, so it may move a pin. Non-vacuity: return the picker default
/// on the `(None, true)` arm of `decide` and this goes red.
#[test]
fn a_compacted_turn_escalates_even_without_tool_activity() {
    let summary = json!([{"role": "user", "content": [
        {"type": "text", "text": "This session is being continued from a previous conversation…"}]}]);

    for messages in [None, Some(&summary)] {
        let decision = decide(&router(StageRouterPicker::EfficientFirst), messages, true);

        assert_eq!(decision.tier, StageTier::Capable);
        assert_eq!(
            decision.source,
            StageSource::Scorer(DecisionSource::Override)
        );
        assert!(decision.source.is_signal_evidence());
    }
}

/// The latch rides on top of a scored transcript rather than replacing it.
#[test]
fn a_compacted_turn_with_tool_activity_still_escalates() {
    // One clean read falls open on its own (`weak_signals_fall_open_to_the_picker_default`).
    let messages = json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a"}]},
    ]);
    let decision = decide(
        &router(StageRouterPicker::EfficientFirst),
        Some(&messages),
        true,
    );

    assert_eq!(decision.tier, StageTier::Capable);
    assert_eq!(
        decision.source,
        StageSource::Scorer(DecisionSource::Override)
    );
}

/// The consultation tests. `select` is what decides *whether* a judge is
/// consulted; `routing::judge` decides what the answer means. These pin the
/// three clauses of that gate, and the transcripts they use are the ones the
/// scorer actually classifies that way — `an_undecided_turn_is_a_fall_open`
/// is the check that keeps them honest, because a transcript that stopped
/// falling open would make every assertion below vacuous.
mod consult {
    use super::*;
    use crate::routing::stage::{select, ConsultJudge, StageContext, StageRouterStore};

    const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

    /// One erroring tool result: a single saturated signal scores
    /// `tanh(0.5) ≈ 0.46`, below the 0.5 gate, so the scorer sees the signals
    /// and still cannot decide.
    fn undecided() -> Value {
        json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
        ]})
    }

    fn decisive() -> Value {
        json!({"messages": erroring()})
    }

    fn judged() -> StageRouterConfig {
        StageRouterConfig {
            classifier: Some(crate::config::StageClassifierConfig {
                target: "judge-alias".to_string(),
                base_threshold: 0.5,
            }),
            ..router(StageRouterPicker::EfficientFirst)
        }
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-session-id", SESSION.parse().unwrap());
        headers
    }

    fn consult_for(
        router: &StageRouterConfig,
        store: &StageRouterStore,
        request: &Value,
        read_only: bool,
    ) -> (StageDecision, Option<ConsultJudge>) {
        let context = StageContext {
            store,
            request,
            headers: &headers(),
            read_only,
            now: Instant::now(),
            pending: Cell::new(None),
            decided: Cell::new(None),
            consult: Cell::new(None),
            prefill: None,
        };
        let decision = select(router, "claude-auto", Some(&context));
        if let Some(pin) = context.pending.take() {
            store.commit(pin, context.now);
        }
        (decision, context.consult.take())
    }

    /// The premise the other tests rest on, asserted rather than assumed.
    #[test]
    fn an_undecided_turn_is_a_fall_open() {
        let (decision, _) = consult_for(&judged(), &StageRouterStore::new(), &undecided(), false);
        assert_eq!(
            decision.source,
            StageSource::Scorer(DecisionSource::FallOpen)
        );
    }

    /// The gate itself: the same undecided turn consults with a classifier
    /// configured and consults nothing without one. Drop the
    /// `router.classifier.is_some()` clause and the pure lane starts calling a
    /// judge it never named.
    #[test]
    fn an_undecided_turn_consults_only_with_a_classifier() {
        let (_, consult) = consult_for(&judged(), &StageRouterStore::new(), &undecided(), false);
        assert!(consult.is_some(), "a configured judge is consulted");

        let (_, consult) = consult_for(
            &router(StageRouterPicker::EfficientFirst),
            &StageRouterStore::new(),
            &undecided(),
            false,
        );
        assert!(consult.is_none(), "the pure lane makes no model call");
    }

    /// A `count_tokens` probe must resolve without any model call at all
    /// (ADR-0005 §3), so it never consults even on the turn that otherwise
    /// would.
    #[test]
    fn a_read_only_probe_is_not_consulted() {
        let (_, consult) = consult_for(&judged(), &StageRouterStore::new(), &undecided(), true);
        assert!(consult.is_none(), "a probe makes zero judge calls");
    }

    /// A pinned session already has an answer. Its held turns report `Sticky`
    /// rather than `FallOpen`, and consulting them would spend the whole
    /// `max_judge_calls` budget re-deciding what the pin already decided.
    #[test]
    fn a_turn_held_by_its_pin_is_not_consulted() {
        let router = judged();
        let store = StageRouterStore::new();

        // A decisive turn first, so the session carries a capable pin.
        let (decided, _) = consult_for(&router, &store, &decisive(), false);
        assert_eq!(decided.tier, StageTier::Capable);

        let (held, consult) = consult_for(&router, &store, &undecided(), false);
        assert_eq!(
            held.source,
            StageSource::Sticky,
            "the pin, not the scorer, answered this turn"
        );
        assert!(
            consult.is_none(),
            "a session with an answer buys no second one"
        );
    }
}
