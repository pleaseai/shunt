//! Table shape, round-trip, and load-time rules for `type = "advisor"`.
//!
//! Non-vacuity: drop the `Advisor` variant from `RouterConfig` and every test
//! here goes red at the parse; drop `validate_advisor` from `validate_router`
//! and the pattern-pairing and passthrough-advisor rejections go red on their
//! variants; drop the `Advisor` arm from `check_buildable` and the three
//! upstream-rule rejections go red, since nothing else constructs the gate at
//! load. Each rejection asserts on its variant (and key), not on `is_err`.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::config::{AuthMode, Config, ConfigError, ModelConfig, ProviderConfig, RouterConfig};

/// Every key upstream's table documents, at non-default values where one
/// would otherwise be omitted from the emitted file.
const ADVISOR: &str = r#"
type = "advisor"
executor_target = "claude-sonnet-4-6"
advisor_target = "claude-opus-4-8"
gate_trigger = "pattern"
gate_trigger_pattern = "(?i)task complete"
max_reviews = 2
gate_stall_turns = 12
gate_min_tool_results = 1
advisor_max_tokens = 1024
advisor_temperature = 0.2
transcript_max_chars = 100000
fail_open = false
reviewer_system_prompt = "Reply APPROVE or REDO."
redo_feedback_prefix = "Keep going: "
judge_timeout_ms = 20000
"#;

/// The smallest table: the two ids and nothing else.
const MINIMAL: &str = r#"
type = "advisor"
executor_target = "claude-sonnet-4-6"
advisor_target = "claude-opus-4-8"
"#;

fn parse(toml: &str) -> Result<RouterConfig, toml::de::Error> {
    toml::from_str(toml)
}

fn advisor(router: &mut RouterConfig) -> &mut super::AdvisorRouterConfig {
    match router {
        RouterConfig::Advisor(advisor) => advisor,
        other => panic!("the fixture is an advisor, got {other:?}"),
    }
}

/// Two providers: `keyed` injects a credential, `open` is passthrough. The
/// default provider is the one each test names, which is what every id in the
/// fixtures resolves to.
fn validate_on(router: RouterConfig, default_provider: &str) -> Result<Config, ConfigError> {
    let mut keyed = Config::default()
        .providers
        .remove("anthropic")
        .expect("the default config ships an anthropic provider");
    keyed.auth = AuthMode::ApiKey;
    keyed.api_key_env = Some("SHUNT_TEST_JUDGE_KEY".to_string());
    let mut open = keyed.clone();
    open.auth = AuthMode::Passthrough;
    open.api_key_env = None;
    let providers: BTreeMap<String, ProviderConfig> =
        BTreeMap::from([("keyed".to_string(), keyed), ("open".to_string(), open)]);
    let mut config = Config {
        models: vec![ModelConfig {
            id: "claude-advised".to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(router),
            subagents: None,
            stage_router: None,
        }],
        providers,
        ..Config::default()
    };
    config.server.default_provider = default_provider.to_string();
    config.validate()
}

fn validate(router: RouterConfig) -> Result<Config, ConfigError> {
    validate_on(router, "keyed")
}

#[test]
fn the_full_example_round_trips() {
    let parsed = parse(ADVISOR).expect("the example parses");
    let emitted = toml::to_string(&parsed).expect("the table serializes");
    let reparsed: RouterConfig = toml::from_str(&emitted).expect("the emitted table parses");
    assert_eq!(parsed, reparsed, "emitted:\n{emitted}");
    assert_eq!(parsed.algorithm(), "advisor");
    assert!(parsed.is_driven());
    assert_eq!(
        parsed.named_targets(),
        vec![(Cow::Borrowed("executor_target"), "claude-sonnet-4-6")]
    );
    assert_eq!(
        parsed.named_judges(),
        vec![(Cow::Borrowed("advisor_target"), "claude-opus-4-8")]
    );
    assert_eq!(parsed.fail_open_target(), Some("claude-sonnet-4-6"));
    assert!(parsed.bounds().is_some());
    validate(parsed).expect("the full example validates");
}

/// Upstream's defaults, and an emitted file that carries only what the
/// operator wrote.
#[test]
fn an_omitted_key_takes_upstreams_default_and_emits_nothing() {
    let mut parsed = parse(MINIMAL).expect("the minimal table parses");
    let emitted = toml::to_string(&parsed).expect("the table serializes");
    for key in [
        "gate_trigger",
        "gate_trigger_pattern",
        "gate_stall_turns",
        "advisor_temperature",
    ] {
        assert!(!emitted.contains(key), "{key} was not written: {emitted}");
    }
    let libsy = advisor(&mut parsed).to_libsy();
    let defaults = switchyard_libsy::AdvisorGateConfig::default();
    assert_eq!(
        libsy.gate_trigger,
        switchyard_libsy::GateTrigger::NoToolCall
    );
    assert_eq!(libsy.max_reviews, defaults.max_reviews);
    assert_eq!(libsy.advisor_max_tokens, defaults.advisor_max_tokens);
    assert_eq!(libsy.transcript_max_chars, defaults.transcript_max_chars);
    assert_eq!(libsy.fail_open, defaults.fail_open);
    assert_eq!(
        libsy.reviewer_system_prompt,
        defaults.reviewer_system_prompt
    );
    assert_eq!(libsy.redo_feedback_prefix, defaults.redo_feedback_prefix);
}

#[test]
fn a_stray_key_is_rejected() {
    let error = parse(&format!("{MINIMAL}picker = \"efficient_first\"\n")).unwrap_err();
    assert!(error.to_string().contains("picker"), "{error}");
}

/// The pairing rule, both halves, naming the key.
#[test]
fn the_trigger_pattern_pairing_is_enforced() {
    for (trigger, pattern) in [
        ("pattern", None),
        ("pattern", Some("   ")),
        ("no_tool_call", Some("done")),
    ] {
        let mut router = parse(MINIMAL).expect("parses");
        let payload = advisor(&mut router);
        payload.gate_trigger = match trigger {
            "pattern" => super::AdvisorGateTrigger::Pattern,
            _ => super::AdvisorGateTrigger::NoToolCall,
        };
        payload.gate_trigger_pattern = pattern.map(str::to_string);
        let error = validate(router).unwrap_err();
        assert!(
            matches!(&error, ConfigError::InvalidAdvisorGatePattern { .. })
                && error.to_string().contains("gate_trigger_pattern"),
            "trigger {trigger} with pattern {pattern:?}: {error}"
        );
    }
}

/// upstream's own constructor rules, reached through the build check and
/// quoting upstream's message.
#[test]
fn upstreams_constructor_rules_fail_the_load() {
    type Edit = fn(&mut super::AdvisorRouterConfig);
    let cases: [(&str, Edit); 3] = [
        ("max_reviews", |advisor| advisor.max_reviews = 0),
        ("advisor_max_tokens", |advisor| {
            advisor.advisor_max_tokens = 0
        }),
        ("transcript_max_chars", |advisor| {
            advisor.transcript_max_chars = 255
        }),
    ];
    for (key, edit) in cases {
        let mut router = parse(MINIMAL).expect("parses");
        edit(advisor(&mut router));
        let error = validate(router).unwrap_err();
        assert!(
            matches!(&error, ConfigError::DrivenRouterBuild { model, .. } if model == "claude-advised")
                && error.to_string().contains(key),
            "{key}: {error}"
        );
    }
    // The boundary itself is accepted: upstream's floor is inclusive.
    let mut router = parse(MINIMAL).expect("parses");
    advisor(&mut router).transcript_max_chars = 256;
    validate(router).expect("256 is upstream's floor");
}

#[test]
fn a_zero_call_bound_is_rejected_naming_its_key() {
    let mut router = parse(MINIMAL).expect("parses");
    advisor(&mut router).gated_idle_ms = 0;
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::ZeroCallBound { key, .. } if key == "gated_idle_ms"
    ));
}

/// The advisor is a judge — it runs on the gateway's credential — while the
/// executor's gated turn carries the caller's own. So a passthrough advisor is
/// refused and a passthrough executor is not.
#[test]
fn only_the_advisor_is_held_to_the_judge_rule() {
    let error = validate_on(parse(MINIMAL).expect("parses"), "open").unwrap_err();
    assert!(
        matches!(&error, ConfigError::PassthroughJudgeTarget { key, .. } if key == "advisor_target"),
        "{error}"
    );

    let mut config = validate(parse(MINIMAL).expect("parses")).expect("validates");
    // The executor alone moved to the passthrough provider.
    config.models.push(ModelConfig {
        id: "claude-sonnet-4-6".to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            "open".to_string(),
            "claude-sonnet-4-6".to_string(),
        )])),
        router: None,
        subagents: None,
        stage_router: None,
    });
    config
        .validate()
        .expect("a passthrough executor is the caller's own dispatch");
}
