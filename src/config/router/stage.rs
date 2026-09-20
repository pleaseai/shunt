//! `[models.router] type = "stage_router"` — opt-in, content-aware tier
//! selection for one advertised model id.
//!
//! A `[[models]]` entry normally names a fixed destination. An entry carrying
//! this table instead names *two* destinations — a capable tier and an efficient
//! one — and lets the request's recent tool-result history decide which serves
//! the turn. The chosen target is itself a public model id, so it resolves
//! through the ordinary routing ladder and inherits failover chains, account
//! pools, adapter selection, `effort`, and `service_tier` unchanged.
//!
//! Policy only: the table holds no credentials, so a derived `Debug` cannot leak
//! one. Absent this table a `[[models]]` entry behaves exactly as before.

use serde::{Deserialize, Serialize};

use super::bounds;

/// Which tier serves a turn whose signals are too weak to decide.
///
/// The scorer reports low confidence far more often than it reports a wrong
/// answer, so this default — not the score — governs most early turns in a
/// session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageRouterPicker {
    /// Start efficient and escalate only when signals support capable.
    #[default]
    EfficientFirst,
    /// Start capable and de-escalate only when signals support efficient.
    CapableFirst,
}

/// `[models.router]` with `type = "stage_router"`.
///
/// The default thresholds are deliberately asymmetric. Escalating costs one
/// forfeited prompt-cache prefix; staying on the efficient tier through a turn
/// it cannot handle costs a wasted turn *and* the escalation afterwards. So
/// `confidence_threshold` gates the way up and the higher default
/// `deescalate_threshold` gates the way down.
///
/// That ordering is the default, not an invariant: validation ranges each
/// threshold independently, so an operator may set `deescalate_threshold`
/// *below* `confidence_threshold` and make the down direction the easier one.
/// A cost-first deployment may want exactly that, so the inverted pair loads —
/// with a warning, once per load, from `Config::warn_router_threshold_inversion`
/// (issue #562). The same call decided the neighbouring rule: two targets that
/// resolve to one id flatten both tiers onto one model, which is degenerate but
/// a real way to test, so `Config::warn_router_identical_targets` warns
/// rather than rejecting.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StageRouterConfig {
    /// Model id serving hard reasoning, investigation, and error recovery.
    pub capable_target: String,
    /// Model id serving routine work once the plan is settled.
    pub efficient_target: String,
    #[serde(default)]
    pub picker: StageRouterPicker,
    /// Minimum scorer confidence to act on a signal-driven decision.
    #[serde(default = "default_confidence_threshold")]
    pub confidence_threshold: f64,
    /// How many recent turns of tool results feed the scorer.
    #[serde(default = "default_recent_turn_window")]
    pub recent_turn_window: usize,
    /// Turns the capable tier is held before a de-escalation may fire.
    ///
    /// Counted from the turn that chose the tier, so `0` and `1` both mean "no
    /// dwell floor" — de-escalation then rests on `deescalate_threshold` alone.
    /// Unlike `recent_turn_window`, `0` is accepted rather than rejected: it is
    /// a degenerate but coherent setting, not a config that cannot work.
    #[serde(default = "default_min_dwell_turns")]
    pub min_dwell_turns: u32,
    /// Confidence required to move *down* to the efficient tier. Defaults to
    /// [`DEFAULT_DEESCALATE_THRESHOLD`], which is higher than the default
    /// escalation threshold but is not required to exceed a configured one;
    /// read it through [`StageRouterConfig::deescalate_threshold`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deescalate_threshold: Option<f64>,
    /// How long a session's pinned tier survives without traffic.
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
    /// Turns the capable tier is held after a signal-driven escalation, during
    /// which a de-escalation is refused however convincing it is.
    ///
    /// Upstream's default is `2`; shunt's is **`0`**, so an existing pin behaves
    /// exactly as it did before this key existed. shunt already prices the same
    /// churn twice — `min_dwell_turns` and the higher `deescalate_threshold` —
    /// and layering a third gate on by default would change every shipped
    /// deployment's de-escalation timing for a hysteresis it did not ask for.
    #[serde(default)]
    pub capable_hold_turns: u32,
    /// Operator-declared semantics for tool names the built-in vocabulary
    /// leaves as `Other` (`Bash`, `Skill`, `mcp__*`, …).
    #[serde(default, skip_serializing_if = "ToolSemanticsConfig::is_empty")]
    pub tool_semantics: ToolSemanticsConfig,
    /// A note appended to the system prompt on the turns a signal moves the
    /// tier, so the receiving model knows why it was handed the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_notes: Option<HandoffNotesConfig>,
    /// `[models.router.classifier]` — the judge consulted on turns the signals
    /// leave undecided. Its presence is what moves this entry to the driven
    /// lane (ADR-0005 §1); absent, the table is the signal-only router it has
    /// always been and costs exactly what it costs today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<StageClassifierConfig>,
    /// The six per-call bounds (ADR-0005 §3), spelled as explicit fields rather
    /// than a flattened [`CallBounds`]: serde forbids `flatten` beside
    /// `deny_unknown_fields`, and this table needs the latter. Read them
    /// through [`StageRouterConfig::bounds`].
    #[serde(default = "default_judge_timeout_ms")]
    pub judge_timeout_ms: u64,
    #[serde(default = "default_judge_max_response_bytes")]
    pub judge_max_response_bytes: usize,
    #[serde(default = "default_gated_max_bytes")]
    pub gated_max_bytes: usize,
    #[serde(default = "default_gated_idle_ms")]
    pub gated_idle_ms: u64,
    #[serde(default = "default_gated_max_duration_ms")]
    pub gated_max_duration_ms: u64,
    #[serde(default = "default_max_judge_calls")]
    pub max_judge_calls: u32,
}

/// `[models.router.classifier]` — the judge a stage router consults when its
/// signals cannot decide a turn.
///
/// The target is a public model id like any other, so the judge inherits
/// failover, pools, adapter, `effort`, and `service_tier` from the entry it
/// names, and sits on its own quota by pointing at an entry that maps one
/// (ADR-0005 §2). It is **consulted, never served**: it appears in `/routes`
/// under `judges`, not `targets`, and no client request is ever routed to it.
///
/// Policy only, like its parent table: no credential can land here, so a
/// derived `Debug` cannot leak one.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StageClassifierConfig {
    /// Public model id of the judge. Consulted, never served.
    pub target: String,
    /// Lowest `p_solve` that keeps a *supported* task on the efficient tier —
    /// libsy's `TaskClassifierConfig::base_threshold`. Range-checked exactly
    /// like `confidence_threshold`.
    #[serde(default = "default_base_threshold")]
    pub base_threshold: f64,
}

/// libsy's own capability-classifier calibration point, and the same number
/// [`DEFAULT_CONFIDENCE_THRESHOLD`] uses for the signal scorer.
pub const DEFAULT_BASE_THRESHOLD: f64 = 0.5;

fn default_base_threshold() -> f64 {
    DEFAULT_BASE_THRESHOLD
}

fn default_judge_timeout_ms() -> u64 {
    bounds::DEFAULT_JUDGE_TIMEOUT_MS
}

fn default_judge_max_response_bytes() -> usize {
    bounds::DEFAULT_JUDGE_MAX_RESPONSE_BYTES
}

fn default_gated_max_bytes() -> usize {
    bounds::DEFAULT_GATED_MAX_BYTES
}

fn default_gated_idle_ms() -> u64 {
    bounds::DEFAULT_GATED_IDLE_MS
}

fn default_gated_max_duration_ms() -> u64 {
    bounds::DEFAULT_GATED_MAX_DURATION_MS
}

fn default_max_judge_calls() -> u32 {
    bounds::DEFAULT_MAX_JUDGE_CALLS
}

/// `[models.router.tool_semantics]` — exact tool names added to the built-in
/// vocabulary.
///
/// Additive only: a name whose *built-in* classification is Observe, Mutate, or
/// Plan cannot be reclassified, and config validation rejects the attempt
/// rather than silently ignoring it (upstream's `ToolSemantics::validate` takes
/// the same position). Matching is ASCII case-insensitive and exact, so
/// `mcp__jbcontext__code_search` is named in full and no prefix wildcard is
/// implied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSemanticsConfig {
    /// Read-only lookup or inspection. Counted as a read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub observe: Vec<String>,
    /// Changes file, task, or external state. Counted as a *write* rather than
    /// an edit: an operator-declared mutation names a tool shunt cannot see
    /// inside, so whole-file semantics is the conservative reading — and it is
    /// the one upstream's `ToolSemantics::classify` takes for the same reason.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mutate: Vec<String>,
    /// Explicit planning or delegation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<String>,
    /// Forward activity that favours neither tier, counted into libsy's
    /// `new_count`/`recent_new_count`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub new: Vec<String>,
}

impl ToolSemanticsConfig {
    /// Whether the operator declared nothing at all — the common case, and the
    /// one that must not serialize an empty table into a round-tripped config.
    pub fn is_empty(&self) -> bool {
        self.observe.is_empty()
            && self.mutate.is_empty()
            && self.plan.is_empty()
            && self.new.is_empty()
    }

    /// The four lists with their category names, in declaration order. Shared
    /// by validation and lookup so neither can enumerate a category the other
    /// does not.
    pub fn categories(&self) -> [(&'static str, &[String]); 4] {
        [
            ("observe", &self.observe),
            ("mutate", &self.mutate),
            ("plan", &self.plan),
            ("new", &self.new),
        ]
    }
}

/// `[models.router.handoff_notes]` — what the receiving tier is told when a
/// signal moves the turn.
///
/// Each toggle costs a prompt-cache miss: the note is a new system block, and
/// Anthropic prompt caching keys on the serialized prefix, so the turn that
/// adds it and the turn that drops it both write a fresh cache entry. That is
/// the price of the note and is why it fires only on the turns a signal
/// actually decided.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffNotesConfig {
    /// Handed to the capable tier on an escalation. Required: a notes table
    /// with nothing to say is a config that does nothing.
    pub escalation_note: String,
    /// Handed back to the efficient tier on a de-escalation. Absent means no
    /// note in that direction, which is the common shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deescalation_note: Option<String>,
    /// Restrict the escalation note to a *signal-driven* escalation
    /// (`override` / `dimensions`) rather than any turn that lands capable.
    ///
    /// Gating is the safe default, upstream's included: an ungated note tells
    /// the capable model the efficient one was stalling on turns where it was
    /// not — a fall-open default, or the picker's own `capable_first`.
    #[serde(default = "default_true")]
    pub only_on_wrong_signal_escalation: bool,
}

fn default_true() -> bool {
    true
}

/// Matches the stage router's published calibration: one saturated signal scores
/// `tanh(0.5) ≈ 0.46` and so falls short on its own, while two corroborating
/// signals reach `tanh(1.0) ≈ 0.76` and clear this bar.
pub const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.5;
/// Strictly above [`DEFAULT_CONFIDENCE_THRESHOLD`]: a tier flip forfeits the
/// warm prompt-cache prefix and any `previous_response_id` continuation, so
/// coming back down must clear a higher bar than going up.
pub const DEFAULT_DEESCALATE_THRESHOLD: f64 = 0.75;

fn default_confidence_threshold() -> f64 {
    DEFAULT_CONFIDENCE_THRESHOLD
}

fn default_recent_turn_window() -> usize {
    3
}

fn default_min_dwell_turns() -> u32 {
    3
}

fn default_session_ttl_seconds() -> u64 {
    3600
}

impl StageRouterConfig {
    /// The effective de-escalation threshold, applying the default when the
    /// operator left the key out.
    pub fn deescalate_threshold(&self) -> f64 {
        self.deescalate_threshold
            .unwrap_or(DEFAULT_DEESCALATE_THRESHOLD)
    }

    /// Both configured targets, in the order they are declared.
    pub fn targets(&self) -> [&str; 2] {
        [&self.capable_target, &self.efficient_target]
    }

    /// The same pair, each with the key that named it.
    pub fn named_targets(&self) -> Vec<(&'static str, &str)> {
        vec![
            ("capable_target", self.capable_target.as_str()),
            ("efficient_target", self.efficient_target.as_str()),
        ]
    }

    /// A stage table built from the two targets, a picker, and a confidence
    /// threshold, with every remaining key at its shunt default.
    ///
    /// This is what `type = "auto"` is: upstream's preset, pre-built at load so
    /// the routed path never constructs one per request.
    pub fn preset(
        capable_target: String,
        efficient_target: String,
        picker: StageRouterPicker,
        confidence_threshold: f64,
    ) -> Self {
        Self {
            capable_target,
            efficient_target,
            picker,
            confidence_threshold,
            recent_turn_window: default_recent_turn_window(),
            min_dwell_turns: default_min_dwell_turns(),
            deescalate_threshold: None,
            session_ttl_seconds: default_session_ttl_seconds(),
            capable_hold_turns: 0,
            tool_semantics: ToolSemanticsConfig::default(),
            handoff_notes: None,
            // `type = "auto"` is the two-key preset and never carries a judge:
            // its wire shape has exactly `capable_target` and
            // `efficient_target`, so a classifier here would be a table no
            // operator could have written.
            classifier: None,
            judge_timeout_ms: default_judge_timeout_ms(),
            judge_max_response_bytes: default_judge_max_response_bytes(),
            gated_max_bytes: default_gated_max_bytes(),
            gated_idle_ms: default_gated_idle_ms(),
            gated_max_duration_ms: default_gated_max_duration_ms(),
            max_judge_calls: default_max_judge_calls(),
        }
    }
}
