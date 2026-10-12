//! The drive: one judge consultation for a `[models.router] type =
//! "stage_router"` carrying a `[models.router.classifier]` (ADR-0005 §3,
//! issue #594).
//!
//! What this module guards is that a judge can only ever *add* a decision,
//! never take one away. Every path that is not a clean verdict inside its
//! bounds — a translation failure, a stalled upstream, an oversized or
//! unparseable reply, an ambiguous verdict, a driver that hangs — resolves to
//! [`JudgeOutcome::FailOpen`], and the caller keeps the tier the signal scorer
//! had already fallen open to. So the worst a misbehaving judge costs is the
//! time its own deadline allows.
//!
//! The algorithm driven here is deliberately **not** libsy's full
//! `LlmTaskClassifier`: that one wraps its judge in affinity and a
//! default-category fallback, and both are state shunt already owns. shunt's
//! pin store is the session memory, so the classifier runs with
//! `ClassifyTrigger::EveryRequest` (libsy keeps no session state of its own)
//! and the fallback is this module's own fail-open, which is the one the
//! caller can attribute.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use switchyard_libsy::{
    drive, Algorithm, Classifier, ClassifyTrigger, Driver, LibsyError, LlmClassifierConfig,
    LlmTaskClassifier, Result as LibsyResult, RoutingOutcome, RuntimeModels, State,
    TaskClassifierConfig,
};
use switchyard_protocol::{Category, ModelId, Request};
use switchyard_translation::{TranslationEngine, TranslationPolicy, WireFormat};

use crate::config::{StageClassifierConfig, StageRouterConfig};
use crate::routing::serve::{judge_call, AdmittedContext, JudgeFailure};
use crate::routing::stage::StageTier;
use crate::server::AppState;

/// The algorithm label on `shunt.router.judge_calls`, and libsy's own span
/// name for this run. `stage_router` rather than `llm_task_classifier`: the
/// series is keyed by the `[models.router] type` an operator wrote, so it
/// totals against `shunt.router.decisions` for the same entry.
pub(crate) const ALGORITHM: &str = "stage_router";

/// What one consultation produced.
///
/// `FailOpen` carries a **closed** label set — `timeout`, `oversized`,
/// `upstream_error`, `invalid_reply`, `translation_failed`, and
/// `budget_exhausted`, the last of which the caller supplies without entering
/// this module at all. Closed because it is a metric attribute: an open set
/// keyed on an error string would let one misconfigured upstream inflate the
/// cardinality of `shunt.router.judge_calls` without bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JudgeOutcome {
    /// The judge answered inside its bounds and its verdict picked a tier.
    Decided(StageTier),
    /// No usable verdict. The caller keeps the tier the scorer fell open to.
    FailOpen(&'static str),
}

impl JudgeOutcome {
    /// The `outcome` attribute on `shunt.router.judge_calls`.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Decided(_) => "decided",
            Self::FailOpen(label) => label,
        }
    }
}

/// Consult the judge for one turn.
///
/// `request` is the caller's own Anthropic Messages body. It is decoded to the
/// neutral IR here and libsy's classifier builds its judge request from that —
/// so the transcript the judge sees is the transcript the caller sent, trimmed
/// by libsy's own windowing, and no shunt-side prompt is added.
pub(crate) async fn consult(
    state: &AppState,
    admitted: &AdmittedContext<'_>,
    stage: &StageRouterConfig,
    classifier: &StageClassifierConfig,
    request: &serde_json::Value,
) -> JudgeOutcome {
    let engine = TranslationEngine::default();
    let policy = TranslationPolicy::default();
    let decoded = match engine.decode_request(WireFormat::AnthropicMessages, request, &policy) {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!(
                router = %admitted.router_id(),
                error = %error,
                "judge consultation skipped: the request did not decode to the neutral IR"
            );
            return JudgeOutcome::FailOpen("translation_failed");
        }
    };
    let neutral = Request {
        llm_request: decoded.request,
        // The host's own raw body is deliberately not carried: libsy does not
        // read it, and the judge request is built from the neutral IR alone.
        raw_request: None,
        metadata: None,
    };

    let algorithm = match JudgeOnly::new(stage, classifier) {
        Ok(algorithm) => Arc::new(algorithm) as Arc<dyn Algorithm>,
        Err(error) => {
            // A threshold config validation already range-checked, so this is
            // defensive rather than reachable — but it is libsy's construction,
            // not shunt's, and a fail-open beats an unwrap on the request path.
            tracing::warn!(
                router = %admitted.router_id(),
                error = %error,
                "judge consultation skipped: the classifier could not be constructed"
            );
            return JudgeOutcome::FailOpen("translation_failed");
        }
    };

    let capable = ModelId::from(stage.capable_target.as_str());
    let efficient = ModelId::from(stage.efficient_target.as_str());
    let models = Arc::new(RuntimeModels::new(HashMap::from([
        (
            Category::Judge,
            vec![ModelId::from(classifier.target.as_str())],
        ),
        (Category::Capable, vec![capable.clone()]),
        (Category::Efficient, vec![efficient.clone()]),
    ])));

    let bounds = stage.bounds();
    let failure: Mutex<Option<JudgeFailure>> = Mutex::new(None);
    // The real deadline is inside `judge_call`, which is where the upstream
    // request can be cancelled. This outer one guards only against the driver
    // itself never terminating, so it is the inner budget plus a second of
    // slack rather than a second deadline competing with it.
    let outcome = tokio::time::timeout(
        bounds.judge_timeout + Duration::from_secs(1),
        drive(algorithm, neutral, models, |call| {
            judge_call(state.clone(), admitted, call, bounds, &failure)
        }),
    )
    .await;

    let label = |fallback: &'static str| {
        failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map_or(fallback, JudgeFailure::as_label)
    };
    match outcome {
        Ok(Ok(outcome)) => match outcome.selected_model_ids.first() {
            Some(selected) if *selected == capable => JudgeOutcome::Decided(StageTier::Capable),
            Some(selected) if *selected == efficient => JudgeOutcome::Decided(StageTier::Efficient),
            // The judge answered but the verdict named nothing this router can
            // serve — an ambiguous or invalid reply libsy already declined to
            // act on. `invalid_reply` unless a call recorded a sharper reason.
            _ => JudgeOutcome::FailOpen(label("invalid_reply")),
        },
        Ok(Err(_)) => JudgeOutcome::FailOpen(label("invalid_reply")),
        Err(_) => JudgeOutcome::FailOpen("timeout"),
    }
}

/// libsy's capability classifier with nothing around it.
///
/// `LlmTaskClassifier` as an `Algorithm` runs a cascade: affinity first, the
/// judge second, a default-category classifier last. shunt wants only the
/// middle one — affinity would be a second session memory beside the pin store
/// with its own lifetime, and the default-category close would return the
/// capable tier on *every* failure, which is a policy this gateway makes
/// itself (and makes differently: the scorer's fall-open tier, whichever it
/// is). `LlmTaskClassifier::score` delegates to the bare judge, so this wrapper
/// reaches it without reimplementing any of it.
struct JudgeOnly {
    classifier: LlmTaskClassifier,
}

impl JudgeOnly {
    fn new(stage: &StageRouterConfig, classifier: &StageClassifierConfig) -> LibsyResult<Self> {
        Ok(Self {
            classifier: LlmTaskClassifier::new(LlmClassifierConfig::Capability {
                config: TaskClassifierConfig {
                    base_threshold: classifier.base_threshold,
                    // Every request, so libsy keeps no session state of its
                    // own: shunt's pin store already decides when a session
                    // re-consults, and two memories with different lifetimes
                    // would disagree the moment one of them expired.
                    classify_trigger: ClassifyTrigger::EveryRequest,
                    // The same window the signal scorer reads, so the judge is
                    // shown the conversation the scorer failed to decide on
                    // rather than a different slice of it.
                    recent_turn_window: Some(stage.recent_turn_window),
                    ..TaskClassifierConfig::default()
                },
            })?,
        })
    }
}

#[async_trait]
impl Algorithm for JudgeOnly {
    fn name(&self) -> &str {
        ALGORITHM
    }

    async fn route(
        self: Arc<Self>,
        driver: Driver,
        mut request: Request,
    ) -> LibsyResult<RoutingOutcome> {
        let mut state = State::default();
        let (classification, _) = self
            .classifier
            .score(&mut state, &mut request, &driver)
            .await?;
        // `argmax(false)` — an `Ambiguous` classification yields nothing, which
        // is the judge declining to decide. That must reach the caller as a
        // fail-open, not as the top ambiguous score acted on anyway.
        match classification.argmax(false)? {
            Some(score) => Ok(RoutingOutcome::route_to(score.target, vec![], request)),
            None => Err(LibsyError::AlgorithmError {
                message: "judge verdict unavailable".to_string(),
            }),
        }
    }
}
