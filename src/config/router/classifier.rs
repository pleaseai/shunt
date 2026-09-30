//! `[models.router] type = "llm_classifier"` — a judge's verdict decides the
//! turn (ADR-0005 §8 PR 5, PR 6).
//!
//! Three modes, and the mode is **required**: upstream defaults an omitted
//! `mode` to `capability` (and still accepts an `escalation` table that omits
//! it), shunt does not. The modes are not interchangeable readings of one key
//! set. `escalation` judges a *completed* weak turn before the caller sees it
//! — the retained-turn lane of PR 6, which buffers the turn it gates — while
//! `capability` decides before any answer is made. An operator who wrote one
//! mode's keys and left `mode` out would otherwise get a config that loads
//! and runs a different algorithm, with a different buffering contract, on
//! their turns; an internally tagged enum with no default makes that a load
//! error naming the three modes that exist.
//!
//! Policy only, like every other `[models.router]` table: targets are public
//! model ids and no credential can land here, so a derived `Debug` cannot leak
//! one.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::bounds::{
    default_gated_idle_ms, default_gated_max_bytes, default_gated_max_duration_ms,
    default_judge_max_response_bytes, default_judge_timeout_ms, default_max_judge_calls,
    impl_call_bounds,
};

/// How often a driven router re-decides a session's target.
///
/// shunt's own enum rather than libsy's re-exported one, for the same reason
/// every other key is shunt's: the config surface is serialized into a
/// round-trippable TOML file and read back by `shunt check`, so its
/// `snake_case` spelling and its default must be shunt's to keep, not a
/// dependency's to change. [`ClassifyTrigger::to_libsy`] is the one place the
/// two meet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyTrigger {
    /// Judge every turn that reaches the classifier, tool continuations
    /// included.
    #[default]
    EveryRequest,
    /// Judge each new human user turn and hold that answer across the tool
    /// calls between.
    UserTurn,
    /// Judge once per session and reuse the answer.
    NewSession,
}

impl ClassifyTrigger {
    /// libsy's own trigger, for the algorithm construction in
    /// [`crate::routing::driven`].
    pub fn to_libsy(self) -> switchyard_libsy::ClassifyTrigger {
        match self {
            Self::EveryRequest => switchyard_libsy::ClassifyTrigger::EveryRequest,
            Self::UserTurn => switchyard_libsy::ClassifyTrigger::UserTurn,
            Self::NewSession => switchyard_libsy::ClassifyTrigger::NewSession,
        }
    }

    /// Whether this is the default, so an omitted key round-trips as omitted
    /// rather than as an explicit `every_request`.
    pub fn is_every_request(&self) -> bool {
        matches!(self, Self::EveryRequest)
    }
}

/// libsy's own judge ceiling, and the one the live capture observed on the
/// wire (`max_tokens: 4096`).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4_096;

pub(crate) fn default_max_output_tokens() -> u64 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

/// `[models.router]` with `type = "llm_classifier"`.
///
/// `mode` is the tag and carries no default — see the module docs.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum LlmClassifierConfig {
    /// The packaged capability rubric: the judge reports a solve probability
    /// for the task and the threshold decides between two tiers.
    Capability(CapabilityClassifierConfig),
    /// An operator-supplied prompt, JSON schema, and selector over named model
    /// groups.
    Custom(CustomClassifierConfig),
    /// The weak tier answers first and a trajectory judge reads the completed
    /// turn; a confirmed streak latches the session to the strong tier. The
    /// weak turn is retained and replayed, never streamed live (ADR-0005 §4).
    Escalation(EscalationClassifierConfig),
}

/// `mode = "capability"`: two answer tiers and the packaged rubric.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityClassifierConfig {
    /// Public model id of the judge. Consulted, never served.
    pub classifier_target: String,
    /// Served when the verdict says the task is beyond the weak tier.
    pub strong_target: String,
    /// Served when the verdict's solve probability clears `base_threshold`.
    pub weak_target: String,
    /// Lowest `p_solve` that keeps a *supported* task on the weak tier.
    ///
    /// Required, following upstream: the number is the whole calibration of
    /// this mode, and a default would silently pick one deployment's operating
    /// point for every other.
    pub base_threshold: f64,
    /// Added to `base_threshold` per capability-boundary step — one step for an
    /// uncertain or unmatched verdict, two for an unsupported one.
    #[serde(default)]
    pub threshold_step: f64,
    /// Replaces the packaged rubric. Absent means the packaged one, which is
    /// what the live capture was taken against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "ClassifyTrigger::is_every_request")]
    pub classify_trigger: ClassifyTrigger,
    /// Key an unkeyed request on a hash of its first user message. Requires
    /// `classify_trigger = "new_session"` (libsy's rule, raised here at load).
    #[serde(default, skip_serializing_if = "is_false")]
    pub message_hash_fallback: bool,
    /// Trailing turns the judge sees on top of the opening task. Absent judges
    /// the opening task and the latest user follow-up alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_turn_window: Option<usize>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    /// The six per-call bounds (ADR-0005 §3), spelled as explicit fields for
    /// the same reason [`super::StageRouterConfig`] spells them: serde forbids
    /// `flatten` beside `deny_unknown_fields`.
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

/// `mode = "custom"`: named model groups, an operator's prompt and schema, and
/// a selector that reads the group name out of the verdict.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomClassifierConfig {
    /// Group served when the judge produces no usable verdict. Must name a
    /// configured group other than `judge`.
    pub default_target: String,
    /// The judge's system prompt. Must not carry `{{RESPONSE_SCHEMA}}`: libsy
    /// supplies the schema itself and rejects the placeholder.
    pub prompt: String,
    /// The verdict's JSON Schema, held as the string the operator wrote so a
    /// round-tripped config emits their formatting rather than a re-serialized
    /// value. Parsed at validation and at build.
    pub response_schema: String,
    #[serde(default, skip_serializing_if = "ClassifyTrigger::is_every_request")]
    pub classify_trigger: ClassifyTrigger,
    #[serde(default, skip_serializing_if = "is_false")]
    pub message_hash_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_turn_window: Option<usize>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
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
    // header, so a struct that declares one earlier cannot be serialized at
    // all — and this config surface must round-trip through `toml::to_string`
    // for `shunt check` and the config fingerprint.
    /// How the group name is read out of a schema-valid verdict.
    pub policy: ClassifierPolicy,
    /// Group name → the public model ids in it. `judge` and `any` are required
    /// (validation), `judge` is consulted and never served, and every other
    /// group is a destination the selector may name.
    ///
    /// A `BTreeMap` so `Serialize` — and the config fingerprint with it — is
    /// independent of how the operator ordered the inline table.
    pub models: BTreeMap<String, Vec<String>>,
}

/// `mode = "escalation"`: the weak tier answers, the judge reads the completed
/// turn, and a confirmed streak moves the session to the strong tier.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EscalationClassifierConfig {
    /// Public model id of the trajectory judge. Consulted, never served.
    pub classifier_target: String,
    /// Served once the session latches, and whenever the weak turn cannot be
    /// retained whole.
    pub strong_target: String,
    /// Answers every turn before the latch. Its turn is the gated one: made in
    /// the caller's own `stream` mode, retained, and replayed after the verdict.
    pub weak_target: String,
    /// Replaces the packaged trajectory prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
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
    // Table-valued, so last — see `CustomClassifierConfig::policy`.
    /// `[models.router.escalation]` — the streak and the transcript window.
    /// Omitted when it holds only upstream's defaults, so a table the operator
    /// never wrote does not appear in the round-tripped file.
    #[serde(default, skip_serializing_if = "EscalationJudgeTable::is_default")]
    pub escalation: EscalationJudgeTable,
}

/// `[models.router.escalation]`.
///
/// shunt's own struct rather than libsy's `EscalationJudgeConfig`, for the
/// reason [`ClassifyTrigger`] is shunt's: the defaults are part of the config
/// surface an operator reads back from `shunt check`, so they must not move
/// when the dependency does. The values mirror upstream's benchmarked ones and
/// [`EscalationJudgeTable::to_libsy`] is where the two meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EscalationJudgeTable {
    /// Consecutive escalate verdicts that latch the session. Above `1` needs a
    /// session id, since the streak is kept per session.
    pub confirmations: u32,
    /// Trailing messages the judge sees on top of the task anchors.
    pub recent_turn_window: usize,
    /// Per-message character cap inside that window.
    pub window_message_chars: usize,
}

impl Default for EscalationJudgeTable {
    fn default() -> Self {
        Self {
            confirmations: 2,
            recent_turn_window: 28,
            window_message_chars: 500,
        }
    }
}

impl EscalationJudgeTable {
    /// libsy's own settings. Its constructor is what validates them (`0`
    /// confirmations, an empty window, a sub-50 character cap), so a bad value
    /// fails `shunt check` quoting upstream's message.
    pub fn to_libsy(self) -> switchyard_libsy::EscalationJudgeConfig {
        switchyard_libsy::EscalationJudgeConfig {
            confirmations: self.confirmations,
            recent_turn_window: self.recent_turn_window,
            window_message_chars: self.window_message_chars,
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `policy` — how a schema-valid verdict becomes a group name.
///
/// One variant today, and tagged anyway: upstream's `CustomClassifierPolicy`
/// is an open enum, so a second policy is a new variant here rather than a
/// reinterpretation of this one's keys.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClassifierPolicy {
    /// A JSON Pointer resolved against the verdict; its string value is the
    /// group name.
    TargetSelector { selector: String },
}

impl_call_bounds!(CapabilityClassifierConfig);
impl_call_bounds!(CustomClassifierConfig);
impl_call_bounds!(EscalationClassifierConfig);

/// The group whose members a custom classifier consults rather than serves.
pub(crate) const JUDGE_GROUP: &str = "judge";
/// The config key that names that group, spelled out so the two `named_judges`
/// accessors can borrow it instead of formatting one per call. The assertion
/// below keeps it tied to [`JUDGE_GROUP`] if that group is ever renamed.
pub(crate) const JUDGE_GROUP_KEY: &str = "models.judge";
const _: () = assert!(
    JUDGE_GROUP_KEY.len() == "models.".len() + JUDGE_GROUP.len(),
    "JUDGE_GROUP_KEY must stay `models.<JUDGE_GROUP>`"
);
/// The group every other group's members must belong to (upstream's rule: the
/// runtime model list is `any`).
pub(crate) const ANY_GROUP: &str = "any";

impl LlmClassifierConfig {
    /// Every public model id a client turn can land on, paired with the key
    /// that named it.
    ///
    /// Custom groups key as `models.<group>`, which is why
    /// [`super::RouterConfig::named_targets`] returns a `Cow` rather than the
    /// `&'static str` it used to: a group name is the operator's, so only that
    /// arm owns its key.
    pub fn named_targets(&self) -> Vec<(Cow<'static, str>, &str)> {
        match self {
            Self::Capability(capability) => vec![
                (
                    Cow::Borrowed("strong_target"),
                    capability.strong_target.as_str(),
                ),
                (
                    Cow::Borrowed("weak_target"),
                    capability.weak_target.as_str(),
                ),
            ],
            Self::Custom(custom) => custom
                .models
                .iter()
                .filter(|(group, _)| group.as_str() != JUDGE_GROUP)
                .flat_map(|(group, ids)| {
                    ids.iter()
                        .map(move |id| (Cow::Owned(format!("models.{group}")), id.as_str()))
                })
                .collect(),
            Self::Escalation(escalation) => vec![
                (
                    Cow::Borrowed("strong_target"),
                    escalation.strong_target.as_str(),
                ),
                (
                    Cow::Borrowed("weak_target"),
                    escalation.weak_target.as_str(),
                ),
            ],
        }
    }

    /// Every id this router consults and never serves.
    pub fn named_judges(&self) -> Vec<(Cow<'static, str>, &str)> {
        match self {
            Self::Capability(capability) => vec![(
                Cow::Borrowed("classifier_target"),
                capability.classifier_target.as_str(),
            )],
            Self::Custom(custom) => custom
                .models
                .get(JUDGE_GROUP)
                .into_iter()
                .flatten()
                .map(|id| (Cow::Borrowed(JUDGE_GROUP_KEY), id.as_str()))
                .collect(),
            Self::Escalation(escalation) => vec![(
                Cow::Borrowed("classifier_target"),
                escalation.classifier_target.as_str(),
            )],
        }
    }

    /// Where a turn goes when the judge produces no usable verdict — libsy's
    /// own `DefaultCategoryClassifier`, spelled in shunt's config so the
    /// body-less surfaces and the first pass of `resolve_chain` can answer
    /// without constructing an algorithm.
    ///
    /// Capability mode's is the **strong** target: libsy's capability route
    /// closes on `Category::Capable`, so a judge that cannot answer hands the
    /// turn to the tier that can.
    pub fn fail_open_target(&self) -> &str {
        match self {
            Self::Capability(capability) => capability.strong_target.as_str(),
            // Validation guarantees `default_target` names a configured,
            // non-empty group, so the fallback below is unreachable on a config
            // that loaded.
            Self::Custom(custom) => custom
                .models
                .get(&custom.default_target)
                .and_then(|ids| ids.first())
                .map_or(custom.default_target.as_str(), String::as_str),
            // The weak tier, unlike capability mode: an escalation judge that
            // cannot answer leaves the weak turn standing (libsy serves it with
            // `fail_open` evidence), and a turn that has made no call yet has
            // nothing to judge. The strong tier is reached only by a verdict,
            // a latch, or a weak turn that could not be retained.
            Self::Escalation(escalation) => escalation.weak_target.as_str(),
        }
    }

    /// The six per-call bounds, whichever mode carries them.
    pub fn bounds(&self) -> super::CallBounds {
        match self {
            Self::Capability(capability) => capability.bounds(),
            Self::Custom(custom) => custom.bounds(),
            Self::Escalation(escalation) => escalation.bounds(),
        }
    }

    /// The same six, paired with the keys that wrote them.
    pub fn bound_keys(&self) -> [(&'static str, u64); 6] {
        match self {
            Self::Capability(capability) => capability.bound_keys(),
            Self::Custom(custom) => custom.bound_keys(),
            Self::Escalation(escalation) => escalation.bound_keys(),
        }
    }

    /// The trigger, whichever mode carries it. Escalation has no key: it
    /// judges every turn that is not latched, which is `every_request`.
    pub fn classify_trigger(&self) -> ClassifyTrigger {
        match self {
            Self::Capability(capability) => capability.classify_trigger,
            Self::Custom(custom) => custom.classify_trigger,
            Self::Escalation(_) => ClassifyTrigger::EveryRequest,
        }
    }

    /// Whether this mode retains the turn it gates (ADR-0005 §4) — the one
    /// property that sends a turn down the buffer-and-replay lane instead of
    /// the decide-then-stream one.
    pub fn is_gated(&self) -> bool {
        matches!(self, Self::Escalation(_))
    }
}

#[cfg(test)]
mod tests;
