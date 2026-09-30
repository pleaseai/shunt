//! Table-shape tests.
//!
//! Non-vacuity: drop `deny_unknown_fields` from [`PassthroughSubagentsConfig`]
//! and `a_stray_key_is_rejected` goes red; swap the `map_or` arms in
//! [`PassthroughSubagentsConfig::target_for`] and `by_type_beats_target_only_on_an_exact_key`
//! goes red on both assertions.

use super::*;

fn parse(toml: &str) -> Result<SubagentsConfig, toml::de::Error> {
    toml::from_str(toml)
}

fn passthrough(overlay: &SubagentsConfig) -> &PassthroughSubagentsConfig {
    overlay
        .passthrough()
        .expect("the fixtures here are all the passthrough form")
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

    let passthrough = overlay.passthrough().expect("the passthrough form parses");
    assert_eq!(passthrough.target, "claude-haiku-4-5");
    assert_eq!(passthrough.by_type.len(), 3);
    assert_eq!(overlay.algorithm(), "subagents");
    assert_eq!(
        overlay.named_targets(),
        vec![
            (Cow::Borrowed("target"), "claude-haiku-4-5"),
            (Cow::Borrowed("by_type.Explore"), "claude-haiku-4-5"),
            (Cow::Borrowed("by_type.fork"), "claude-sonnet-4-6"),
            (Cow::Borrowed("by_type.teammate"), "claude-sonnet-4-6"),
        ]
    );
}

/// `by_type` is optional: `target` alone is the whole upstream form.
#[test]
fn by_type_is_optional() {
    let overlay = parse("type = \"passthrough\"\ntarget = \"claude-haiku-4-5\"")
        .expect("target alone is well formed");

    assert_eq!(
        passthrough(&overlay).target_for(Some("Explore")),
        ("claude-haiku-4-5", false)
    );
    assert_eq!(
        passthrough(&overlay).target_for(None),
        ("claude-haiku-4-5", false)
    );
}

/// Upstream's own sub-agent example (`docs/routing_algorithms/subagent_routing.md`),
/// on public shunt ids: its `[targets.*]` indirection is shunt's `[[models]]`
/// ladder, so the groups name model ids directly.
const UPSTREAM_EXAMPLE: &str = r#"
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
classify_trigger = "new_session"
max_output_tokens = 64
prompt = """
Select exactly one target for the delegated task.

- Select "capable" for code review, critique, auditing, or correctness analysis.
- Select "efficient" for implementation, research, explanation, and other delegated work.

Return only JSON matching the response schema.
"""
response_schema = '''
{
  "type": "object",
  "properties": {
    "target": {"type": "string", "enum": ["capable", "efficient"]}
  },
  "required": ["target"],
  "additionalProperties": false
}
'''
policy = { type = "target_selector", selector = "/target" }
"#;

/// The classifier form, parsed and round-tripped: TOML → struct → TOML →
/// struct, compared on the value, because a re-serialized table may order its
/// keys differently from the operator's file.
#[test]
fn the_upstream_classifier_example_round_trips() {
    let overlay = parse(UPSTREAM_EXAMPLE).expect("the upstream example is well formed");
    let emitted = toml::to_string(&overlay).expect("the overlay serializes");
    let reparsed = parse(&emitted).expect("the emitted table parses");
    assert_eq!(overlay, reparsed, "emitted:\n{emitted}");

    let classifier = overlay
        .classifier()
        .expect("the classifier form parses as one");
    assert_eq!(overlay.algorithm(), "subagents");
    assert!(overlay.is_driven());
    assert!(overlay.passthrough().is_none());
    // `judge` is consulted, never served, so it is absent from the targets and
    // is the whole judge list.
    assert_eq!(
        overlay.named_targets(),
        vec![
            (Cow::Borrowed("models.any"), "claude-sonnet-4-6"),
            (Cow::Borrowed("models.any"), "claude-opus-4-8"),
            (Cow::Borrowed("models.capable"), "claude-opus-4-8"),
            (Cow::Borrowed("models.efficient"), "claude-sonnet-4-6"),
        ]
    );
    assert_eq!(
        overlay.named_judges(),
        vec![(Cow::Borrowed("models.judge"), "claude-haiku-4-5")]
    );
    assert_eq!(classifier.fail_open_target(), "claude-sonnet-4-6");
    assert_eq!(classifier.custom().max_output_tokens, 64);
    // The overlay keys on (session, agent), so the default is once per agent.
    assert_eq!(
        classifier.custom().classify_trigger,
        crate::config::ClassifyTrigger::NewSession
    );
    // `by_type` is a passthrough key, so the classifier form declares no agent
    // types at all — the judge decides, not the header.
    assert_eq!(overlay.agent_types().count(), 0);
}

/// The trigger defaults to `new_session` rather than to `every_request`, which
/// is what makes a delegated agent classified once instead of per turn.
#[test]
fn an_omitted_trigger_defaults_to_new_session() {
    let overlay = parse(&UPSTREAM_EXAMPLE.replace("classify_trigger = \"new_session\"\n", ""))
        .expect("the trigger is optional");

    assert_eq!(
        overlay
            .classifier()
            .expect("the classifier form")
            .custom()
            .classify_trigger,
        crate::config::ClassifyTrigger::NewSession
    );
}

/// `mode` is the tag, so the capability rubric — which judges a task against
/// two tiers rather than naming the operator's own groups — is an
/// unknown-variant load error naming the one mode that exists.
#[test]
fn a_non_custom_mode_is_rejected_by_name() {
    for mode in ["capability", "escalation"] {
        let error = parse(&format!("type = \"llm_classifier\"\nmode = \"{mode}\"")).unwrap_err();
        let rendered = error.to_string();
        assert!(
            rendered.contains(mode) && rendered.contains("custom"),
            "the error names the rejected mode and the one that exists: {rendered}"
        );
    }
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
        passthrough(&overlay).target_for(Some("Explore")),
        ("explore-target", true)
    );
    assert_eq!(
        passthrough(&overlay).target_for(Some("explore")),
        ("fallback", false)
    );
    assert_eq!(
        passthrough(&overlay).target_for(Some("custom")),
        ("custom-target", true)
    );
    assert_eq!(
        passthrough(&overlay).target_for(Some("general-purpose")),
        ("fallback", false)
    );
    assert_eq!(passthrough(&overlay).target_for(None), ("fallback", false));
}
