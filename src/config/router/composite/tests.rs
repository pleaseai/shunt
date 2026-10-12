//! Table shape, round-trip, and load-time rules for `type = "composite"`.
//!
//! Non-vacuity: give [`CompositeTrigger`] an `EveryRequest` variant and
//! `every_request_is_rejected_by_name` goes red; add a `picker` key to
//! [`CompositeStageConfig`] and `a_stray_picker_is_rejected` goes red; skip
//! `validate_composite` from `validate_router` and every rejection here goes
//! green for the wrong reason — which is why each asserts on the variant, not
//! on `is_err`.

use std::collections::BTreeMap;

use super::*;
use crate::config::{AuthMode, Config, ConfigError, ModelConfig, ProviderConfig, RouterConfig};

/// ADR-0005 §2's composite example, verbatim apart from the `[models.router]`
/// prefixes the ADR writes on the table headers.
const COMPOSITE: &str = r#"
type = "composite"

[classifier]
target = "claude-sonnet-4-6"
base_threshold = 0.5
classify_trigger = "user_turn"

[stage]
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
confidence_threshold = 0.5
"#;

fn parse(toml: &str) -> Result<RouterConfig, toml::de::Error> {
    toml::from_str(toml)
}

fn fixture() -> RouterConfig {
    parse(COMPOSITE).expect("the ADR example parses")
}

fn payload(router: &mut RouterConfig) -> &mut CompositeRouterConfig {
    match router {
        RouterConfig::Composite(composite) => composite,
        other => panic!("the fixture is a composite router, got {other:?}"),
    }
}

fn validate(router: RouterConfig) -> Result<Config, ConfigError> {
    let mut provider = Config::default()
        .providers
        .remove("anthropic")
        .expect("the default config ships an anthropic provider");
    provider.auth = AuthMode::ApiKey;
    provider.api_key_env = Some("SHUNT_TEST_JUDGE_KEY".to_string());
    let mut config = Config {
        models: vec![ModelConfig {
            id: "claude-composite".to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(router),
            subagents: None,
            stage_router: None,
        }],
        providers: BTreeMap::from([("keyed".to_string(), provider)])
            as BTreeMap<String, ProviderConfig>,
        ..Config::default()
    };
    config.server.default_provider = "keyed".to_string();
    config.validate()
}

#[test]
fn the_adr_example_round_trips() {
    let parsed = fixture();
    let emitted = toml::to_string(&parsed).expect("the table serializes");
    let reparsed: RouterConfig = toml::from_str(&emitted).expect("the emitted table parses");
    assert_eq!(parsed, reparsed, "emitted:\n{emitted}");

    assert_eq!(parsed.algorithm(), "composite");
    assert!(parsed.is_driven());
    // No stage *table*: a composite entry's tier retention lives inside libsy,
    // not in shunt's `StageRouterStore`, so the pure lane must not find one.
    assert!(parsed.stage().is_none());
    assert_eq!(
        parsed.named_targets(),
        vec![
            (Cow::Borrowed("stage.capable_target"), "claude-opus-4-8"),
            (Cow::Borrowed("stage.efficient_target"), "claude-sonnet-4-6"),
        ]
    );
    assert_eq!(
        parsed.named_judges(),
        vec![(Cow::Borrowed("classifier.target"), "claude-sonnet-4-6")]
    );
    assert_eq!(parsed.fail_open_target(), Some("claude-sonnet-4-6"));
}

/// Upstream's `CompositeRouter::new` refuses `every_request`, so the enum has
/// no variant for it and the rejection happens at parse.
#[test]
fn every_request_is_rejected_by_name() {
    let error = parse(&COMPOSITE.replace("user_turn", "every_request")).unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.contains("every_request")
            && rendered.contains("user_turn")
            && rendered.contains("new_session"),
        "the error names the rejected trigger and the two that exist: {rendered}"
    );
}

/// The judge supplies the fall-open tier, so a configured picker would fight
/// the verdict. It is an unknown key, not a silently ignored one.
#[test]
fn a_stray_picker_is_rejected() {
    let error = parse(&format!("{COMPOSITE}picker = \"capable_first\"\n")).unwrap_err();
    assert!(error.to_string().contains("picker"), "{error}");
}

/// `classify_trigger` has no default here, unlike everywhere else the key
/// appears.
#[test]
fn a_missing_trigger_is_rejected() {
    let error = parse(&COMPOSITE.replace("classify_trigger = \"user_turn\"\n", "")).unwrap_err();
    assert!(error.to_string().contains("classify_trigger"), "{error}");
}

#[test]
fn both_thresholds_are_range_checked_by_key() {
    // NaN compares false against every bound, so a naive negated range would
    // accept it.
    for value in [0.0, -0.1, 1.1, f64::NAN] {
        let mut router = fixture();
        payload(&mut router).classifier.base_threshold = value;
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidStageRouterThreshold { key, .. }
                    if key == "classifier.base_threshold"
            ),
            "classifier.base_threshold {value} must be rejected naming its key"
        );

        let mut router = fixture();
        payload(&mut router).stage.confidence_threshold = value;
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidStageRouterThreshold { key, .. }
                    if key == "stage.confidence_threshold"
            ),
            "stage.confidence_threshold {value} must be rejected naming its key"
        );
    }
}

#[test]
fn a_zero_recent_turn_window_is_rejected() {
    let mut router = fixture();
    payload(&mut router).stage.recent_turn_window = 0;
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::InvalidStageRouterWindow { ref model } if model == "claude-composite"
    ));
}

#[test]
fn a_zero_call_bound_is_rejected_naming_its_key() {
    let mut router = fixture();
    payload(&mut router).judge_timeout_ms = 0;
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::ZeroCallBound { key, .. } if key == "judge_timeout_ms"
    ));
}

/// The standalone classifier's `new_session` pairing rule is *not* raised on a
/// composite: upstream refuses only `every_request` there and retains the tier
/// by hashed first user message under either trigger this table can spell.
#[test]
fn message_hash_fallback_is_accepted_on_either_trigger() {
    let mut router = fixture();
    let composite = payload(&mut router);
    composite.classifier.message_hash_fallback = true;
    composite.classifier.classify_trigger = CompositeTrigger::UserTurn;
    validate(router).expect("user_turn accepts the fallback");

    let mut router = fixture();
    let composite = payload(&mut router);
    composite.classifier.message_hash_fallback = true;
    composite.classifier.classify_trigger = CompositeTrigger::NewSession;
    validate(router).expect("new_session accepts the fallback");
}

/// The stage sub-table's `tool_semantics` is held to the same rule the
/// `stage_router` table's is: a built-in name cannot be reclassified.
#[test]
fn a_builtin_tool_semantics_name_is_rejected() {
    let mut router = fixture();
    payload(&mut router).stage.tool_semantics.observe = vec!["Read".to_string()];
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::BuiltinToolSemanticsName { ref name, .. } if name == "Read"
    ));
}

/// A judge must resolve to a credential-injecting route, under its own key.
#[test]
fn a_passthrough_judge_is_rejected_naming_its_key() {
    let mut provider = Config::default()
        .providers
        .remove("anthropic")
        .expect("the default config ships an anthropic provider");
    provider.auth = AuthMode::Passthrough;
    let mut config = Config {
        models: vec![ModelConfig {
            id: "claude-composite".to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(fixture()),
            subagents: None,
            stage_router: None,
        }],
        providers: BTreeMap::from([("open".to_string(), provider)]),
        ..Config::default()
    };
    config.server.default_provider = "open".to_string();

    let error = config.validate().unwrap_err();
    assert!(
        matches!(&error, ConfigError::PassthroughJudgeTarget { key, .. } if key == "classifier.target"),
        "{error}"
    );
}
