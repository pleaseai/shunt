//! Table shape, round-trip, and load-time rules for
//! `type = "llm_classifier"`.
//!
//! Non-vacuity: drop the `mode` tag and `a_missing_mode_is_rejected` goes
//! red; drop the `Escalation` variant and `the_escalation_example_round_trips`
//! goes red at the parse; drop
//! `skip_serializing_if` from `classify_trigger` and both round-trips stay
//! green while the *emitted* TOML gains a key the operator did not write,
//! which is why `an_omitted_trigger_emits_no_key` asserts on the text rather
//! than on the struct; skip `validate_llm_classifier` from `validate_router`
//! and every rejection here goes green for the wrong reason — which is why
//! each asserts on the variant, not on `is_err`.

use std::collections::BTreeMap;

use super::*;
use crate::config::{
    AuthMode, Config, ConfigError, ModelConfig, ProviderConfig, RouterConfig,
    DEFAULT_MAX_JUDGE_CALLS,
};

/// The capability example: two tiers, a judge, and the packaged rubric.
const CAPABILITY: &str = r#"
type = "llm_classifier"
mode = "capability"
classifier_target = "claude-haiku-4-5"
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
base_threshold = 0.5
threshold_step = 0.2
classify_trigger = "user_turn"
max_output_tokens = 4096
"#;

/// The custom example, keyed like upstream's sub-agent page but on public
/// shunt ids and as a `[models.router]` table.
const CUSTOM: &str = r#"
type = "llm_classifier"
mode = "custom"
default_target = "efficient"
classify_trigger = "new_session"
prompt = "Select exactly one target for the task."
response_schema = '''
{"type": "object", "properties": {"target": {"type": "string"}}, "required": ["target"], "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }

[models]
judge = ["claude-haiku-4-5"]
capable = ["claude-opus-4-8"]
efficient = ["claude-sonnet-4-6"]
any = ["claude-sonnet-4-6", "claude-opus-4-8"]
"#;

fn parse(toml: &str) -> Result<RouterConfig, toml::de::Error> {
    toml::from_str(toml)
}

fn fixture(toml: &str) -> RouterConfig {
    parse(toml).expect("the example parses")
}

/// TOML → struct → TOML → struct, compared on the struct: a re-serialized
/// table may order its keys differently from the operator's file, so the text
/// is not the invariant — the value is.
fn round_trip(toml: &str) -> (RouterConfig, String) {
    let parsed = fixture(toml);
    let emitted = toml::to_string(&parsed).expect("the table serializes");
    let reparsed: RouterConfig = toml::from_str(&emitted).expect("the emitted table parses");
    assert_eq!(parsed, reparsed, "emitted:\n{emitted}");
    (parsed, emitted)
}

/// One provider that injects a credential, so a judge target is legal and the
/// only rejections these tests can see are the ones under test.
fn injecting() -> BTreeMap<String, ProviderConfig> {
    let mut provider = Config::default()
        .providers
        .remove("anthropic")
        .expect("the default config ships an anthropic provider");
    provider.auth = AuthMode::ApiKey;
    provider.api_key_env = Some("SHUNT_TEST_JUDGE_KEY".to_string());
    BTreeMap::from([("keyed".to_string(), provider)])
}

fn validate(router: RouterConfig) -> Result<Config, ConfigError> {
    let mut config = Config {
        models: vec![ModelConfig {
            id: "claude-classified".to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(router),
            subagents: None,
            stage_router: None,
        }],
        providers: injecting(),
        ..Config::default()
    };
    config.server.default_provider = "keyed".to_string();
    config.validate()
}

/// The payload of a parsed `llm_classifier`, for the tests that edit one key.
fn capability(router: &mut RouterConfig) -> &mut CapabilityClassifierConfig {
    match router {
        RouterConfig::LlmClassifier(LlmClassifierConfig::Capability(capability)) => capability,
        other => panic!("the fixture is capability mode, got {other:?}"),
    }
}

fn custom(router: &mut RouterConfig) -> &mut CustomClassifierConfig {
    match router {
        RouterConfig::LlmClassifier(LlmClassifierConfig::Custom(custom)) => custom,
        other => panic!("the fixture is custom mode, got {other:?}"),
    }
}

#[test]
fn the_capability_example_round_trips() {
    let (parsed, _) = round_trip(CAPABILITY);
    let RouterConfig::LlmClassifier(classifier) = &parsed else {
        panic!("the fixture is an llm_classifier, got {parsed:?}");
    };
    assert_eq!(parsed.algorithm(), "llm_classifier");
    assert!(parsed.is_driven());
    assert_eq!(
        classifier.named_targets(),
        vec![
            (Cow::Borrowed("strong_target"), "claude-opus-4-8"),
            (Cow::Borrowed("weak_target"), "claude-sonnet-4-6"),
        ]
    );
    assert_eq!(
        classifier.named_judges(),
        vec![(Cow::Borrowed("classifier_target"), "claude-haiku-4-5")]
    );
    // libsy's capability route closes on `Category::Capable`, so the strong
    // tier is where a turn with no usable verdict lands.
    assert_eq!(classifier.fail_open_target(), "claude-opus-4-8");
    assert_eq!(classifier.classify_trigger(), ClassifyTrigger::UserTurn);
    assert_eq!(classifier.bounds().max_judge_calls, DEFAULT_MAX_JUDGE_CALLS);
}

#[test]
fn the_custom_example_round_trips() {
    let (parsed, _) = round_trip(CUSTOM);
    let RouterConfig::LlmClassifier(classifier) = &parsed else {
        panic!("the fixture is an llm_classifier, got {parsed:?}");
    };
    // `judge` is excluded from the targets and is the only judge; the rest are
    // in `BTreeMap` order, which is what makes the list stable across writes
    // of the same table.
    assert_eq!(
        classifier.named_targets(),
        vec![
            (Cow::Borrowed("models.any"), "claude-sonnet-4-6"),
            (Cow::Borrowed("models.any"), "claude-opus-4-8"),
            (Cow::Borrowed("models.capable"), "claude-opus-4-8"),
            (Cow::Borrowed("models.efficient"), "claude-sonnet-4-6"),
        ]
    );
    assert_eq!(
        classifier.named_judges(),
        vec![(Cow::Borrowed("models.judge"), "claude-haiku-4-5")]
    );
    assert_eq!(classifier.fail_open_target(), "claude-sonnet-4-6");
}

/// The key that makes the emitted file the operator's rather than serde's.
#[test]
fn an_omitted_trigger_emits_no_key() {
    let (_, emitted) = round_trip(
        r#"
        type = "llm_classifier"
        mode = "capability"
        classifier_target = "judge"
        strong_target = "strong"
        weak_target = "weak"
        base_threshold = 0.5
        "#,
    );
    assert!(
        !emitted.contains("classify_trigger"),
        "an omitted trigger must round-trip as omitted: {emitted}"
    );
}

/// `mode` carries no default, so one mode's keys cannot load as some other
/// algorithm — upstream would read an escalation table without `mode`.
#[test]
fn a_missing_mode_is_rejected() {
    let error = parse(
        "type = \"llm_classifier\"\nclassifier_target = \"j\"\nstrong_target = \"s\"\nweak_target = \"w\"\nbase_threshold = 0.5",
    )
    .unwrap_err();
    assert!(error.to_string().contains("mode"), "{error}");
}

/// The escalation example: the three ids, a replacement prompt, and the
/// `[escalation]` sub-table the streak is tuned by.
const ESCALATION: &str = r#"
type = "llm_classifier"
mode = "escalation"
classifier_target = "claude-haiku-4-5"
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
prompt = "Judge whether the trajectory is stuck."
gated_idle_ms = 30000

[escalation]
confirmations = 1
recent_turn_window = 12
window_message_chars = 400
"#;

/// The positive twin of PR 5's placeholder refusal: `mode = "escalation"`
/// loads, names its targets and judge under their own keys, fails open to the
/// *weak* tier (the turn a failed judge leaves standing), and is gated.
///
/// Deleting the `Escalation` variant turns this red at the parse; routing its
/// `fail_open_target` to the strong tier turns it red at that assertion.
#[test]
fn the_escalation_example_round_trips() {
    let (parsed, _) = round_trip(ESCALATION);
    let RouterConfig::LlmClassifier(classifier) = &parsed else {
        panic!("the fixture is an llm_classifier, got {parsed:?}");
    };
    let LlmClassifierConfig::Escalation(escalation) = classifier else {
        panic!("the fixture is escalation mode, got {classifier:?}");
    };
    assert!(parsed.is_driven());
    assert!(classifier.is_gated(), "escalation retains the weak turn");
    assert_eq!(
        classifier.named_targets(),
        vec![
            (Cow::Borrowed("strong_target"), "claude-opus-4-8"),
            (Cow::Borrowed("weak_target"), "claude-sonnet-4-6"),
        ]
    );
    assert_eq!(
        classifier.named_judges(),
        vec![(Cow::Borrowed("classifier_target"), "claude-haiku-4-5")]
    );
    assert_eq!(classifier.fail_open_target(), "claude-sonnet-4-6");
    assert_eq!(escalation.escalation.confirmations, 1);
    assert_eq!(escalation.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(
        classifier.bounds().gated_idle,
        std::time::Duration::from_millis(30_000)
    );
    validate(parsed).expect("the escalation example validates");
}

/// An omitted `[escalation]` table is upstream's defaults, and round-trips as
/// omitted rather than as a table the operator never wrote.
#[test]
fn an_omitted_escalation_table_takes_upstreams_defaults_and_emits_nothing() {
    let (parsed, emitted) = round_trip(
        "type = \"llm_classifier\"\nmode = \"escalation\"\nclassifier_target = \"j\"\nstrong_target = \"s\"\nweak_target = \"w\"\n",
    );
    let RouterConfig::LlmClassifier(LlmClassifierConfig::Escalation(escalation)) = &parsed else {
        panic!("escalation mode, got {parsed:?}");
    };
    assert_eq!(
        (
            escalation.escalation.confirmations,
            escalation.escalation.recent_turn_window,
            escalation.escalation.window_message_chars
        ),
        (2, 28, 500)
    );
    assert!(!emitted.contains("[escalation]"), "{emitted}");
}

/// A stray key is refused on the mode's table and on its sub-table alike.
#[test]
fn a_stray_escalation_key_is_rejected() {
    // Another mode's key on this one's table: `base_threshold` is capability's.
    let top_level = ESCALATION.replace("[escalation]", "base_threshold = 0.5\n\n[escalation]");
    let error = parse(&top_level).unwrap_err();
    assert!(error.to_string().contains("base_threshold"), "{error}");
    // Appended after the header, so it lands inside `[escalation]`.
    let error = parse(&format!("{ESCALATION}streak = 3\n")).unwrap_err();
    assert!(error.to_string().contains("streak"), "{error}");
}

/// upstream's constructor rule, reached through the build check: zero
/// confirmations would escalate on no verdict at all.
#[test]
fn zero_confirmations_is_rejected_as_a_build_failure() {
    let mut router = fixture(ESCALATION);
    let RouterConfig::LlmClassifier(LlmClassifierConfig::Escalation(escalation)) = &mut router
    else {
        panic!("escalation mode");
    };
    escalation.escalation.confirmations = 0;
    let error = validate(router).unwrap_err();
    assert!(
        matches!(&error, ConfigError::DrivenRouterBuild { .. })
            && error.to_string().contains("confirmations"),
        "{error}"
    );
}

#[test]
fn a_stray_key_is_rejected() {
    let error = parse(&format!("{CAPABILITY}picker = \"efficient_first\"\n")).unwrap_err();
    assert!(error.to_string().contains("picker"), "{error}");
}

/// The threshold rules, each asserting its own variant.
#[test]
fn the_threshold_rules_are_enforced_by_key() {
    // NaN is in the table deliberately: it compares false against every bound,
    // so a naive `v < 0.0 || v > 1.0` check would accept it.
    for value in [0.0, -0.1, 1.1, f64::NAN] {
        let mut router = fixture(CAPABILITY);
        capability(&mut router).base_threshold = value;
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidStageRouterThreshold { key, .. } if key == "base_threshold"
            ),
            "base_threshold {value} must be rejected naming its own key"
        );
    }
    // `base + 2 * step` must still be a probability, and a negative or
    // non-finite step is refused outright — libsy's own rule, at load.
    for step in [-0.1, f64::NAN, 0.3] {
        let mut router = fixture(CAPABILITY);
        capability(&mut router).threshold_step = step;
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidThresholdStep { .. }
            ),
            "threshold_step {step} must be rejected"
        );
    }
}

#[test]
fn a_zero_max_output_tokens_is_rejected() {
    let mut router = fixture(CAPABILITY);
    capability(&mut router).max_output_tokens = 0;
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::ZeroMaxOutputTokens { ref model } if model == "claude-classified"
    ));
}

#[test]
fn a_zero_recent_turn_window_is_rejected() {
    let mut router = fixture(CAPABILITY);
    capability(&mut router).recent_turn_window = Some(0);
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::InvalidClassifierWindow { .. }
    ));
}

#[test]
fn a_zero_call_bound_is_rejected_naming_its_key() {
    let mut router = fixture(CAPABILITY);
    capability(&mut router).max_judge_calls = 0;
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::ZeroCallBound { key, .. } if key == "max_judge_calls"
    ));
}

/// libsy refuses the pair; shunt refuses it at load so the message names the
/// operator's own keys.
#[test]
fn message_hash_fallback_requires_new_session() {
    let mut router = fixture(CAPABILITY);
    capability(&mut router).message_hash_fallback = true;
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::MessageHashFallbackTrigger { .. }
    ));

    let mut router = fixture(CAPABILITY);
    let payload = capability(&mut router);
    payload.message_hash_fallback = true;
    payload.classify_trigger = ClassifyTrigger::NewSession;
    validate(router).expect("new_session accepts the fallback");
}

/// The four group rules, each naming the group the operator wrote.
#[test]
fn the_custom_group_rules_are_enforced() {
    for group in ["any", "judge"] {
        let mut router = fixture(CUSTOM);
        custom(&mut router).models.remove(group);
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::MissingClassifierGroup { group: found, .. } if found == group
            ),
            "a missing models.{group} must be rejected naming it"
        );
    }

    let mut router = fixture(CUSTOM);
    custom(&mut router)
        .models
        .insert("extra".to_string(), vec!["claude-unlisted".to_string()]);
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::ClassifierTargetNotInAny { ref group, ref target, .. }
            if group == "extra" && target == "claude-unlisted"
    ));

    // `judge` is consulted, never served, so it can never be the fail-open
    // destination.
    for default_target in ["judge", "no-such-group"] {
        let mut router = fixture(CUSTOM);
        custom(&mut router).default_target = default_target.to_string();
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidClassifierDefaultTarget { ref group, .. } if group == default_target
            ),
            "default_target = {default_target:?} must be rejected"
        );
    }
}

#[test]
fn a_malformed_response_schema_is_rejected() {
    for schema in ["{ not json", "[1, 2]", "\"a string\""] {
        let mut router = fixture(CUSTOM);
        custom(&mut router).response_schema = schema.to_string();
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidResponseSchema { .. }
            ),
            "response_schema {schema:?} must be rejected"
        );
    }
}

/// libsy supplies the schema itself and refuses a prompt that tries to.
#[test]
fn a_prompt_placeholder_or_blank_prompt_is_rejected() {
    for prompt in ["   ", "Return JSON matching {{RESPONSE_SCHEMA}}"] {
        let mut router = fixture(CUSTOM);
        custom(&mut router).prompt = prompt.to_string();
        assert!(
            matches!(
                validate(router).unwrap_err(),
                ConfigError::InvalidClassifierPrompt { .. }
            ),
            "prompt {prompt:?} must be rejected"
        );
    }
}

/// The rule shunt does *not* spell: a selector that is not a JSON Pointer is
/// upstream's own construction error, surfaced at load through the real
/// `LlmTaskClassifier` build rather than re-implemented here.
#[test]
fn an_invalid_selector_is_reported_as_a_build_failure() {
    let mut router = fixture(CUSTOM);
    custom(&mut router).policy = ClassifierPolicy::TargetSelector {
        selector: "target".to_string(),
    };
    let error = validate(router).unwrap_err();
    assert!(
        matches!(&error, ConfigError::DrivenRouterBuild { model, .. } if model == "claude-classified"),
        "{error}"
    );
    assert!(
        error.to_string().contains("JSON Pointer"),
        "upstream's own message must reach the operator: {error}"
    );
}

/// A blank id anywhere in the table is refused under the key that wrote it —
/// the same loop that holds answer targets and judges to the one-hop rule.
#[test]
fn a_blank_group_target_is_rejected_by_key() {
    let mut router = fixture(CUSTOM);
    custom(&mut router)
        .models
        .insert("capable".to_string(), vec!["  ".to_string()]);
    assert!(matches!(
        validate(router).unwrap_err(),
        ConfigError::EmptyRouterTarget { ref key, .. } if key == "models.capable"
    ));
}

/// A judge must resolve to a credential-injecting route, and the rejection
/// names the key that wrote it rather than a generic "target".
#[test]
fn a_passthrough_judge_is_rejected_naming_its_key() {
    let mut config = Config {
        models: vec![ModelConfig {
            id: "claude-classified".to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(fixture(CAPABILITY)),
            subagents: None,
            stage_router: None,
        }],
        providers: {
            let mut providers = injecting();
            let mut open = providers["keyed"].clone();
            open.auth = AuthMode::Passthrough;
            open.api_key_env = None;
            providers.insert("open".to_string(), open);
            providers
        },
        ..Config::default()
    };
    config.server.default_provider = "open".to_string();

    let error = config.validate().unwrap_err();
    assert!(
        matches!(&error, ConfigError::PassthroughJudgeTarget { key, .. } if key == "classifier_target"),
        "{error}"
    );
}
