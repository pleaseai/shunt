//! The load-time rules, against a whole `Config`.
//!
//! Non-vacuity: drop `names_a_policy`'s `subagents` arm and
//! `a_router_target_that_carries_an_overlay_is_rejected` goes red; skip
//! `validate_subagents` from `validate` and every rejection here goes green
//! for the wrong reason — which is why each asserts on the variant, not on
//! `is_err`.

use std::collections::BTreeMap;

use super::*;
use crate::config::{Config, ConfigError, ModelConfig, RouterConfig, SubagentsClassifierConfig};

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

    let error = validate(vec![routed, capable, mapped("efficient"), mapped("child")]).unwrap_err();

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

/// The classifier form's own rules (ADR-0005 §5, §8 PR 5).
///
/// Non-vacuity: drop the `classifier()` dispatch from `validate_subagents` and
/// every rejection here goes green while the overlay keeps a config libsy
/// would refuse to build — which is why each asserts on the variant, and why
/// the last one asserts on upstream's own message.
mod classifier {
    use super::*;
    use crate::config::{AuthMode, ClassifyTrigger, ProviderConfig, SubagentsCustomConfig};

    const CLASSIFIER: &str = r#"
    type = "llm_classifier"
    mode = "custom"
    models = { judge = ["judge-alias"], capable = ["capable-alias"], efficient = ["efficient-alias"], any = ["efficient-alias", "capable-alias"] }
    default_target = "efficient"
    prompt = "Select exactly one target for the delegated task."
    response_schema = '{"type": "object", "properties": {"target": {"type": "string"}}, "required": ["target"]}'
    policy = { type = "target_selector", selector = "/target" }
    "#;

    fn classifier() -> SubagentsConfig {
        overlay(CLASSIFIER)
    }

    fn custom(overlay: &mut SubagentsConfig) -> &mut SubagentsCustomConfig {
        match overlay {
            SubagentsConfig::LlmClassifier(SubagentsClassifierConfig::Custom(custom)) => custom,
            other => panic!("the fixture is the custom classifier form, got {other:?}"),
        }
    }

    /// One provider that injects a credential, so the judge rule is satisfied
    /// and the only rejections these tests see are the ones under test.
    fn injecting() -> BTreeMap<String, ProviderConfig> {
        let mut provider = Config::default()
            .providers
            .remove("anthropic")
            .expect("the default config ships an anthropic provider");
        provider.auth = AuthMode::ApiKey;
        provider.api_key_env = Some("SHUNT_TEST_JUDGE_KEY".to_string());
        BTreeMap::from([("keyed".to_string(), provider)])
    }

    /// The host entry, mapped on the injecting provider rather than on the
    /// shipped `anthropic` one, because these fixtures replace the provider
    /// map wholesale.
    fn host() -> ModelConfig {
        ModelConfig {
            upstream_model: Some(BTreeMap::from([(
                "keyed".to_string(),
                "upstream".to_string(),
            )])),
            ..entry("claude-main")
        }
    }

    fn validate_overlay(subagents: SubagentsConfig) -> Result<Config, ConfigError> {
        let mut host = host();
        host.subagents = Some(subagents);
        let mut config = Config {
            models: vec![host],
            providers: injecting(),
            ..Config::default()
        };
        config.server.default_provider = "keyed".to_string();
        config.validate()
    }

    #[test]
    fn the_classifier_form_loads_on_a_fixed_entry() {
        validate_overlay(classifier()).expect("the classifier overlay is well formed");
    }

    /// Upstream's own page: "user_turn is not supported for sub-agent routing."
    #[test]
    fn a_user_turn_trigger_is_rejected() {
        let mut overlay = classifier();
        custom(&mut overlay).classify_trigger = ClassifyTrigger::UserTurn;

        assert!(matches!(
            validate_overlay(overlay).unwrap_err(),
            ConfigError::SubagentsClassifierTrigger { ref model } if model == "claude-main"
        ));
    }

    /// The identity is `(session, agent)`; hashing the first user message
    /// would merge two agents whose opening prompt happens to match.
    #[test]
    fn message_hash_fallback_is_rejected() {
        let mut overlay = classifier();
        custom(&mut overlay).message_hash_fallback = true;

        assert!(matches!(
            validate_overlay(overlay).unwrap_err(),
            ConfigError::SubagentsMessageHashFallback { ref model } if model == "claude-main"
        ));
    }

    #[test]
    fn the_group_rules_are_enforced() {
        for group in ["any", "judge"] {
            let mut overlay = classifier();
            custom(&mut overlay).models.remove(group);
            assert!(
                matches!(
                    validate_overlay(overlay).unwrap_err(),
                    ConfigError::MissingClassifierGroup { group: found, .. } if found == group
                ),
                "a missing models.{group} must be rejected naming it"
            );
        }

        let mut overlay = classifier();
        custom(&mut overlay)
            .models
            .insert("extra".to_string(), vec!["unlisted-alias".to_string()]);
        assert!(matches!(
            validate_overlay(overlay).unwrap_err(),
            ConfigError::ClassifierTargetNotInAny { ref target, .. } if target == "unlisted-alias"
        ));

        let mut overlay = classifier();
        custom(&mut overlay).default_target = "judge".to_string();
        assert!(matches!(
            validate_overlay(overlay).unwrap_err(),
            ConfigError::InvalidClassifierDefaultTarget { ref group, .. } if group == "judge"
        ));
    }

    #[test]
    fn a_zero_call_bound_is_rejected_naming_its_key() {
        let mut overlay = classifier();
        custom(&mut overlay).max_judge_calls = 0;

        assert!(matches!(
            validate_overlay(overlay).unwrap_err(),
            ConfigError::ZeroCallBound { key, .. } if key == "max_judge_calls"
        ));
    }

    /// The overlay's judge is held to the same credential rule the router's
    /// is, and reports the key that named it.
    #[test]
    fn a_passthrough_judge_is_rejected_naming_its_key() {
        let mut host = entry("claude-main");
        host.upstream_model = Some(BTreeMap::from([(
            "open".to_string(),
            "upstream".to_string(),
        )]));
        host.subagents = Some(classifier());
        let mut provider = Config::default()
            .providers
            .remove("anthropic")
            .expect("the default config ships an anthropic provider");
        provider.auth = AuthMode::Passthrough;
        let mut config = Config {
            models: vec![host],
            providers: BTreeMap::from([("open".to_string(), provider)]),
            ..Config::default()
        };
        config.server.default_provider = "open".to_string();

        let error = config.validate().unwrap_err();
        assert!(
            matches!(&error, ConfigError::PassthroughJudgeTarget { key, .. } if key == "models.judge"),
            "{error}"
        );
    }

    /// The rule shunt does not spell: upstream's own constructor is the
    /// authority, and its message reaches the operator.
    #[test]
    fn an_invalid_selector_is_reported_as_a_build_failure() {
        let mut overlay = classifier();
        custom(&mut overlay).policy = crate::config::ClassifierPolicy::TargetSelector {
            selector: "target".to_string(),
        };

        let error = validate_overlay(overlay).unwrap_err();
        assert!(
            matches!(&error, ConfigError::DrivenRouterBuild { model, .. } if model == "claude-main"),
            "{error}"
        );
        assert!(
            error.to_string().contains("JSON Pointer"),
            "upstream's own message must reach the operator: {error}"
        );
    }
}
