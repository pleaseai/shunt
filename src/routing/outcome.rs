//! What a `[models.router]` entry decided for one request, for observability.
//!
//! Nothing here steers routing — the chain is already resolved by the time this
//! is read. It exists because the decision is otherwise invisible downstream:
//! `Route.model` is deliberately re-stamped to the id the client asked for
//! (issue #172), so neither the response nor the resolved chain says which
//! target served the turn or why.
//!
//! One type for every algorithm (ADR-0005 §7). The two headers and the
//! `shunt.router.decisions` counter are stamped from it whatever ran, so a new
//! algorithm is observable the moment it produces an outcome rather than after
//! a second, parallel reporting path is written for it.

use super::stage::{StageSource, StageTier};

/// Why a request landed on the target it did.
///
/// Closed, so the label set cannot inflate however many sessions route through
/// it: the stage arm delegates to libsy's own `DecisionSource` labels, and the
/// rest are the fixed strings ADR-0005 §7 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteSource {
    /// A stage or auto router scored the turn (or held a pin).
    Stage(StageTier, StageSource),
    /// A `random` router drew this arm for this request alone.
    Random,
    /// A `random` router under `affinity = "session"` hashed the session id to
    /// this arm, so every turn of the session lands here.
    RandomSession,
    /// libsy drove the upstream `prefill_router` algorithm for this request —
    /// either its checkpoint scored the latest text user turn, or its own
    /// user-turn affinity replayed the decision that turn earned.
    #[cfg_attr(
        not(feature = "prefill-router"),
        allow(
            dead_code,
            reason = "constructed only by the driven lane, which the feature gates; the label and the `resolve_chain` arm stay in both builds so the two do not drift"
        )
    )]
    Prefill,
    /// The drive errored, so the request was routed to the default target
    /// rather than refused. Learned routing is an optimization; a checkpoint
    /// that fails to answer must not take the conversation down with it.
    #[cfg_attr(
        not(feature = "prefill-router"),
        allow(
            dead_code,
            reason = "constructed only by the driven lane, which the feature gates; the label and the `resolve_chain` arm stay in both builds so the two do not drift"
        )
    )]
    PrefillFailOpen,
    /// No decision was driven for this resolution — a body-less
    /// `resolve_model`, discovery, or `shunt check` — so the default target
    /// answers, the same id upstream picks for a turn with no text user
    /// message.
    PrefillDefault,
    /// No judge verdict is in hand yet for a driven `llm_classifier` or
    /// `composite` entry, so the algorithm's own fail-open target answers.
    ///
    /// That is the first pass of every driven turn — `resolve_chain` runs
    /// before admission and therefore before any judge call — and the final
    /// answer for a body-less surface and a `count_tokens` probe, which make
    /// zero judge calls by design (ADR-0005 §3).
    DrivenDefault,
    /// A driven `llm_classifier` (or an `llm_classifier` overlay) decided the
    /// turn: the judge answered inside its bounds and its verdict named the
    /// target. Labelled `llm-classifier`, upstream's own hyphenated spelling,
    /// so the same decision reads identically whether it came through this
    /// lane or through the stage router's `[models.router.classifier]`.
    Driven,
    /// No verdict decided the turn, so the algorithm's own fallback answered —
    /// a judge that could not be reached, answered unparseably, or answered
    /// with nothing the selector could resolve, and on a `count_tokens`-free
    /// path a budget that was already spent.
    DrivenFailOpen,
    /// The turn replayed an answer an earlier turn of the same session and
    /// agent had already earned, so no judge was called. libsy's affinity, or
    /// a composite entry's retained tier.
    DrivenRetained,
    /// An `escalation` classifier served the weak turn it retained: the judge
    /// declined to escalate, or its escalate verdict is still short of
    /// `confirmations`. The turn is replayed, not streamed live.
    EscalationWeak,
    /// An `escalation` classifier served the strong tier because the session
    /// is latched — this turn's verdict confirmed the streak, or an earlier
    /// one already had and no call was made.
    EscalationLatch,
    /// An `escalation` classifier served the strong tier because the weak turn
    /// could not be retained whole — a gated bound crossed, the transport
    /// broke, or it ended before `message_stop` — so the judge was never
    /// asked.
    EscalationFallback,
    /// An `advisor` reviewed the retained executor turn and approved it.
    AdvisorApprove,
    /// An `advisor` review failed and `fail_open` let the retained turn
    /// through unreviewed.
    AdvisorFailOpen,
    /// The retained executor turn was not a review trigger (it called tools,
    /// or the review budget could not be reserved), so it was served as
    /// generated.
    AdvisorPass,
    /// An `advisor` discarded the retained turn and sent the executor back to
    /// work; the turn the caller receives is that second, live dispatch.
    AdvisorRedo,
    /// The `advisor`'s review budget for this scope is spent, so the executor
    /// answered live with nothing retained.
    AdvisorExhausted,
    /// A gated turn could not be served: a nonterminal advisor executor turn,
    /// an upstream error the gated call relayed, or an advisor failure under
    /// `fail_open = false`.
    GatedError,
    /// A `noop` router answered without an upstream call.
    Noop,
    /// A `[models.subagents]` overlay diverted delegated work to its `target`
    /// fallback — the agent type named no `by_type` entry, or was not sent.
    Subagent,
    /// A `[models.subagents]` overlay diverted delegated work to the `by_type`
    /// target matching its `x-claude-code-agent-type`.
    SubagentType,
}

impl RouteSource {
    /// The `x-gateway-route-source` and metric label.
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            Self::Stage(_, source) => source.as_label(),
            Self::Random => "random",
            Self::RandomSession => "random_session",
            Self::Prefill => "prefill",
            Self::PrefillFailOpen => "prefill_fail_open",
            Self::PrefillDefault => "prefill_default",
            Self::DrivenDefault => "classifier_default",
            Self::Driven => "llm-classifier",
            Self::DrivenFailOpen => "classifier_fail_open",
            Self::DrivenRetained => "classifier_retained",
            Self::EscalationWeak => "escalation_weak",
            Self::EscalationLatch => "escalation_latch",
            Self::EscalationFallback => "escalation_fallback",
            Self::AdvisorApprove => "advisor_approve",
            Self::AdvisorFailOpen => "advisor_fail_open",
            Self::AdvisorPass => "advisor_pass",
            Self::AdvisorRedo => "advisor_redo",
            Self::AdvisorExhausted => "advisor_exhausted",
            Self::GatedError => "gated_error",
            Self::Noop => "noop",
            Self::Subagent => "subagent",
            Self::SubagentType => "subagent_type",
        }
    }

    /// The `(tier, source)` label pair the shipped `shunt.stage_router.*`
    /// counters take, and `None` for every algorithm that has no tier.
    ///
    /// Those two series are kept exactly as shipped, so they must not gain rows
    /// for `random` or `noop` traffic that has no tier to report — a dashboard
    /// reading them as "stage-router decisions" would silently start counting
    /// something else.
    pub(crate) fn stage_labels(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Stage(tier, source) => Some((tier.as_label(), source.as_label())),
            Self::Random
            | Self::RandomSession
            | Self::Prefill
            | Self::PrefillFailOpen
            | Self::PrefillDefault
            | Self::DrivenDefault
            | Self::Driven
            | Self::DrivenFailOpen
            | Self::DrivenRetained
            | Self::EscalationWeak
            | Self::EscalationLatch
            | Self::EscalationFallback
            | Self::AdvisorApprove
            | Self::AdvisorFailOpen
            | Self::AdvisorPass
            | Self::AdvisorRedo
            | Self::AdvisorExhausted
            | Self::GatedError
            | Self::Noop
            | Self::Subagent
            | Self::SubagentType => None,
        }
    }
}

/// One router decision, parked on the request until it is admitted.
#[derive(Debug, Clone)]
pub(crate) struct RouterOutcome {
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
    /// The configured model id the router chose. For `noop` this is the
    /// requested id: the entry answers as itself.
    pub target: String,
    /// The `type` that decided, as a `/routes` and metric label.
    pub algorithm: &'static str,
    pub source: RouteSource,
}

/// What [`crate::routing::prefill::decide`] produced for one admitted request,
/// handed to [`crate::routing::prefill::reroute`] to move the chain off the
/// provisional default target `resolve_chain` landed it on.
///
/// Unconditional, in both builds: the type and the `reroute` step are the same
/// code either way, so only the producer is feature-gated — without the
/// feature nothing ever constructs one and the drive is skipped.
#[derive(Debug, Clone)]
pub(crate) struct PrefillDecision {
    pub target: String,
    pub source: RouteSource,
}
