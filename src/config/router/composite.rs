//! `[models.router] type = "composite"` — a judge sets the stage router's
//! fall-open tier (ADR-0005 §2, §8 PR 5).
//!
//! The two halves do different jobs. The stage router scores every turn from
//! its tool-result history and serves it; the judge runs once per trigger and
//! only changes *which tier the scorer falls open to* when its signals cannot
//! decide. So a composite entry pays for a judge call per user turn or per
//! session, not per turn, and the pure-lane scorer answers everything between.
//!
//! Two keys upstream accepts are absent here on purpose. There is no `picker`
//! on `[models.router.stage]` — the judge supplies the fall-open tier, so a
//! configured picker would either be overwritten or silently fight the verdict
//! — and `classify_trigger = "every_request"` is not in
//! [`CompositeTrigger`], because upstream's `CompositeRouter::new` rejects it
//! outright. Both are unknown-key/unknown-variant load errors rather than
//! runtime surprises.

use serde::{Deserialize, Serialize};

use super::bounds::{
    default_gated_idle_ms, default_gated_max_bytes, default_gated_max_duration_ms,
    default_judge_max_response_bytes, default_judge_timeout_ms, default_max_judge_calls,
    impl_call_bounds,
};
use super::classifier::ClassifyTrigger;
use super::stage::ToolSemanticsConfig;

/// `[models.router]` with `type = "composite"`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeRouterConfig {
    /// The six per-call bounds (ADR-0005 §3). They sit on `[models.router]`
    /// rather than on the classifier sub-table so every driven type spells
    /// them in the same place.
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
    // The two sub-tables last: TOML forbids a bare key after a table header,
    // so a struct that declares one earlier cannot be serialized at all.
    /// `[models.router.classifier]` — the judge that sets the tier.
    pub classifier: CompositeClassifierConfig,
    /// `[models.router.stage]` — the scorer that serves the turns.
    pub stage: CompositeStageConfig,
}

/// `[models.router.classifier]` on a composite entry.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeClassifierConfig {
    /// Public model id of the judge. Consulted, never served.
    pub target: String,
    /// Lowest `p_solve` that sets the fall-open tier to efficient. Required,
    /// following upstream.
    pub base_threshold: f64,
    /// Required here, unlike everywhere else the key appears: the whole point
    /// of this type is that the judge runs *less* often than the scorer, and
    /// upstream refuses `every_request` on it.
    pub classify_trigger: CompositeTrigger,
    /// Key an unkeyed request on a hash of its first user message. Requires
    /// `classify_trigger = "new_session"`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub message_hash_fallback: bool,
}

/// `[models.router.stage]` on a composite entry.
///
/// A narrower table than `type = "stage_router"`: the shunt-only hysteresis
/// keys (`min_dwell_turns`, `deescalate_threshold`, `session_ttl_seconds`)
/// belong to shunt's own store, which does not run here — libsy's composite
/// router owns the tier retention itself.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeStageConfig {
    /// Model id serving hard reasoning, investigation, and error recovery.
    pub capable_target: String,
    /// Model id serving routine work once the plan is settled.
    pub efficient_target: String,
    /// How much corroboration a decisive pick needs. Required, following
    /// upstream's `StageRouterConfig`.
    pub confidence_threshold: f64,
    /// How many recent turns of tool results feed the scorer.
    #[serde(default = "default_recent_turn_window")]
    pub recent_turn_window: usize,
    /// Turns the capable tier is held after an escalation. shunt's default is
    /// `0` here for the same reason it is on `type = "stage_router"`.
    #[serde(default)]
    pub capable_hold_turns: u32,
    /// Operator-declared semantics for tool names the built-in vocabulary
    /// leaves as `Other`.
    #[serde(default, skip_serializing_if = "ToolSemanticsConfig::is_empty")]
    pub tool_semantics: ToolSemanticsConfig,
}

/// The two triggers a composite entry accepts.
///
/// Deliberately not [`ClassifyTrigger`]: `every_request` would construct an
/// algorithm upstream rejects, so it is refused by the type rather than by a
/// runtime check that would have to duplicate upstream's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositeTrigger {
    /// Judge each new human user turn and hold the tier across the tool calls
    /// between.
    UserTurn,
    /// Judge once per session and hold the tier for it.
    NewSession,
}

impl CompositeTrigger {
    /// The shared trigger enum, so validation and the build path read one type.
    pub fn as_classify_trigger(self) -> ClassifyTrigger {
        match self {
            Self::UserTurn => ClassifyTrigger::UserTurn,
            Self::NewSession => ClassifyTrigger::NewSession,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_recent_turn_window() -> usize {
    3
}

impl_call_bounds!(CompositeRouterConfig);

impl CompositeRouterConfig {
    /// Both answer tiers, each with the key that named it.
    pub fn named_targets(&self) -> Vec<(String, &str)> {
        vec![
            (
                "stage.capable_target".to_string(),
                self.stage.capable_target.as_str(),
            ),
            (
                "stage.efficient_target".to_string(),
                self.stage.efficient_target.as_str(),
            ),
        ]
    }

    /// The judge, with the key that named it.
    pub fn named_judges(&self) -> Vec<(String, &str)> {
        vec![(
            "classifier.target".to_string(),
            self.classifier.target.as_str(),
        )]
    }

    /// Where a turn goes when the judge sets no tier: the efficient one.
    ///
    /// Upstream's reading, not a shunt choice — the stage router's fall-open
    /// tier is only *raised* by a verdict, so a turn the classifier could not
    /// reach lands where the picker default already was.
    pub fn fail_open_target(&self) -> &str {
        self.stage.efficient_target.as_str()
    }
}

#[cfg(test)]
mod tests;
