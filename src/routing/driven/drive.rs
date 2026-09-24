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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use axum::http::HeaderMap;
use switchyard_libsy::{drive as libsy_drive, DecisionSource, LibsyError, RoutingOutcome};
use switchyard_protocol::Request;
use switchyard_translation::{TranslationEngine, TranslationPolicy, WireFormat};

use crate::config::CallBounds;
use crate::routing::context::{libsy_metadata, RouterContext};
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
    ///
    /// `budget_exhausted` whenever the drive asked for a call and
    /// `max_judge_calls` refused it before any call was made — whatever target
    /// libsy's cascade then closed on — so the series still tells a session's
    /// ceiling apart from an ordinary fail-open (issue #648).
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
    // Scoped by the same delegation rule the metadata above carries, so an
    // agent-id-less delegated turn draws on a budget of its own rather than
    // its parent's (issue #649).
    let key = JudgeBudget::key_for(&RouterContext::from_headers(headers));
    // No pre-drive `used() >= max` check: `max_judge_calls` bounds calls, not
    // turns, and a drive that consults no judge — the `new_session` and
    // `user_turn` triggers replaying an affinity assignment — costs nothing,
    // so a spent session must still be able to replay what it paid for (issue
    // #648). The reservation inside the call closure below is the whole bound,
    // and it is taken only for a call libsy actually asks for.

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
    let notes = DriveNotes::default();
    // Borrowed once: the call closure below is an `async move`, so naming
    // `notes` inside it would move the record the label reader still needs.
    let notes = &notes;
    // The per-request allowance for a keyless drive (see `reserve`): a count
    // that lives exactly as long as this drive, so a chaining algorithm on a
    // sessionless request is bounded by `max_judge_calls` like any other.
    let request_used = AtomicU32::new(0);
    let request_used = &request_used;
    // The real deadline is inside `judge_call`, which is where the upstream
    // request can be cancelled. This outer one guards only against the
    // algorithm itself never terminating, so it is sized to what a drive can
    // legitimately spend — every call it may reserve at its full per-call
    // deadline, since a chaining algorithm (composite, subagents classifier)
    // makes them one after another — plus a second of slack, rather than a
    // second deadline competing with the per-call one (ADR-0005 §3: bounds are
    // per call). One deadline for the whole drive would cut a healthy second
    // call short whenever the first used most of its own allowance.
    let outcome = tokio::time::timeout(
        drive_deadline(bounds),
        libsy_drive(
            entry.algorithm.clone(),
            neutral,
            entry.models.clone(),
            |call| {
                // Reserved per `CallModel`, not per drive: upstream makes one
                // today, a future rev that made two must spend two, and an
                // algorithm that chains judges past the budget is refused the
                // call rather than charged for it afterwards. The reservation
                // is atomic, so two turns racing on this key cannot both take
                // the last one.
                let reserved = reserve(
                    &entry.budget,
                    key.as_ref(),
                    request_used,
                    bounds.max_judge_calls,
                );
                // Noted here, beside the reservation, rather than when the
                // call's future is first polled: the record is of what the
                // budget decided, and it decided now.
                if reserved {
                    notes.charge();
                } else {
                    notes.refuse_budget();
                }
                async move {
                    if !reserved {
                        // libsy folds a refused call into "no verdict" and
                        // closes its own cascade. Which target that lands on is
                        // the algorithm's to decide — the classifier forms
                        // close on their `default_target`, a composite on its
                        // retained tier or its picker — so this refusal reports
                        // no destination of its own, exactly as a judge that
                        // answered nothing usable does. The note is what keeps
                        // the *label* from being folded in too: without it the
                        // refusal would be counted as `invalid_reply`, blaming
                        // the judge for a call it was never asked to make.
                        return call.respond(Err(LibsyError::AlgorithmError {
                            message: "max_judge_calls is spent for this session".to_string(),
                        }));
                    }
                    judge_call(state.clone(), admitted, call, bounds, notes.failure()).await
                }
            },
        ),
    )
    .await;

    let label = |fallback: &'static str| notes.label(fallback);
    let decision = match outcome {
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
        Err(_) => return fail_open(entry, "timeout"),
    };
    // The destination is libsy's: a refused call is folded into "no verdict"
    // and the algorithm closes its own cascade, which for the classifier forms
    // is the entry's fail-open target anyway and for a composite is the
    // session's retained tier or the picker's default. Serving
    // `entry.fail_open` instead would disagree with the state libsy just kept
    // — a composite's user turn on `efficient_target` while its tool
    // continuations replay the retained tier — so only the label is shunt's.
    DrivenDecision {
        judge_outcome: notes.exhausted().unwrap_or(decision.judge_outcome),
        ..decision
    }
}

/// Reserve one judge call for this drive, against the session budget when the
/// request carries a key and against `request_used` when it does not.
///
/// Called from inside libsy's call closure, synchronously, before the call's
/// future is built — so the reservation is taken at dispatch and only for a
/// call the algorithm actually asked for.
///
/// A keyless request has nothing to accumulate under across turns, so the
/// module docs on [`JudgeBudget`] give it a per-request allowance instead —
/// and "per request" has to be enforced somewhere, or a composite or
/// subagents-classifier drive on a sessionless request would chain past
/// `max_judge_calls` while the keyed form is refused at it. `request_used` is
/// that somewhere: a counter scoped to one drive, so the ceiling reads the
/// same to both forms and only its lifetime differs.
pub(super) fn reserve(
    budget: &JudgeBudget,
    key: Option<&crate::routing::driven::budget::BudgetKey>,
    request_used: &AtomicU32,
    max: u32,
) -> bool {
    match key {
        Some(key) => budget.try_charge(Some(key), max),
        None => request_used.fetch_add(1, Ordering::Relaxed) < max,
    }
}

/// The outer guard on one whole drive: every call the budget may admit at its
/// full per-call deadline, plus a second of slack for the algorithm's own work.
///
/// Saturating, so an operator who writes both bounds at their ceiling gets
/// "effectively unbounded" rather than a panic at the first judged turn.
pub(super) fn drive_deadline(bounds: CallBounds) -> Duration {
    bounds
        .judge_timeout
        .saturating_mul(bounds.max_judge_calls)
        .saturating_add(Duration::from_secs(1))
}

/// What one drive learned about *why* it has no verdict, read back as the
/// `outcome` label once libsy has closed its cascade.
///
/// Two facts rather than one, because they answer different questions and only
/// one of them is the judge's fault. `failure` means a call was made and came
/// back unusable; `budget_refused` means the atomic reservation in the call
/// closure turned a call down and none was ever made. libsy folds both into the
/// same "no verdict", so without the second the deterministic refusal — a
/// chaining algorithm's second `CallModel` on a budget down to one, or two
/// turns of a session racing for the last one — would be reported as
/// `invalid_reply` and never as `budget_exhausted`.
///
/// A third fact, `charged`, says whether any call got past the budget at all.
/// A drive whose first call was refused made none, and *that* drive is what
/// `budget_exhausted` names whatever libsy's cascade then reported — a
/// composite that closes on a retained tier would otherwise record `retained`
/// for a turn that wanted a judge and hit the ceiling.
#[derive(Debug, Default)]
pub(super) struct DriveNotes {
    failure: Mutex<Option<JudgeFailure>>,
    budget_refused: AtomicBool,
    charged: AtomicBool,
}

impl DriveNotes {
    /// The slot [`judge_call`] records a failed call in.
    pub(super) fn failure(&self) -> &Mutex<Option<JudgeFailure>> {
        &self.failure
    }

    /// Note that `max_judge_calls` refused a call this drive asked for.
    pub(super) fn refuse_budget(&self) {
        self.budget_refused.store(true, Ordering::Relaxed);
    }

    /// Note that `max_judge_calls` admitted a call this drive asked for.
    pub(super) fn charge(&self) {
        self.charged.store(true, Ordering::Relaxed);
    }

    /// `Some("budget_exhausted")` when this drive asked for a call and the
    /// budget refused it before any call was made.
    pub(super) fn exhausted(&self) -> Option<&'static str> {
        (self.budget_refused.load(Ordering::Relaxed) && !self.charged.load(Ordering::Relaxed))
            .then_some("budget_exhausted")
    }

    /// `fallback`, unless this drive recorded something more specific: a failed
    /// call's own label first — it is the more specific fact, since a call that
    /// was made and failed is what the operator is being told about — then the
    /// budget refusal.
    pub(super) fn label(&self, fallback: &'static str) -> &'static str {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map_or_else(
                || {
                    if self.budget_refused.load(Ordering::Relaxed) {
                        "budget_exhausted"
                    } else {
                        fallback
                    }
                },
                JudgeFailure::as_label,
            )
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
