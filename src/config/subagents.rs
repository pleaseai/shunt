//! `[models.subagents]` — the delegated-work overlay on a `[[models]]` entry
//! (ADR-0005 §5, §11).
//!
//! The table sits on the entry rather than inside `[models.router]` because a
//! fixed entry has no router table, and upstream Switchyard's "passthrough with
//! subagents" is exactly a fixed entry here: the parent's turns resolve the
//! entry as they always did, and only a delegated turn — a `Task` child, a hook
//! agent, a workflow sub-agent — is diverted to the overlay's target. Parent
//! traffic never sees the table.
//!
//! Two forms. `passthrough` runs on the pure lane: the target is a function of
//! the config and the request's hint headers alone, with no store read, no pin,
//! and no judge call. `llm_classifier` (ADR-0005 §8 PR 5) classifies a child
//! **once per `(session, agent)`** and reuses that answer, so the classification
//! is paid once per delegated agent rather than once per delegated turn —
//! which is why its `classify_trigger` defaults to `new_session` and refuses
//! `user_turn` outright.

mod classifier;

pub use classifier::{SubagentsClassifierConfig, SubagentsCustomConfig};

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The `type` discriminator on `[models.subagents]`.
///
/// Internally tagged like [`crate::config::RouterConfig`], and for the same
/// reason: upstream's own `type` values and pages quote unchanged. The payload
/// carries `deny_unknown_fields` because serde strips `type` before handing the
/// rest of the table to the newtype variant.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentsConfig {
    /// A fixed target for delegated work, optionally keyed on the agent type.
    Passthrough(PassthroughSubagentsConfig),
    /// A judge picks the child's model group from the delegated prompt.
    LlmClassifier(SubagentsClassifierConfig),
}

/// `type = "passthrough"`: `target`, and the `by_type` map ADR-0005 §11 adds.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PassthroughSubagentsConfig {
    /// Where a delegated turn goes when `by_type` names no entry for its agent
    /// type — and where every delegated turn goes when the type header is
    /// absent, which behind the default hint gate is every one of them.
    pub target: String,
    /// Agent type → target, keyed on the **literal** `x-claude-code-agent-type`
    /// value and matched exactly: a built-in agent's id travels verbatim, case
    /// included (`Explore`, `Plan`, `general-purpose`, `claude`, `fork`), and a
    /// project agent arrives as `custom` — its own name is never on the wire, so
    /// `custom` is the only key such an agent can match. `teammate` is the
    /// binary's literal for an Agent Teams member and has not been observed
    /// live (`docs/notes/adr-0005-routing-live-captures.md`, fact (b)).
    ///
    /// A `BTreeMap` so `Serialize` — and with it the config fingerprint and
    /// `shunt check` output — is order-independent of how the operator wrote
    /// the inline table.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_type: BTreeMap<String, String>,
}

impl PassthroughSubagentsConfig {
    /// The target a delegated turn of `agent_type` is diverted to: the
    /// `by_type` entry matching the literal header value, else `target`.
    ///
    /// Whether the turn *is* delegated is decided by the caller
    /// ([`crate::routing::context::RouterContext::is_delegated`]); this only
    /// answers "where", never "whether", so it cannot be reached for parent,
    /// `compaction`, or `auxiliary` traffic by any path that consults it.
    ///
    /// On this form rather than on [`SubagentsConfig`]: the classifier form
    /// has no synchronous answer at all — its target comes from a judge call
    /// the request path makes after admission — so an enum-level `target_for`
    /// would have to invent one, and the caller that invented it would be the
    /// one place the overlay silently stopped classifying.
    pub fn target_for(&self, agent_type: Option<&str>) -> (&str, bool) {
        agent_type
            .and_then(|agent_type| self.by_type.get(agent_type))
            .map_or((self.target.as_str(), false), |target| {
                (target.as_str(), true)
            })
    }
}

impl SubagentsConfig {
    /// The `type` value, as the closed `algorithm` metric label for the
    /// overlay's decisions.
    ///
    /// Both forms report `"subagents"`: the label answers "which table
    /// decided", and an operator reading a dashboard split by it would find the
    /// overlay's traffic halved across two series for a difference the
    /// `x-gateway-route-source` header already reports.
    pub fn algorithm(&self) -> &'static str {
        "subagents"
    }

    /// Every public model id the overlay can name, each paired with the config
    /// key that named it (`target`, `by_type.<Type>`, or `models.<group>`), in
    /// a stable order.
    ///
    /// The one-hop rule and the unresolvable-target warning both range over
    /// this, exactly as they range over [`crate::config::RouterConfig::named_targets`].
    pub fn named_targets(&self) -> Vec<(Cow<'static, str>, &str)> {
        match self {
            Self::Passthrough(passthrough) => {
                let mut targets = Vec::with_capacity(1 + passthrough.by_type.len());
                targets.push((Cow::Borrowed("target"), passthrough.target.as_str()));
                targets.extend(passthrough.by_type.iter().map(|(agent_type, target)| {
                    (Cow::Owned(format!("by_type.{agent_type}")), target.as_str())
                }));
                targets
            }
            Self::LlmClassifier(classifier) => classifier.named_targets(),
        }
    }

    /// Every id the overlay *consults* and never serves — the `models.judge`
    /// group of the classifier form, and nothing at all for passthrough.
    ///
    /// Separate from [`SubagentsConfig::named_targets`] for the reason
    /// [`crate::config::RouterConfig::named_judges`] is separate: admission
    /// must cover a judge the overlay calls on the gateway's credential, and
    /// `/routes` must not report one as a place a client turn can land.
    pub fn named_judges(&self) -> Vec<(Cow<'static, str>, &str)> {
        match self {
            Self::Passthrough(_) => Vec::new(),
            Self::LlmClassifier(classifier) => classifier.named_judges(),
        }
    }

    /// The `by_type` keys, for validation. Empty for the classifier form,
    /// which keys on model groups rather than on agent types.
    pub fn agent_types(&self) -> impl Iterator<Item = &str> {
        let by_type = match self {
            Self::Passthrough(passthrough) => Some(&passthrough.by_type),
            Self::LlmClassifier(_) => None,
        };
        by_type
            .into_iter()
            .flat_map(|map| map.keys().map(String::as_str))
    }

    /// The passthrough payload, for the pure-lane resolver.
    pub fn passthrough(&self) -> Option<&PassthroughSubagentsConfig> {
        match self {
            Self::Passthrough(passthrough) => Some(passthrough),
            Self::LlmClassifier(_) => None,
        }
    }

    /// The classifier payload, for validation and the driven lane.
    pub fn classifier(&self) -> Option<&SubagentsClassifierConfig> {
        match self {
            Self::Passthrough(_) => None,
            Self::LlmClassifier(classifier) => Some(classifier),
        }
    }

    /// Whether resolving this overlay makes a model call.
    pub fn is_driven(&self) -> bool {
        matches!(self, Self::LlmClassifier(_))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod validation_tests;
