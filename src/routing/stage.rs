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

pub(crate) mod signals;
pub(crate) mod store;
mod vocabulary;

pub(crate) use store::StageRouterStore;

use std::{cell::Cell, time::Instant};

use axum::http::HeaderMap;
use serde_json::Value;
use switchyard_libsy::{pick_tier, DecisionSource, PickOutcome, PickerMode, Tier, ToolSignals};

use crate::config::{StageRouterConfig, StageRouterPicker};
use crate::routing::context::RouterContext;

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
    /// Why this tier was chosen.
    pub source: StageSource,
    /// Scorer confidence, absent where the scorer did not decide the turn.
    pub confidence: Option<f64>,
}

/// Why a tier was chosen.
///
/// Closed, so the evidence test in [`StageSource::is_signal_evidence`] — which
/// gates whether a pinned tier may move — is checked by the compiler against
/// the producer instead of matching on a string. A renamed label or a new
/// upstream variant used to fall through to "not evidence" silently, leaving a
/// pin that simply stopped moving: no error, no log, no failing test.
///
/// The variants libsy owns are carried as its own type rather than re-spelled,
/// so adding one upstream fails to compile in the match below. Bounded metric
/// cardinality — the reason the source used to be a `&'static str` — is a
/// property of the label, not of this type, and [`StageSource::as_label`] is
/// where it is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageSource {
    /// The scorer reached this turn and stamped its own reason.
    Scorer(DecisionSource),
    /// The request carried no tool history to score, so the picker's default
    /// took the turn.
    NoSignal,
    /// A pin held the tier and this turn's estimate did not earn a move.
    Sticky,
}

impl StageSource {
    /// Whether the signals actually decided this turn, rather than a default
    /// standing in for a decision. Only evidence may move a pinned tier.
    pub(crate) fn is_signal_evidence(self) -> bool {
        match self {
            Self::Scorer(DecisionSource::Override | DecisionSource::Dimensions) => true,
            // `Ambiguous` is the scorer declining to decide, `FallOpen` is the
            // picker's default standing in, `LlmClassifier` cannot occur because
            // shunt runs no judge, and `CapableHold` is libsy's own hysteresis —
            // an earlier escalation being held, which is what `Sticky` already
            // means here — and cannot occur either, because only libsy's stateful
            // `StageClassifier` stamps it and shunt calls `pick_tier` directly.
            // None of the four is evidence.
            Self::Scorer(
                DecisionSource::Ambiguous
                | DecisionSource::LlmClassifier
                | DecisionSource::CapableHold
                | DecisionSource::FallOpen,
            ) => false,
            Self::NoSignal | Self::Sticky => false,
        }
    }

    /// The metric and header label. A closed set, so it cannot inflate label
    /// cardinality however many distinct sessions route through it.
    ///
    /// The scorer's variants delegate to libsy's own `as_str` rather than a
    /// second set of literals here — including `llm-classifier`, the one label
    /// libsy hyphenates, which the hand-written copy this replaced had spelled
    /// `llm_classifier`.
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            Self::Scorer(source) => source.as_str(),
            Self::NoSignal => "no_signal",
            Self::Sticky => "sticky",
        }
    }
}

impl StageTier {
    /// The metric and header label for this tier.
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            StageTier::Capable => "capable",
            StageTier::Efficient => "efficient",
        }
    }

    /// The configured model id this tier routes to.
    pub(crate) fn target(self, router: &StageRouterConfig) -> &str {
        match self {
            StageTier::Capable => &router.capable_target,
            StageTier::Efficient => &router.efficient_target,
        }
    }
}

/// What the router decided for one request, for observability only.
///
/// Nothing here steers routing — the chain is already resolved by the time this
/// is read. It exists because the decision is otherwise invisible downstream:
/// `Route.model` is deliberately re-stamped to the id the client asked for
/// (issue #172), so neither the response nor the resolved chain says which tier
/// served the turn or why.
#[derive(Debug, Clone)]
pub(crate) struct StageOutcome {
    /// The configured model id that carries the router — the id
    /// [`crate::routing::resolve_chain`] matched, so already past
    /// `strip_context_window_hint`.
    ///
    /// Carried rather than re-derived at the reporting site: a client-side
    /// `[1m]` suffix is stripped before the router is looked up and before the
    /// session is keyed, so a counter labelled with the raw request id would
    /// split one router's series in two and attribute one session's pin to
    /// both halves.
    pub model: String,
    /// The configured model id the chosen tier routes to.
    pub target: String,
    pub tier: StageTier,
    pub source: StageSource,
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
    /// The inbound headers, read for the `x-claude-code-*` hints
    /// ([`RouterContext`]) only once the requested id turns out to carry a
    /// router — so non-router traffic pays no header lookup at all. A caller
    /// that sends no session header is routed statelessly; a delegated turn
    /// keys to its own pin.
    pub headers: &'a HeaderMap,
    /// Set for `count_tokens`, which must reach the same tier as the real turn
    /// without recording it.
    pub read_only: bool,
    /// Request-entry clock, shared with the rest of the request's timing.
    pub now: Instant,
    /// The pin [`select`] decided this request earns, parked until the request
    /// is admitted. Routing runs before inbound auth and the managed-model
    /// policy — those need the resolved chain — so writing it here would let a
    /// request that is about to be rejected pin, evict, or steer a session it
    /// never proved it owns. [`StageContext::commit`] writes it once the
    /// request is known to be served.
    pub pending: Cell<Option<store::PendingPin>>,
    /// What the router decided, parked for the observability surfaces to read
    /// once the request is admitted. Set only when the requested id actually
    /// carries a `[models.stage_router]` table.
    pub decided: Cell<Option<StageOutcome>>,
}

impl StageContext<'_> {
    /// Write the pin this request earned. Called after inbound auth and the
    /// managed-model policy have admitted it, and a no-op for every request
    /// that earned none — a read-only `count_tokens` probe, a caller with no
    /// session header, and any id the router never looked at.
    ///
    /// Admission, not a successful upstream response, is the boundary: the tier
    /// chosen here is the tier the turn was dispatched at, and an upstream 500
    /// afterwards is not evidence that the choice was wrong.
    ///
    /// Returns the `(from, to)` tier change the write made, for the flip
    /// counter. `None` for every request that wrote nothing, and for a write
    /// that landed on the tier already pinned.
    pub(crate) fn commit(&self) -> Option<(StageTier, StageTier)> {
        self.pending
            .replace(None)
            .and_then(|pin| self.store.commit(pin, self.now))
    }
}

/// Resolve a router to the tier that serves this request.
///
/// `context` is `None` for the body-less entry points — `/routes`, discovery,
/// and the public [`crate::routing::resolve_model`] — which have no conversation
/// to score and no session to pin, and so report the picker's default. That is
/// the right answer for those surfaces: the tier the picker falls back to when
/// no signal decides.
pub(crate) fn select(
    router: &StageRouterConfig,
    model: &str,
    context: Option<&StageContext<'_>>,
) -> StageDecision {
    let Some(context) = context else {
        return decide(router, None, false);
    };

    let messages = context.request.get("messages");
    let hints = RouterContext::from_headers(context.headers);
    let applied = context.store.apply(
        model,
        &hints,
        router,
        // Scored inside `apply`, against the compaction latch the pin holds:
        // the store copies the pin out and releases its lock before calling
        // this, so a long transcript is never scored under the mutex.
        |compacted| decide(router, messages, compacted),
        context.read_only,
        context.now,
    );
    let (decision, pin) = (applied.decision, applied.pin);
    // Parked for the observability surfaces, which read it only after the
    // request is admitted — the same boundary the pin waits for, and for the
    // same reason: a rejected request neither pins nor counts.
    context.decided.set(Some(StageOutcome {
        model: model.to_string(),
        target: decision.tier.target(router).to_string(),
        tier: decision.tier,
        source: decision.source,
    }));
    // Parked, not written: see [`StageContext::pending`]. Validation forbids a
    // router whose target is itself a router, so one request reaches this line
    // at most once and no earlier pin can be dropped here.
    context.pending.set(pin);
    decision
}

/// Pick a tier for a request from its conversation so far.
///
/// `messages` is `None` for the body-less entry points (`/routes`, discovery,
/// and the public `resolve_model`), and a conversation with no completed tool
/// call yields no signals either. Both cases land on the picker's default rather
/// than scoring silence as agreement.
///
/// `compacted` is the compaction latch (ADR-0005 §11): set on the turn that
/// carries `x-claude-code-context-compacted` and read back from the session
/// pin on the turns after it. It is the one signal that comes from the headers
/// rather than the transcript, and it is fed to the scorer even when the
/// transcript has no tool activity to score — the history right after a
/// compaction is typically the summary and nothing else, and libsy's
/// compaction override must still fire on it.
pub(crate) fn decide(
    router: &StageRouterConfig,
    messages: Option<&Value>,
    compacted: bool,
) -> StageDecision {
    let mode = match router.picker {
        StageRouterPicker::EfficientFirst => PickerMode::EfficientFirst,
        StageRouterPicker::CapableFirst => PickerMode::CapableFirst,
    };
    let default_tier = match router.picker {
        StageRouterPicker::EfficientFirst => StageTier::Efficient,
        StageRouterPicker::CapableFirst => StageTier::Capable,
    };

    let extracted =
        messages.and_then(|messages| signals::extract(messages, router.recent_turn_window));
    let signals = match (extracted, compacted) {
        (Some(mut signals), _) => {
            signals.compacted = compacted;
            signals
        }
        (None, true) => ToolSignals {
            compacted: true,
            ..ToolSignals::default()
        },
        (None, false) => {
            return StageDecision {
                tier: default_tier,
                source: StageSource::NoSignal,
                confidence: None,
            }
        }
    };

    match pick_tier(&signals, mode, router.confidence_threshold) {
        PickOutcome::Resolved {
            tier,
            source,
            confidence,
            ..
        } => StageDecision {
            tier: tier_from(tier),
            source: StageSource::Scorer(source),
            confidence,
        },
        // The signals were too weak to decide and shunt runs no judge, so the
        // picker's default takes the turn. This is libsy's documented fall-open
        // path, not an error.
        //
        // The sub-threshold score libsy returns here is deliberately dropped:
        // `confidence` is documented as the scorer's confidence *in this tier*,
        // and on this arm the scorer chose no tier at all — the picker did.
        // Reporting it would let a metric read a fallback as a scored decision,
        // which is the one reading the field must never support.
        PickOutcome::ConsultClassifier { default_tier, .. } => StageDecision {
            tier: tier_from(default_tier),
            source: StageSource::Scorer(DecisionSource::FallOpen),
            confidence: None,
        },
    }
}

fn tier_from(tier: Tier) -> StageTier {
    match tier {
        Tier::Capable => StageTier::Capable,
        Tier::Efficient => StageTier::Efficient,
    }
}

#[cfg(test)]
mod tests;
