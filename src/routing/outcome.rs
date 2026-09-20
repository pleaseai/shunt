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
