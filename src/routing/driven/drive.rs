//! One drive of a built [`DrivenEntry`] for one admitted request
//! (ADR-0005 §8 PR 5).
//!
//! The shape is [`crate::routing::judge::consult`]'s — decode the caller's own
//! Anthropic body to the neutral IR, hand it to libsy, serve whatever calls it
//! asks for through [`crate::routing::serve::judge_call`], and bound the whole
//! run — with one difference that runs all the way through: this lane does not
//! own the fallback. libsy's algorithm closes its own cascade, so the answer
//! here is always a target *and* the evidence that says why, and shunt's job
//! is to read that evidence rather than to invent a policy beside it. The
//! reading is the table in [the module docs](super).

use std::sync::Mutex;
use std::time::Duration;

use axum::http::HeaderMap;
use switchyard_libsy::{drive as libsy_drive, DecisionSource, RoutingOutcome};
use switchyard_protocol::Request;
use switchyard_translation::{TranslationEngine, TranslationPolicy, WireFormat};

use crate::routing::context::libsy_metadata;
use crate::routing::driven::{budget::JudgeBudget, DrivenEntry};
use crate::routing::outcome::RouteSource;
use crate::routing::serve::{judge_call, AdmittedContext, JudgeFailure};
use crate::routing::stage::{StageSource, StageTier};
use crate::server::AppState;

/// What one drive produced: where the turn goes, why, and the label the
/// `shunt.router.judge_calls` series records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrivenDecision {
    pub target: String,
    pub source: RouteSource,
    /// The closed `outcome` label — `decided`, `retained`, `budget_exhausted`,
    /// or the [`JudgeFailure`] the call recorded.
    pub judge_outcome: &'static str,
}

/// Drive `entry` for one admitted request.
///
/// Never fails: every path that is not a verdict inside its bounds lands on
/// the entry's own `fail_open` target, because a judge is an optimization and
/// refusing the caller's turn over one would be worse than routing without it.
pub(crate) async fn drive(
    state: &AppState,
    admitted: &AdmittedContext<'_>,
    entry: &DrivenEntry,
    request: &serde_json::Value,
    headers: &HeaderMap,
) -> DrivenDecision {
    let metadata = libsy_metadata(headers);
    let key = JudgeBudget::key(metadata.session_id.as_deref(), metadata.agent_id.as_deref());
    // Checked before the call is charged, so the budget is a ceiling on calls
    // made rather than on calls attempted — the same reading the stage lane's
    // budget has.
    if entry.budget.used(key.as_ref()) >= entry.bounds.max_judge_calls {
        return fail_open(entry, "budget_exhausted");
    }

    let engine = TranslationEngine::default();
    let policy = TranslationPolicy::default();
    let decoded = match engine.decode_request(WireFormat::AnthropicMessages, request, &policy) {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!(
                router = %admitted.router_id(),
                error = %error,
                "driven routing skipped: the request did not decode to the neutral IR"
            );
            return fail_open(entry, "translation_failed");
        }
    };
    let neutral = Request {
        llm_request: decoded.request,
        // The host's own raw body is deliberately not carried: libsy does not
        // read it, and the judge request is built from the neutral IR alone.
        raw_request: None,
        metadata: Some(metadata),
    };

    let bounds = entry.bounds;
    let failure: Mutex<Option<JudgeFailure>> = Mutex::new(None);
    // The real deadline is inside `judge_call`, which is where the upstream
    // request can be cancelled. This outer one guards only against the
    // algorithm itself never terminating, so it is the inner budget plus a
    // second of slack rather than a second deadline competing with it.
    let outcome = tokio::time::timeout(
        bounds.judge_timeout + Duration::from_secs(1),
        libsy_drive(
            entry.algorithm.clone(),
            neutral,
            entry.models.clone(),
            |call| {
                // Charged per `CallModel`, not per drive: upstream makes one
                // today, and a future rev that made two must spend two.
                entry.budget.charge(key.as_ref());
                judge_call(state.clone(), admitted, call, bounds, &failure)
            },
        ),
    )
    .await;

    let label = |fallback: &'static str| {
        failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map_or(fallback, JudgeFailure::as_label)
    };
    match outcome {
        Ok(Ok(outcome)) => decide(entry, &outcome, &label),
        // libsy folds a failed call into "no verdict" and answers from its own
        // default, so an `Err` here is the algorithm itself refusing to route
        // — no targets, or a construction it could not complete.
        Ok(Err(error)) => {
            tracing::warn!(
                router = %admitted.router_id(),
                error = %error,
                "driven routing produced no outcome; serving the fail-open target"
            );
            fail_open(entry, label("invalid_reply"))
        }
        Err(_) => fail_open(entry, "timeout"),
    }
}

/// Read one successful [`RoutingOutcome`] into a decision.
pub(super) fn decide(
    entry: &DrivenEntry,
    outcome: &RoutingOutcome,
    label: &dyn Fn(&'static str) -> &'static str,
) -> DrivenDecision {
    let Some(selected) = outcome.selected_model_ids.first() else {
        return fail_open(entry, label("invalid_reply"));
    };
    let selected = selected.to_string();
    if !entry.targets.contains(&selected) {
        // Defensive: libsy selects from the groups it was built with. Routing
        // to an id that is not one of them would resolve through
        // `server.default_provider` as a literal upstream model name, which is
        // a silently wrong destination rather than a loud one.
        tracing::warn!(
            algorithm = entry.algorithm_label,
            "the driven router selected a target outside its configured set; serving the fail-open target"
        );
        return fail_open(entry, label("invalid_reply"));
    }
    let evidence = outcome
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.evidence.as_ref())
        .and_then(|evidence| evidence.get("source"))
        .and_then(serde_json::Value::as_str);
    let source = source_for(entry, evidence, &selected);
    let judge_outcome = match source {
        RouteSource::DrivenRetained => "retained",
        RouteSource::DrivenFailOpen => label("invalid_reply"),
        _ => "decided",
    };
    DrivenDecision {
        target: selected,
        source,
        judge_outcome,
    }
}

/// The evidence → [`RouteSource`] map documented in [the module docs](super).
///
/// `entry.tiers` is what tells the two readings apart: a composite entry's
/// turns are *served* by libsy's stage router, so its `DecisionSource` labels
/// must keep landing in the `shunt.stage_router.*` series they have always
/// been counted in, while the same strings on a classifier entry are the
/// classifier's own and have no tier to report.
pub(super) fn source_for(
    entry: &DrivenEntry,
    evidence: Option<&str>,
    selected: &str,
) -> RouteSource {
    let composite_tier = || {
        entry.tiers.as_ref().map(|(capable, _)| {
            if selected == capable {
                StageTier::Capable
            } else {
                StageTier::Efficient
            }
        })
    };
    match evidence {
        // Upstream's affinity, and a composite's retained tier: the turn
        // replayed an earlier verdict and made no call of its own.
        Some("retained") => RouteSource::DrivenRetained,
        // The judge answered with nothing usable, or could not be reached.
        Some("fail_open") => RouteSource::DrivenFailOpen,
        // `DefaultCategoryClassifier` closing a classifier cascade is a
        // fail-open; the same string from libsy's stage router is its picker
        // default, which is a tier.
        Some("fall_open") => match composite_tier() {
            Some(tier) => RouteSource::Stage(tier, StageSource::Scorer(DecisionSource::FallOpen)),
            None => RouteSource::DrivenFailOpen,
        },
        other => match (other.and_then(decision_source), composite_tier()) {
            (Some(source), Some(tier)) => RouteSource::Stage(tier, StageSource::Scorer(source)),
            // A classifier verdict, and every evidence string this build does
            // not know: the algorithm decided, and `llm-classifier` is what it
            // decided as.
            _ => RouteSource::Driven,
        },
    }
}

/// libsy's own `DecisionSource` labels, parsed back.
///
/// Upstream publishes `as_str` and no inverse, so this is the inverse — spelt
/// exhaustively rather than as a `_ => None` over a guessed set, so a renamed
/// upstream label shows up as an unmapped string here rather than as a
/// silently different header.
fn decision_source(label: &str) -> Option<DecisionSource> {
    let source = match label {
        "override" => DecisionSource::Override,
        "capable_hold" => DecisionSource::CapableHold,
        "dimensions" => DecisionSource::Dimensions,
        "ambiguous" => DecisionSource::Ambiguous,
        "llm-classifier" => DecisionSource::LlmClassifier,
        "fall_open" => DecisionSource::FallOpen,
        _ => return None,
    };
    Some(source)
}

fn fail_open(entry: &DrivenEntry, judge_outcome: &'static str) -> DrivenDecision {
    DrivenDecision {
        target: entry.fail_open.clone(),
        source: RouteSource::DrivenFailOpen,
        judge_outcome,
    }
}
