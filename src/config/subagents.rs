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
//! Only the `passthrough` form lands here. It runs on the pure lane: the target
//! is a function of the config and the request's hint headers alone, with no
//! store read, no pin, and no judge call. The `llm_classifier` form is ADR-0005
//! §8 PR 5 and is rejected at load until then, by the enum's own unknown-variant
//! error naming the one form that exists.

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

impl SubagentsConfig {
    /// The `type` value, as the closed `algorithm` metric label for the
    /// overlay's decisions. There is one form today; the classifier form will
    /// add its own arm rather than reuse this label.
    pub fn algorithm(&self) -> &'static str {
        match self {
            Self::Passthrough(_) => "subagents",
        }
    }

    /// Every public model id the overlay can name, each paired with the config
    /// key that named it (`target`, or `by_type.<Type>`), in a stable order.
    ///
    /// The one-hop rule and the unresolvable-target warning both range over
    /// this, exactly as they range over [`crate::config::RouterConfig::named_targets`].
    pub fn named_targets(&self) -> Vec<(String, &str)> {
        match self {
            Self::Passthrough(passthrough) => {
                let mut targets = Vec::with_capacity(1 + passthrough.by_type.len());
                targets.push(("target".to_string(), passthrough.target.as_str()));
                targets.extend(passthrough.by_type.iter().map(|(agent_type, target)| {
                    (format!("by_type.{agent_type}"), target.as_str())
                }));
                targets
            }
        }
    }

    /// The `by_type` keys, for validation.
    pub fn agent_types(&self) -> impl Iterator<Item = &str> {
        match self {
            Self::Passthrough(passthrough) => passthrough.by_type.keys().map(String::as_str),
        }
    }

    /// The target a delegated turn of `agent_type` is diverted to: the
    /// `by_type` entry matching the literal header value, else `target`.
    ///
    /// Whether the turn *is* delegated is decided by the caller
    /// ([`crate::routing::context::RouterContext::is_delegated`]); this only
    /// answers "where", never "whether", so it cannot be reached for parent,
    /// `compaction`, or `auxiliary` traffic by any path that consults it.
    pub fn target_for(&self, agent_type: Option<&str>) -> (&str, bool) {
        match self {
            Self::Passthrough(passthrough) => agent_type
                .and_then(|agent_type| passthrough.by_type.get(agent_type))
                .map_or((passthrough.target.as_str(), false), |target| {
                    (target.as_str(), true)
                }),
        }
    }
}

/// Table-shape tests.
///
/// Non-vacuity: drop `deny_unknown_fields` from [`PassthroughSubagentsConfig`]
/// and `a_stray_key_is_rejected` goes red; swap the `map_or` arms in
/// [`SubagentsConfig::target_for`] and `by_type_beats_target_only_on_an_exact_key`
/// goes red on both assertions.
#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<SubagentsConfig, toml::de::Error> {
        toml::from_str(toml)
    }

    /// The §11 example, verbatim.
    #[test]
    fn the_adr_example_parses() {
        let overlay = parse(
            r#"
            type = "passthrough"
            target = "claude-haiku-4-5"
            by_type = { Explore = "claude-haiku-4-5", fork = "claude-sonnet-4-6", teammate = "claude-sonnet-4-6" }
            "#,
        )
        .expect("the ADR example is well formed");

        let SubagentsConfig::Passthrough(passthrough) = &overlay;
        assert_eq!(passthrough.target, "claude-haiku-4-5");
        assert_eq!(passthrough.by_type.len(), 3);
        assert_eq!(overlay.algorithm(), "subagents");
        assert_eq!(
            overlay.named_targets(),
            vec![
                ("target".to_string(), "claude-haiku-4-5"),
                ("by_type.Explore".to_string(), "claude-haiku-4-5"),
                ("by_type.fork".to_string(), "claude-sonnet-4-6"),
                ("by_type.teammate".to_string(), "claude-sonnet-4-6"),
            ]
        );
    }

    /// `by_type` is optional: `target` alone is the whole upstream form.
    #[test]
    fn by_type_is_optional() {
        let overlay = parse("type = \"passthrough\"\ntarget = \"claude-haiku-4-5\"")
            .expect("target alone is well formed");

        assert_eq!(
            overlay.target_for(Some("Explore")),
            ("claude-haiku-4-5", false)
        );
        assert_eq!(overlay.target_for(None), ("claude-haiku-4-5", false));
    }

    /// The classifier form is a later PR; naming it must fail at load rather
    /// than silently pass delegated turns through to the parent's destination.
    #[test]
    fn the_classifier_form_is_rejected_by_name() {
        let error = parse("type = \"llm_classifier\"\nmode = \"custom\"").unwrap_err();

        assert!(
            error.to_string().contains("llm_classifier")
                && error.to_string().contains("passthrough"),
            "the error names the rejected type and the one that exists: {error}"
        );
    }

    #[test]
    fn a_stray_key_is_rejected() {
        let error = parse("type = \"passthrough\"\ntarget = \"x\"\ntargets = [\"x\"]").unwrap_err();

        assert!(error.to_string().contains("targets"), "{error}");
    }

    #[test]
    fn a_missing_type_is_rejected() {
        assert!(parse("target = \"x\"").is_err());
    }

    /// Keys are the header's literal values: `Explore` matches, `explore` does
    /// not, and an unlisted type takes `target`.
    #[test]
    fn by_type_beats_target_only_on_an_exact_key() {
        let overlay = parse(
            r#"
            type = "passthrough"
            target = "fallback"
            by_type = { Explore = "explore-target", custom = "custom-target" }
            "#,
        )
        .unwrap();

        assert_eq!(
            overlay.target_for(Some("Explore")),
            ("explore-target", true)
        );
        assert_eq!(overlay.target_for(Some("explore")), ("fallback", false));
        assert_eq!(overlay.target_for(Some("custom")), ("custom-target", true));
        assert_eq!(
            overlay.target_for(Some("general-purpose")),
            ("fallback", false)
        );
        assert_eq!(overlay.target_for(None), ("fallback", false));
    }
}

/// The load-time rules, against a whole `Config`.
///
/// Non-vacuity: drop `names_a_policy`'s `subagents` arm and
/// `a_router_target_that_carries_an_overlay_is_rejected` goes red; skip
/// `validate_subagents` from `validate` and every rejection here goes green
/// for the wrong reason — which is why each asserts on the variant, not on
/// `is_err`.
#[cfg(test)]
mod validation_tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{Config, ConfigError, ModelConfig, RouterConfig};

    fn overlay(toml: &str) -> SubagentsConfig {
        toml::from_str(toml).expect("the overlay parses")
    }

    fn router() -> RouterConfig {
        serde_json::from_value(serde_json::json!({
            "type": "auto",
            "capable_target": "capable",
            "efficient_target": "efficient",
        }))
        .expect("the router parses")
    }

    fn entry(id: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            display_name: None,
            upstream_model: None,
            router: None,
            subagents: None,
            stage_router: None,
        }
    }

    fn mapped(id: &str) -> ModelConfig {
        ModelConfig {
            upstream_model: Some(BTreeMap::from([(
                "anthropic".to_string(),
                "upstream".to_string(),
            )])),
            ..entry(id)
        }
    }

    fn validate(models: Vec<ModelConfig>) -> Result<Config, ConfigError> {
        Config {
            models,
            ..Config::default()
        }
        .validate()
    }

    /// Upstream's "passthrough with subagents" is a fixed entry; the overlay
    /// also rides a router-backed one, and the two targets resolve as plain ids.
    #[test]
    fn the_overlay_rides_a_fixed_entry_and_a_routed_one() {
        let mut fixed = mapped("claude-main");
        fixed.subagents = Some(overlay(
            "type = \"passthrough\"\ntarget = \"child\"\nby_type = { Explore = \"explorer\" }",
        ));
        let mut routed = entry("claude-auto");
        routed.router = Some(router());
        routed.subagents = Some(overlay("type = \"passthrough\"\ntarget = \"child\""));

        validate(vec![fixed, routed, mapped("child"), mapped("explorer")])
            .expect("both hosts are well formed");
    }

    #[test]
    fn an_overlay_target_that_carries_a_router_is_rejected() {
        let mut host = mapped("claude-main");
        host.subagents = Some(overlay("type = \"passthrough\"\ntarget = \"claude-auto\""));
        let mut routed = entry("claude-auto");
        routed.router = Some(router());

        let error = validate(vec![host, routed]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::SubagentsRecursion { model, key, target }
                if model == "claude-main" && key == "target" && target == "claude-auto"),
            "{error}"
        );
    }

    /// The hint is stripped before ids are compared, so `"<overlay>[1m]"` is
    /// the same recursion and names the `by_type` key that wrote it.
    #[test]
    fn a_by_type_target_that_carries_an_overlay_is_rejected() {
        let mut host = mapped("claude-main");
        host.subagents = Some(overlay(
            "type = \"passthrough\"\ntarget = \"child\"\nby_type = { Explore = \"claude-other[1m]\" }",
        ));
        let mut other = mapped("claude-other");
        other.subagents = Some(overlay("type = \"passthrough\"\ntarget = \"child\""));

        let error = validate(vec![host, other, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::SubagentsRecursion { key, target, .. }
                if key == "by_type.Explore" && target == "claude-other[1m]"),
            "{error}"
        );
    }

    /// The one-hop rule in the other direction: a router may not target an
    /// entry that carries an overlay, or the overlay would never run.
    #[test]
    fn a_router_target_that_carries_an_overlay_is_rejected() {
        let mut routed = entry("claude-auto");
        routed.router = Some(router());
        let mut capable = mapped("capable");
        capable.subagents = Some(overlay("type = \"passthrough\"\ntarget = \"child\""));

        let error =
            validate(vec![routed, capable, mapped("efficient"), mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::RouterRecursion { model, target }
                if model == "claude-auto" && target == "capable"),
            "{error}"
        );
    }

    #[test]
    fn a_padded_by_type_key_is_rejected() {
        let mut host = mapped("claude-main");
        host.subagents = Some(overlay(
            "type = \"passthrough\"\ntarget = \"child\"\nby_type = { \"Explore \" = \"child\" }",
        ));

        let error = validate(vec![host, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::InvalidSubagentsType { key, .. } if key == "Explore "),
            "{error}"
        );
    }

    /// Interior whitespace, not just padding: no agent type on the wire carries
    /// a space, so `"general purpose"` could never match and must be rejected
    /// at load rather than sit in the map looking configured.
    ///
    /// Non-vacuity: narrow the predicate back to `trim() != key` and this goes
    /// red while `a_padded_by_type_key_is_rejected` stays green.
    #[test]
    fn a_by_type_key_with_interior_whitespace_is_rejected() {
        let mut host = mapped("claude-main");
        host.subagents = Some(overlay(
            "type = \"passthrough\"\ntarget = \"child\"\nby_type = { \"general purpose\" = \"child\" }",
        ));

        let error = validate(vec![host, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::InvalidSubagentsType { key, .. } if key == "general purpose"),
            "{error}"
        );
    }

    /// A key outside visible ASCII can never match either, for a different
    /// reason than whitespace: `HeaderValue::to_str` refuses the whole value,
    /// so `RouterContext::from_headers` reads `agent_type` as absent and the
    /// turn silently takes `target`. The operator meant some type, so reject it
    /// at load like the blank and whitespace cases.
    ///
    /// Non-vacuity: drop the visible-ASCII byte check in `validate_subagents`
    /// and this goes red while `a_by_type_key_with_interior_whitespace_is_rejected`
    /// stays green.
    #[test]
    fn a_by_type_key_outside_visible_ascii_is_rejected() {
        let mut host = mapped("claude-main");
        host.subagents = Some(overlay(
            "type = \"passthrough\"\ntarget = \"child\"\nby_type = { \"é\" = \"child\" }",
        ));

        let error = validate(vec![host, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::InvalidSubagentsType { key, .. } if key == "é"),
            "{error}"
        );
    }

    #[test]
    fn a_blank_target_is_rejected_by_key() {
        let mut host = mapped("claude-main");
        host.subagents = Some(overlay(
            "type = \"passthrough\"\ntarget = \"child\"\nby_type = { Explore = \" \" }",
        ));

        let error = validate(vec![host, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::EmptySubagentsTarget { key, .. } if key == "by_type.Explore"),
            "{error}"
        );
    }

    #[test]
    fn an_overlaid_id_ending_in_a_context_window_hint_is_rejected() {
        let mut host = entry("claude-main[1m]");
        host.subagents = Some(overlay("type = \"passthrough\"\ntarget = \"child\""));

        let error = validate(vec![host, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::SubagentsContextWindowHint { model } if model == "claude-main[1m]"),
            "{error}"
        );
    }

    /// An overlay names a routing policy, so a duplicate map-less id is two
    /// policies for one id — rejected like a duplicate router entry, not
    /// tolerated like duplicate discovery metadata.
    #[test]
    fn a_duplicate_id_where_one_entry_carries_an_overlay_is_rejected() {
        let mut host = entry("claude-main");
        host.subagents = Some(overlay("type = \"passthrough\"\ntarget = \"child\""));

        let error = validate(vec![entry("claude-main"), host, mapped("child")]).unwrap_err();

        assert!(
            matches!(&error, ConfigError::DuplicateModelId { model } if model == "claude-main"),
            "{error}"
        );
    }
}
