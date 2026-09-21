//! `[models.subagents] type = "llm_classifier"` — a judge picks the child's
//! model group (ADR-0005 §5, §8 PR 5).
//!
//! `mode = "custom"` is the only mode, and it is the tag rather than a plain
//! key so `mode = "capability"` is an unknown-variant load error naming the one
//! that exists. Upstream takes the same position for the same reason: the
//! capability rubric judges a *task's* difficulty against two tiers, while a
//! delegated turn is routed by what kind of work it is — which is the
//! operator's own vocabulary, not a rubric's.
//!
//! Two keys are constrained beyond what upstream's own construction checks,
//! because the identity this overlay keys on is `(session, agent)`:
//! `classify_trigger = "user_turn"` is refused (upstream's own page: "user_turn
//! is not supported for sub-agent routing"), and `message_hash_fallback` must
//! be `false` — hashing the first user message of a delegated turn would key
//! two different agents' identical opening prompts onto one assignment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::router::{
    default_gated_idle_ms, default_gated_max_bytes, default_gated_max_duration_ms,
    default_judge_max_response_bytes, default_judge_timeout_ms, default_max_judge_calls,
    default_max_output_tokens, impl_call_bounds, ClassifierPolicy, ClassifyTrigger, ANY_GROUP,
    JUDGE_GROUP,
};

/// The `mode` discriminator on a classifier-form `[models.subagents]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentsClassifierConfig {
    /// An operator-supplied prompt, JSON schema, and selector over named model
    /// groups.
    Custom(SubagentsCustomConfig),
}

/// `mode = "custom"`: the payload, keyed the same way the router-level custom
/// classifier is.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentsCustomConfig {
    /// Group served when the judge produces no usable verdict.
    pub default_target: String,
    /// The judge's system prompt. Must not carry `{{RESPONSE_SCHEMA}}`.
    pub prompt: String,
    /// The verdict's JSON Schema, held as the string the operator wrote.
    pub response_schema: String,
    /// Defaults to `new_session`, which for this overlay means once per
    /// `(session, agent)`. `user_turn` is rejected at validation.
    #[serde(default = "default_subagents_trigger")]
    pub classify_trigger: ClassifyTrigger,
    /// Must be `false`; the key exists so a config that sets it is *told* so
    /// rather than rejected as an unknown key.
    #[serde(default)]
    pub message_hash_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_turn_window: Option<usize>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    /// The six per-call bounds (ADR-0005 §3): a classifier-form overlay makes
    /// internal calls, so it is fenced exactly as a driven `[models.router]`
    /// is.
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
    // Every table-valued key last: TOML forbids a bare key after a table
    // header, so a struct that declares one earlier cannot be serialized.
    /// How the group name is read out of a schema-valid verdict.
    pub policy: ClassifierPolicy,
    /// Group name → the public model ids in it. `judge` and `any` are
    /// required; `judge` is consulted and never served.
    pub models: BTreeMap<String, Vec<String>>,
}

fn default_subagents_trigger() -> ClassifyTrigger {
    ClassifyTrigger::NewSession
}

impl_call_bounds!(SubagentsCustomConfig);

impl SubagentsClassifierConfig {
    /// The `mode = "custom"` payload. One mode today, so this is total; a
    /// second mode adds an arm here rather than a second accessor.
    pub fn custom(&self) -> &SubagentsCustomConfig {
        match self {
            Self::Custom(custom) => custom,
        }
    }

    /// Every id a delegated turn can land on, keyed `models.<group>`.
    pub fn named_targets(&self) -> Vec<(String, &str)> {
        let custom = self.custom();
        custom
            .models
            .iter()
            .filter(|(group, _)| group.as_str() != JUDGE_GROUP)
            .flat_map(|(group, ids)| {
                ids.iter()
                    .map(move |id| (format!("models.{group}"), id.as_str()))
            })
            .collect()
    }

    /// Every id the overlay consults and never serves.
    pub fn named_judges(&self) -> Vec<(String, &str)> {
        self.custom()
            .models
            .get(JUDGE_GROUP)
            .into_iter()
            .flatten()
            .map(|id| (format!("models.{JUDGE_GROUP}"), id.as_str()))
            .collect()
    }

    /// Where a delegated turn goes when the judge produces no usable verdict.
    ///
    /// Validation guarantees `default_target` names a configured, non-empty
    /// group, so the fallback below is unreachable on a config that loaded.
    pub fn fail_open_target(&self) -> &str {
        let custom = self.custom();
        custom
            .models
            .get(&custom.default_target)
            .and_then(|ids| ids.first())
            .map_or(custom.default_target.as_str(), String::as_str)
    }

    /// The `any` group — the runtime model list every other group's members
    /// must belong to.
    pub fn any_group(&self) -> Option<&Vec<String>> {
        self.custom().models.get(ANY_GROUP)
    }

    /// The six per-call bounds.
    pub fn bounds(&self) -> crate::config::CallBounds {
        self.custom().bounds()
    }

    /// The same six, paired with the keys that wrote them.
    pub fn bound_keys(&self) -> [(&'static str, u64); 6] {
        self.custom().bound_keys()
    }
}
