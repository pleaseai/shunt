//! `[models.router] type = "advisor"` — an executor answers every turn and a
//! stronger advisor reviews the first terminal one before the caller sees it
//! (ADR-0005 §4, §8 PR 6).
//!
//! The keys are upstream's own, verbatim, so its advisor-gate page quotes
//! unchanged — including the two that are only meaningful together:
//! `gate_trigger_pattern` is required under `gate_trigger = "pattern"` and
//! refused under `no_tool_call`, where it would be silently ignored. shunt
//! raises that pair at load so the rejection names the key the operator wrote;
//! every other constructor rule (`max_reviews`, `advisor_max_tokens`,
//! `transcript_max_chars`, an uncompilable pattern) is upstream's
//! `AdvisorGate::new`, constructed and dropped at validation
//! ([`crate::routing::driven::check_buildable`]) so it fails `shunt check`
//! quoting upstream's message rather than the first gated turn.
//!
//! Opting an entry into this type opts it into buffer-and-replay: the executor
//! turn the advisor may review is retained whole and served once the verdict
//! is in, in the mode the caller asked for. Nothing else about the entry
//! streams differently.

use serde::{Deserialize, Serialize};

use super::bounds::{
    default_gated_idle_ms, default_gated_max_bytes, default_gated_max_duration_ms,
    default_judge_max_response_bytes, default_judge_timeout_ms, default_max_judge_calls,
    impl_call_bounds,
};

/// What fires a review. shunt's enum rather than libsy's `GateTrigger`, whose
/// `Pattern` variant carries the regex: on the wire the two are separate keys,
/// and folding them would make the pairing rule above unspellable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorGateTrigger {
    /// The first executor turn with no tool calls.
    #[default]
    NoToolCall,
    /// The first executor turn whose visible text matches
    /// `gate_trigger_pattern` (searched, not anchored).
    Pattern,
}

impl AdvisorGateTrigger {
    fn is_no_tool_call(&self) -> bool {
        matches!(self, Self::NoToolCall)
    }
}

/// `[models.router]` with `type = "advisor"`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorRouterConfig {
    /// Serves every client-visible turn. Its gated turn carries the caller's
    /// credential exactly as a live turn does, so it may be passthrough.
    pub executor_target: String,
    /// Reviews gated turns. Consulted, never served — a judge, and held to
    /// the judge rules (credential-injecting, one hop).
    pub advisor_target: String,
    #[serde(default, skip_serializing_if = "AdvisorGateTrigger::is_no_tool_call")]
    pub gate_trigger: AdvisorGateTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_trigger_pattern: Option<String>,
    #[serde(default = "default_max_reviews")]
    pub max_reviews: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gate_stall_turns: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gate_min_tool_results: u32,
    #[serde(default = "default_advisor_max_tokens")]
    pub advisor_max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_temperature: Option<f64>,
    #[serde(default = "default_transcript_max_chars")]
    pub transcript_max_chars: usize,
    /// An advisor that cannot answer lets the retained turn through (`true`),
    /// or fails the request with a gateway-owned `502` (`false`).
    #[serde(default = "default_fail_open")]
    pub fail_open: bool,
    /// Replaces the packaged APPROVE/REDO reviewer prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_system_prompt: Option<String>,
    /// Replaces the text put in front of a REDO plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_feedback_prefix: Option<String>,
    /// The six per-call bounds (ADR-0005 §3): `judge_*` bound the review call,
    /// `gated_*` the executor turn it reviews.
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

impl_call_bounds!(AdvisorRouterConfig);

fn default_max_reviews() -> u32 {
    1
}

fn default_advisor_max_tokens() -> u64 {
    2_048
}

fn default_transcript_max_chars() -> usize {
    200_000
}

fn default_fail_open() -> bool {
    true
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl AdvisorRouterConfig {
    /// The key-level pairing rule upstream enforces on the trigger, as the
    /// reason a load error names; `None` when the pair is well formed.
    pub(crate) fn trigger_pattern_problem(&self) -> Option<&'static str> {
        let pattern = self.gate_trigger_pattern.as_deref();
        match self.gate_trigger {
            AdvisorGateTrigger::Pattern if pattern.is_none_or(|p| p.trim().is_empty()) => {
                Some("gate_trigger = \"pattern\" requires a non-empty gate_trigger_pattern")
            }
            AdvisorGateTrigger::NoToolCall if pattern.is_some() => Some(
                "gate_trigger_pattern is only read by gate_trigger = \"pattern\"; remove it or set that trigger",
            ),
            _ => None,
        }
    }

    /// upstream's config, with every key the operator left out at upstream's
    /// own default — including the two packaged prompts, which shunt does not
    /// copy so a dependency update carries its prompt with it.
    pub(crate) fn to_libsy(&self) -> switchyard_libsy::AdvisorGateConfig {
        let defaults = switchyard_libsy::AdvisorGateConfig::default();
        switchyard_libsy::AdvisorGateConfig {
            reviewer_system_prompt: self
                .reviewer_system_prompt
                .clone()
                .unwrap_or(defaults.reviewer_system_prompt),
            redo_feedback_prefix: self
                .redo_feedback_prefix
                .clone()
                .unwrap_or(defaults.redo_feedback_prefix),
            gate_trigger: match self.gate_trigger {
                AdvisorGateTrigger::NoToolCall => switchyard_libsy::GateTrigger::NoToolCall,
                AdvisorGateTrigger::Pattern => switchyard_libsy::GateTrigger::Pattern(
                    self.gate_trigger_pattern.clone().unwrap_or_default(),
                ),
            },
            max_reviews: self.max_reviews,
            gate_stall_turns: self.gate_stall_turns,
            gate_min_tool_results: self.gate_min_tool_results,
            advisor_max_tokens: self.advisor_max_tokens,
            advisor_temperature: self.advisor_temperature,
            transcript_max_chars: self.transcript_max_chars,
            fail_open: self.fail_open,
        }
    }
}

#[cfg(test)]
mod tests;
