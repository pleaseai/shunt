use axum::http::HeaderMap;
use serde_json::{json, Value};

use super::{classifier_upstream_model, is_classifier_request};
use super::{restore_claude_code_identity, CLASSIFIER_PROMPT_PREFIX, CLAUDE_CODE_IDENTITY};
use crate::config::{Config, ProviderConfig};
use crate::request::RequestBody;
use axum::http::HeaderValue;

/// The seeded `anthropic` provider, which is `kind = "anthropic"` and
/// carries no `classifier_model` — the state every assertion below starts
/// from.
fn anthropic_provider() -> ProviderConfig {
    Config::default()
        .providers
        .get("anthropic")
        .expect("the default config seeds an anthropic provider")
        .clone()
}

fn classifier_prompt() -> String {
    format!("{CLASSIFIER_PROMPT_PREFIX}\n\n## Context\n\nThe agent you are monitoring is …")
}

fn run(body: Value, headers: &HeaderMap) -> Value {
    let raw = serde_json::to_vec(&body).unwrap();
    let mut request = RequestBody::parse(raw).unwrap();
    restore_claude_code_identity(&mut request, headers);
    serde_json::from_slice(&request.into_raw()).unwrap()
}

/// Assert the body is forwarded exactly as the client sent it — key order
/// and whitespace included, which is what proves no re-serialization ran.
fn assert_verbatim(body: &str, headers: &HeaderMap) {
    let mut request = RequestBody::parse(body.as_bytes().to_vec()).unwrap();
    restore_claude_code_identity(&mut request, headers);
    assert_eq!(request.into_raw(), body.as_bytes());
}

#[test]
fn classifier_request_gains_the_identity_block() {
    let prompt = classifier_prompt();
    let out = run(
        json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stop_sequences": ["</block>"],
            "system": [
                {"type": "text", "text": prompt},
                {"type": "text", "text": "Session context."},
            ],
        }),
        &HeaderMap::new(),
    );

    let system = out["system"].as_array().unwrap();
    assert_eq!(system.len(), 3);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(system[1]["text"], prompt);
    assert_eq!(system[2]["text"], "Session context.");
    // Nothing else about the request is disturbed.
    assert_eq!(out["model"], "claude-opus-5");
    assert_eq!(out["max_tokens"], 64);
    assert_eq!(out["stop_sequences"], json!(["</block>"]));
}

#[test]
fn claude_code_main_loop_is_untouched() {
    let body = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},{"type":"text","text":"more"}]}"#;

    assert_verbatim(body, &HeaderMap::new());
}

#[test]
fn claude_desktop_chat_and_cowork_are_untouched() {
    // The shape measured through the relay: both Claude Desktop surfaces
    // lead with the billing attribution block, three system blocks in all,
    // and every one of their turns was accepted upstream. The gateway must
    // not rewrite them to claim they are Claude Code. (The `cc_entrypoint`
    // value sat beyond the capture window, so the one here stands in.)
    let body = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.219.f52; cc_entrypoint=desktop;"},{"type":"text","text":"You are Claude, a helpful assistant."},{"type":"text","text":"Workspace context."}]}"#;

    assert_verbatim(body, &HeaderMap::new());
}

#[test]
fn a_third_party_client_with_its_own_system_prompt_is_untouched() {
    let body =
        r#"{"model":"claude-opus-5","system":[{"type":"text","text":"You are a triage bot."}]}"#;

    assert_verbatim(body, &HeaderMap::new());
}

#[test]
fn a_classifier_that_already_carries_a_billing_block_is_untouched() {
    // Either accepted marker is sufficient upstream, so a classifier
    // request carrying the billing block needs no identity block.
    let body = format!(
        r#"{{"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.226.000; cc_entrypoint=cli;"}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
    );

    assert_verbatim(&body, &HeaderMap::new());
}

#[test]
fn custom_string_system_prompt_is_untouched() {
    let body = r#"{"model":"claude-opus-5","system":"You are a Python expert"}"#;

    assert_verbatim(body, &HeaderMap::new());
}

#[test]
fn request_without_a_system_prompt_is_untouched() {
    // Claude Desktop's gateway connection test reaches the relay this way —
    // a `max_tokens: 1` availability probe with no system prompt at all,
    // which upstream accepted.
    let body = r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1}"#;

    assert_verbatim(body, &HeaderMap::new());
}

#[test]
fn classifier_that_already_carries_the_identity_is_untouched() {
    let body = format!(
        r#"{{"system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
    );

    assert_verbatim(&body, &HeaderMap::new());
}

#[test]
fn agent_sdk_identity_also_suppresses_injection() {
    let body = format!(
        r#"{{"system":[{{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK."}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
    );

    assert_verbatim(&body, &HeaderMap::new());
}

#[test]
fn a_classifier_prompt_behind_another_block_is_untouched() {
    // Deliberately narrower than "the sentence appears somewhere": every
    // classifier request captured on the wire carried the prompt at
    // `system[0]`, and the trigger is client-supplied text, so matching
    // deeper would let an arbitrary prompt pull the rewrite in behind a
    // block of its own choosing. If a future Claude Code release moves the
    // prompt, this stops matching — the `debug` line in
    // `restore_claude_code_identity` is what makes that visible.
    let body = format!(
        r#"{{"system":[{{"type":"image","source":{{}}}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
    );

    assert_verbatim(&body, &HeaderMap::new());
}

#[test]
fn a_leading_block_without_text_is_not_a_classifier_request() {
    let body = r#"{"system":[{"type":"image","source":{}}]}"#;

    assert_verbatim(body, &HeaderMap::new());
}

#[test]
fn leading_whitespace_still_identifies_the_classifier() {
    let out = run(
        json!({
            "system": [{"type": "text", "text": format!("\n  {}", classifier_prompt())}],
        }),
        &HeaderMap::new(),
    );

    assert_eq!(out["system"].as_array().unwrap().len(), 2);
    assert_eq!(out["system"][0]["text"], CLAUDE_CODE_IDENTITY);
}

#[test]
fn identity_is_inserted_ahead_of_a_cache_control_block() {
    let out = run(
        json!({
            "system": [{
                "type": "text",
                "text": classifier_prompt(),
                "cache_control": {"type": "ephemeral"},
            }],
        }),
        &HeaderMap::new(),
    );

    let system = out["system"].as_array().unwrap();
    assert_eq!(system[0]["text"], CLAUDE_CODE_IDENTITY);
    assert!(system[0].get("cache_control").is_none());
    assert_eq!(system[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn a_string_system_carrying_the_classifier_prompt_is_untouched() {
    // Claude Code always sends the classifier prompt as a block. A string
    // in this shape is some other client, so it keeps passthrough.
    let body = format!(r#"{{"system":"{CLASSIFIER_PROMPT_PREFIX} …"}}"#);

    assert_verbatim(&body, &HeaderMap::new());
}

#[test]
fn unexpected_system_type_is_left_alone() {
    // Upstream will reject this on its own terms; reshaping it here would
    // turn a clear client error into a confusing gateway-authored request.
    assert_verbatim(r#"{"system":42}"#, &HeaderMap::new());
}

#[test]
fn non_object_request_is_left_alone() {
    assert_verbatim(r#"["not","an","object"]"#, &HeaderMap::new());
}

#[test]
fn empty_block_array_is_untouched() {
    assert_verbatim(r#"{"system":[]}"#, &HeaderMap::new());
}

#[test]
fn a_classifier_request_is_detected() {
    assert!(is_classifier_request(&json!({
        "system": [{"type": "text", "text": classifier_prompt()}],
    })));
}

#[test]
fn a_classifier_request_carrying_an_accepted_marker_is_still_detected() {
    // The accepted-marker list suppresses the identity *repair*, which is
    // redundant once a marker sits anywhere in the array. It must not
    // suppress detection: the request is still the classifier's, and still
    // the one an operator pinned a model to.
    let request = json!({
        "system": [
            {"type": "text", "text": classifier_prompt()},
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.226.000;"},
        ],
    });

    assert_eq!(super::needs_identity(&request, &HeaderMap::new()), None);
    assert!(is_classifier_request(&request));
}

#[test]
fn an_ordinary_request_is_not_a_classifier_request() {
    assert!(!is_classifier_request(&json!({
        "system": [{"type": "text", "text": "You are a triage bot."}],
    })));
}

#[test]
fn a_string_system_is_not_a_classifier_request() {
    assert!(!is_classifier_request(&json!({
        "system": format!("{CLASSIFIER_PROMPT_PREFIX} …"),
    })));
}

#[test]
fn a_request_without_a_system_prompt_is_not_a_classifier_request() {
    assert!(!is_classifier_request(&json!({"model": "claude-opus-5"})));
}

#[test]
fn the_override_is_none_when_the_key_is_unset() {
    let request = json!({"system": [{"type": "text", "text": classifier_prompt()}]});

    assert_eq!(
        classifier_upstream_model(&anthropic_provider(), &request),
        None
    );
}

#[test]
fn the_override_applies_only_to_a_classifier_request() {
    let mut provider = anthropic_provider();
    provider.classifier_model = Some("claude-sonnet-5".to_string());

    assert_eq!(
        classifier_upstream_model(
            &provider,
            &json!({"system": [{"type": "text", "text": classifier_prompt()}]}),
        ),
        Some("claude-sonnet-5"),
    );
    assert_eq!(
        classifier_upstream_model(
            &provider,
            &json!({"system": [{"type": "text", "text": "You are a triage bot."}]}),
        ),
        None,
    );
}

fn claude_code_session() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-claude-code-session-id",
        HeaderValue::from_static("5b1c0c43-session"),
    );
    headers
}

#[test]
fn a_claude_code_side_query_without_a_marker_gains_the_identity_block() {
    let out = run(
        json!({
            "model": "claude-haiku-4-5",
            "system": [{"type": "text", "text": "Summarize the diff."}],
        }),
        &claude_code_session(),
    );

    let system = out["system"].as_array().unwrap();
    assert_eq!(system.len(), 2);
    assert_eq!(
        system[0],
        json!({"type": "text", "text": CLAUDE_CODE_IDENTITY})
    );
    assert_eq!(system[1]["text"], "Summarize the diff.");
    assert_eq!(out["model"], "claude-haiku-4-5");
}

#[test]
fn a_claude_cli_user_agent_alone_identifies_claude_code() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "user-agent",
        HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"),
    );

    let out = run(
        json!({"system": [{"type": "text", "text": "Summarize the diff."}]}),
        &headers,
    );

    assert_eq!(out["system"][0]["text"], CLAUDE_CODE_IDENTITY);
}

#[test]
fn a_claude_code_string_system_becomes_identity_then_the_original_text() {
    let out = run(
        json!({"system": "Summarize the diff."}),
        &claude_code_session(),
    );

    assert_eq!(
        out["system"],
        json!([
            {"type": "text", "text": CLAUDE_CODE_IDENTITY},
            {"type": "text", "text": "Summarize the diff."},
        ])
    );
}

#[test]
fn an_agent_sdk_request_is_judged_like_claude_code() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "user-agent",
        HeaderValue::from_static("claude-cli/2.1.300 (external, sdk-ts)"),
    );

    assert_verbatim(
        r#"{"system":"You are a Claude agent, built on Anthropic's Claude Agent SDK."}"#,
        &headers,
    );
    let out = run(json!({"system": "You are a triage bot."}), &headers);
    assert_eq!(out["system"][0]["text"], CLAUDE_CODE_IDENTITY);
}

#[test]
fn a_claude_code_blank_string_system_becomes_the_identity_alone() {
    for blank in ["", "  \n"] {
        let out = run(json!({"system": blank}), &claude_code_session());

        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": CLAUDE_CODE_IDENTITY}])
        );
    }
}

#[test]
fn a_claude_code_request_without_a_system_gains_one() {
    let out = run(
        json!({"model": "claude-haiku-4-5", "max_tokens": 1}),
        &claude_code_session(),
    );

    assert_eq!(
        out["system"],
        json!([{"type": "text", "text": CLAUDE_CODE_IDENTITY}])
    );
    assert_eq!(out["max_tokens"], 1);
}

#[test]
fn a_claude_code_null_system_gains_the_identity_block() {
    let out = run(json!({"system": null}), &claude_code_session());

    assert_eq!(
        out["system"],
        json!([{"type": "text", "text": CLAUDE_CODE_IDENTITY}])
    );
}

#[test]
fn a_claude_code_request_carrying_the_billing_block_is_untouched() {
    let body = r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.226.000; cc_entrypoint=cli;"},{"type":"text","text":"Summarize the diff."}]}"#;

    assert_verbatim(body, &claude_code_session());
}

#[test]
fn a_claude_code_main_loop_request_is_untouched() {
    let body = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},{"type":"text","text":"more"}]}"#;

    assert_verbatim(body, &claude_code_session());
}

#[test]
fn a_claude_code_string_system_opening_with_the_identity_is_untouched() {
    let body =
        r#"{"system":"  You are Claude Code, Anthropic's official CLI for Claude. Be brief."}"#;

    assert_verbatim(body, &claude_code_session());
}

#[test]
fn a_claude_code_request_with_an_unexpected_system_type_is_left_alone() {
    assert_verbatim(r#"{"system":42}"#, &claude_code_session());
}

#[test]
fn a_claude_code_non_object_request_is_left_alone() {
    assert_verbatim(r#"["not","an","object"]"#, &claude_code_session());
}

#[test]
fn a_non_claude_code_client_string_system_is_untouched() {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_static("my-bot/1.0"));

    assert_verbatim(r#"{"system":"You are a triage bot."}"#, &headers);
}

#[test]
fn a_classifier_request_is_repaired_without_claude_code_headers() {
    let out = run(
        json!({"system": [{"type": "text", "text": classifier_prompt()}]}),
        &HeaderMap::new(),
    );

    assert_eq!(out["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(
        super::needs_identity(
            &json!({"system": [{"type": "text", "text": classifier_prompt()}]}),
            &HeaderMap::new(),
        ),
        Some(super::Repair::Classifier),
    );
}
