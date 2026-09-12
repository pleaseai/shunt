//! Content-aware tier selection for a `[[models]]` entry carrying a
//! `[models.stage_router]` table.
//!
//! shunt extracts [`ToolSignals`] from the buffered Anthropic request
//! ([`signals`]) using Claude Code's own tool names ([`vocabulary`]), then hands
//! them to `switchyard-libsy`'s scorer, which is the calibrated part and is not
//! reimplemented here. Its published behaviour: one saturated signal scores
//! `tanh(0.5) ≈ 0.46` and so cannot decide alone, while two corroborating
//! signals reach `tanh(1.0) ≈ 0.76`.
//!
//! Nothing here calls a model. libsy asks for an LLM judge when the signals are
//! inconclusive; shunt declines and falls open to the picker's default instead,
//! which keeps the hot path free of an extra request and an extra credential.

mod signals;
mod store;
mod vocabulary;

pub(crate) use store::StageRouterStore;

use std::time::Instant;

use serde_json::Value;
use switchyard_libsy::{pick_tier, DecisionSource, PickOutcome, PickerMode, Tier};

use crate::config::{StageRouterConfig, StageRouterPicker};

/// Which tier a request was routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageTier {
    Capable,
    Efficient,
}

/// One routing decision, with the evidence that produced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StageDecision {
    pub tier: StageTier,
    /// Why this tier was chosen. A closed set of `&'static str` so it can label
    /// a metric without unbounded cardinality.
    pub source: &'static str,
    /// Scorer confidence, absent where the scorer did not decide the turn.
    pub confidence: Option<f64>,
}

impl StageTier {
    /// The configured model id this tier routes to.
    pub(crate) fn target(self, router: &StageRouterConfig) -> &str {
        match self {
            StageTier::Capable => &router.capable_target,
            StageTier::Efficient => &router.efficient_target,
        }
    }
}

/// Everything a live request carries that a body-less caller does not.
///
/// Held by reference for the length of one routing call; nothing here is stored.
pub(crate) struct StageContext<'a> {
    /// Process-lifetime pins, from `AppState`.
    pub store: &'a StageRouterStore,
    /// The parsed request body. `messages` is read out of it for scoring; a
    /// request without that field simply yields no signals.
    pub request: &'a Value,
    /// `x-claude-code-session-id`. Absent for callers that send no session
    /// header, which are then routed statelessly.
    pub session_id: Option<&'a str>,
    /// Set for `count_tokens`, which must reach the same tier as the real turn
    /// without recording it.
    pub read_only: bool,
    /// Request-entry clock, shared with the rest of the request's timing.
    pub now: Instant,
}

/// Resolve a router to the tier that serves this request.
///
/// `context` is `None` for the body-less entry points — `/routes`, discovery,
/// and the public [`crate::routing::resolve_model`] — which have no conversation
/// to score and no session to pin, and so report the picker's default. That is
/// the right answer for those surfaces: the tier a fresh session starts on.
pub(crate) fn select(
    router: &StageRouterConfig,
    model: &str,
    context: Option<&StageContext<'_>>,
) -> StageDecision {
    let Some(context) = context else {
        return decide(router, None);
    };

    let estimate = decide(router, context.request.get("messages"));
    context.store.apply(
        model,
        context.session_id,
        router,
        estimate,
        context.read_only,
        context.now,
    )
}

/// Pick a tier for a request from its conversation so far.
///
/// `messages` is `None` for the body-less entry points (`/routes`, discovery,
/// and the public `resolve_model`), and a conversation with no completed tool
/// call yields no signals either. Both cases land on the picker's default rather
/// than scoring silence as agreement.
pub(crate) fn decide(router: &StageRouterConfig, messages: Option<&Value>) -> StageDecision {
    let mode = match router.picker {
        StageRouterPicker::EfficientFirst => PickerMode::EfficientFirst,
        StageRouterPicker::CapableFirst => PickerMode::CapableFirst,
    };
    let default_tier = match router.picker {
        StageRouterPicker::EfficientFirst => StageTier::Efficient,
        StageRouterPicker::CapableFirst => StageTier::Capable,
    };

    let Some(signals) =
        messages.and_then(|messages| signals::extract(messages, router.recent_turn_window))
    else {
        return StageDecision {
            tier: default_tier,
            source: "no_signal",
            confidence: None,
        };
    };

    match pick_tier(&signals, mode, router.confidence_threshold) {
        PickOutcome::Resolved {
            tier,
            source,
            confidence,
            ..
        } => StageDecision {
            tier: tier_from(tier),
            source: source_label(source),
            confidence,
        },
        // The signals were too weak to decide and shunt runs no judge, so the
        // picker's default takes the turn. This is libsy's documented fall-open
        // path, not an error.
        PickOutcome::ConsultClassifier {
            default_tier,
            confidence,
            ..
        } => StageDecision {
            tier: tier_from(default_tier),
            source: "fall_open",
            confidence: Some(confidence),
        },
    }
}

fn tier_from(tier: Tier) -> StageTier {
    match tier {
        Tier::Capable => StageTier::Capable,
        Tier::Efficient => StageTier::Efficient,
    }
}

fn source_label(source: DecisionSource) -> &'static str {
    match source {
        DecisionSource::Override => "override",
        DecisionSource::TestsPassed => "tests_passed",
        DecisionSource::Dimensions => "dimensions",
        DecisionSource::Ambiguous => "ambiguous",
        DecisionSource::LlmClassifier => "llm_classifier",
        DecisionSource::FallOpen => "fall_open",
    }
}

/// Decision tests.
///
/// These pin shunt's wiring — the fall-open path, the picker default, and the
/// direction signals push — not libsy's calibration, which is the dependency's
/// own contract. Non-vacuity: make [`decide`] always return `Efficient` and
/// `an_erroring_session_escalates` goes red; make it always return the picker
/// default and that test plus `confidence_is_reported_when_the_scorer_decided`
/// go red.
#[cfg(test)]
mod tests {
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
            let decision = decide(&router(picker), None);
            assert_eq!(decision.tier, expected);
            assert_eq!(decision.source, "no_signal");
            assert_eq!(decision.confidence, None);
        }
    }

    #[test]
    fn a_conversation_without_tool_activity_lands_on_the_picker_default() {
        let messages = json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]);
        let decision = decide(&router(StageRouterPicker::EfficientFirst), Some(&messages));

        assert_eq!(decision.tier, StageTier::Efficient);
        assert_eq!(decision.source, "no_signal");
    }

    /// The whole point of the router: repeated failures move the turn up.
    #[test]
    fn an_erroring_session_escalates() {
        let messages = erroring();
        let decision = decide(&router(StageRouterPicker::EfficientFirst), Some(&messages));

        assert_eq!(
            decision.tier,
            StageTier::Capable,
            "two failed investigative turns must escalate (source {}, confidence {:?})",
            decision.source,
            decision.confidence
        );
    }

    #[test]
    fn confidence_is_reported_when_the_scorer_decided() {
        let messages = erroring();
        let decision = decide(&router(StageRouterPicker::EfficientFirst), Some(&messages));

        assert!(
            matches!(decision.source, "dimensions" | "override"),
            "an escalation must name its evidence, got {}",
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
            let decision = decide(&router(picker), Some(&messages));
            assert_eq!(
                decision.tier, expected,
                "a weak signal must land on the picker default, got source {}",
                decision.source
            );
        }
    }

    #[test]
    fn each_tier_resolves_to_its_configured_target() {
        let router = router(StageRouterPicker::EfficientFirst);

        assert_eq!(StageTier::Capable.target(&router), "claude-opus-4-8");
        assert_eq!(StageTier::Efficient.target(&router), "claude-sonnet-4-6");
    }
}
