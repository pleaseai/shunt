use serde_json::json;
use shunt::adapters::antigravity::{
    extract_antigravity_prompt, find_agy_binary,
    models::{parse_models, resolve_effort},
    resolve_workspace,
    stream::{AgyEnd, Translator},
};

/// Real `agy models` output shape: `<model>-<effort>` per usable combination,
/// and a bare model name when it takes no `--effort` flag.
const AGY_MODELS: &str = "\
gemini-3.6-flash-high
gemini-3.6-flash-medium
gemini-3.6-flash-low
gemini-3.1-pro-high
gemini-3.1-pro-low
claude-sonnet-4-6
";

#[test]
fn test_antigravity_prompt_extraction() {
    let req = json!({
        "system": "You are a helpful coding assistant.",
        "messages": [
            { "role": "user", "content": "Write a hello world program in Rust." },
            { "role": "assistant", "content": "Here is the code." },
            { "role": "user", "content": "Now add tests." }
        ]
    });

    let prompt = extract_antigravity_prompt(&req);
    assert!(prompt.contains("You are a helpful coding assistant."));
    assert!(prompt.contains("user: Write a hello world program in Rust."));
    assert!(prompt.contains("assistant: Here is the code."));
    assert!(prompt.contains("user: Now add tests."));
}

#[test]
fn test_find_agy_binary_honors_env_override() {
    let fake_bin = std::env::temp_dir().join(format!("shunt-test-agy-{}", std::process::id()));
    std::fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();
    std::env::set_var("AGY_BIN", &fake_bin);

    let found = find_agy_binary();

    std::env::remove_var("AGY_BIN");
    std::fs::remove_file(&fake_bin).unwrap();
    assert_eq!(found, Some(fake_bin));
}

#[test]
fn test_parse_models_builds_effort_matrix() {
    let matrix = parse_models(AGY_MODELS);

    let flash = matrix.get("gemini-3.6-flash").expect("flash entry");
    assert!(flash.contains("low") && flash.contains("medium") && flash.contains("high"));

    let pro = matrix.get("gemini-3.1-pro").expect("pro entry");
    assert!(pro.contains("low") && pro.contains("high"));
    assert!(
        !pro.contains("medium"),
        "gemini-3.1-pro must not advertise medium; agy rejects that pair"
    );

    // A bare line records the model with no effort levels.
    assert!(matrix
        .get("claude-sonnet-4-6")
        .expect("bare entry")
        .is_empty());
}

#[test]
fn test_resolve_effort_clamps_unsupported_level_upward() {
    let matrix = parse_models(AGY_MODELS);

    // The regression that made every gemini-3.1-pro run invalid.
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.1-pro", Some("medium")),
        Some("high".to_string()),
        "medium is unsupported and must clamp to the nearest level, preferring the stronger"
    );
    // Default (no route effort) goes through the same clamp.
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.1-pro", None),
        Some("high".to_string())
    );
    // Supported levels pass through untouched.
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.1-pro", Some("low")),
        Some("low".to_string())
    );
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.6-flash", Some("medium")),
        Some("medium".to_string())
    );
    // A model that takes no --effort flag gets none.
    assert_eq!(
        resolve_effort(&matrix, "claude-sonnet-4-6", Some("high")),
        None
    );
    // Unknown models defer to the CLI's own validation.
    assert_eq!(
        resolve_effort(&matrix, "gemini-9-future", Some("medium")),
        Some("medium".to_string())
    );
}

#[test]
fn test_translator_streams_text_and_real_usage() {
    let mut t = Translator::new("claude-gemini-3.1-pro-via-antigravity", "msg_test");
    let mut out = String::new();

    out.push_str(&t.on_line(r#"{"event":"init","init":{"model":"gemini-3.1-pro"}}"#));
    out.push_str(&t.on_line(
        r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"Changed the message to GOODBYE.","usage":{"input_tokens":40633,"output_tokens":1548,"cache_read_tokens":134496}}}"#,
    ));
    out.push_str(&t.on_line(
        r#"{"event":"result","result":{"status":"SUCCESS","response":"Changed the message to GOODBYE.","usage":{"input_tokens":40633,"output_tokens":1548,"cache_read_tokens":134496}}}"#,
    ));
    out.push_str(&t.finish());

    assert!(out.contains("event: message_start"));
    assert!(out.contains("event: content_block_delta"));
    assert!(out.contains("Changed the message to GOODBYE."));
    assert!(out.contains("event: message_stop"));
    assert_eq!(t.end(), Some(&AgyEnd::Success));

    // Real counts, not the placeholder 1/len-over-4 the adapter used to invent.
    let usage = t.usage();
    assert_eq!(usage.input_tokens, 40633);
    assert_eq!(usage.output_tokens, 1548);
    assert_eq!(usage.cache_read_tokens, 134496);

    // `result.response` repeats the streamed text; it must not be sent twice.
    assert_eq!(t.text(), "Changed the message to GOODBYE.");
    assert_eq!(out.matches("Changed the message to GOODBYE.").count(), 1);
}

#[test]
fn test_translator_falls_back_to_result_response_without_deltas() {
    let mut t = Translator::new("gemini", "msg_test");
    let out = t.on_line(
        r#"{"event":"result","result":{"status":"SUCCESS","response":"PONG","usage":{"output_tokens":2}}}"#,
    );

    assert!(out.contains("PONG"));
    assert_eq!(t.text(), "PONG");
}

#[test]
fn test_translator_emits_ping_for_tool_steps() {
    let mut t = Translator::new("gemini", "msg_test");
    let out = t.on_line(
        r#"{"event":"step_update","step_update":{"step_type":"tool","state":"ACTIVE","tool_name":"view_file"}}"#,
    );

    // Heartbeats keep a long tool-running turn from looking idle, without
    // polluting the assistant message with tool chatter.
    assert!(out.contains("event: ping"));
    assert!(!out.contains("content_block_delta"));
    assert!(t.text().is_empty());
}

#[test]
fn test_translator_reports_failed_runs() {
    let mut t = Translator::new("gemini", "msg_test");
    t.on_line(
        r#"{"event":"result","result":{"status":"ERROR","response":"","error":"invalid model selection"}}"#,
    );

    assert_eq!(
        t.end(),
        Some(&AgyEnd::Failed("invalid model selection".to_string()))
    );
}

#[test]
fn test_translator_tolerates_garbage_lines() {
    let mut t = Translator::new("gemini", "msg_test");

    assert!(t.on_line("not json at all").is_empty());
    assert!(t.on_line("").is_empty());
    assert!(t.on_line(r#"{"event":"unheard_of"}"#).is_empty());
    // A stream that dies mid-run still closes cleanly.
    assert!(t.finish().contains("event: message_stop"));
}

#[test]
fn test_translator_non_streaming_message_shape() {
    let mut t = Translator::new("claude-gemini-3.1-pro-via-antigravity", "msg_test");
    t.on_line(
        r#"{"event":"result","result":{"status":"SUCCESS","response":"done","usage":{"input_tokens":10,"output_tokens":3}}}"#,
    );

    let msg = t.to_message();
    assert_eq!(msg["model"], "claude-gemini-3.1-pro-via-antigravity");
    assert_eq!(msg["content"][0]["text"], "done");
    assert_eq!(msg["stop_reason"], "end_turn");
    assert_eq!(msg["usage"]["input_tokens"], 10);
    assert_eq!(msg["usage"]["output_tokens"], 3);
}

#[test]
fn test_resolve_workspace_prefers_system_prompt_directory() {
    let dir = std::env::temp_dir().join(format!("shunt-agy-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let req = json!({
        "system": format!(
            "You are an agent.\n<env>\nWorking directory: {}\nPlatform: darwin\n</env>",
            dir.display()
        ),
        "messages": [{ "role": "user", "content": "hi" }]
    });

    let resolved = resolve_workspace(&req);
    std::fs::remove_dir_all(&dir).ok();

    // Compare canonically: temp dirs are symlinked on macOS.
    assert_eq!(
        resolved.file_name(),
        dir.file_name(),
        "the caller's cwd must win over the gateway's own directory"
    );
}

#[test]
fn test_resolve_workspace_ignores_nonexistent_hint() {
    let req = json!({
        "system": "Working directory: /definitely/not/a/real/path/12345",
        "messages": [{ "role": "user", "content": "hi" }]
    });

    let resolved = resolve_workspace(&req);
    assert_ne!(
        resolved,
        std::path::PathBuf::from("/definitely/not/a/real/path/12345")
    );
    assert!(resolved.is_dir(), "must fall back to a usable directory");
}
