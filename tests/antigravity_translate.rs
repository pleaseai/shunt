use serde_json::json;
use shunt::adapters::antigravity::{
    extract_antigravity_prompt, find_agy_binary,
    models::{parse_models, resolve_effort, EffortChoice},
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
fn test_explicit_unsupported_effort_is_reported_not_clamped() {
    let matrix = parse_models(AGY_MODELS);

    // An operator who wrote `effort = "medium"` gets told it is unavailable.
    // Silently running `high` instead would change cost, latency and quota
    // while leaving the config file claiming otherwise.
    match resolve_effort(&matrix, "gemini-3.1-pro", Some("medium")) {
        EffortChoice::Unsupported {
            model,
            requested,
            supported,
        } => {
            assert_eq!(model, "gemini-3.1-pro");
            assert_eq!(requested, "medium");
            assert_eq!(supported, vec!["high".to_string(), "low".to_string()]);
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }

    // A supported explicit value passes through untouched.
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.1-pro", Some("low")),
        EffortChoice::Use("low".to_string())
    );
}

#[test]
fn test_gateway_default_effort_is_clamped_not_rejected() {
    let matrix = parse_models(AGY_MODELS);

    // No configured effort: the default is shunt's own choice, so clamping it
    // is the gateway doing its job rather than overriding operator intent.
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.1-pro", None),
        EffortChoice::Use("high".to_string()),
        "medium is unavailable, so the default clamps to the nearest, preferring stronger"
    );
    assert_eq!(
        resolve_effort(&matrix, "gemini-3.6-flash", None),
        EffortChoice::Use("medium".to_string())
    );
}

#[test]
fn test_unknown_model_defers_to_the_cli() {
    let matrix = parse_models(AGY_MODELS);

    // Nothing here is authoritative for a model we have not seen, so pass a
    // configured value through and let agy validate it.
    assert_eq!(
        resolve_effort(&matrix, "gemini-9-future", Some("medium")),
        EffortChoice::Use("medium".to_string())
    );
    // With no configured value, omit the flag: agy's own rejection enumerates
    // the valid levels, which beats guessing one.
    assert_eq!(
        resolve_effort(&matrix, "gemini-9-future", None),
        EffortChoice::Omit
    );
    // A model that takes no --effort flag gets none.
    assert_eq!(
        resolve_effort(&matrix, "claude-sonnet-4-6", Some("high")),
        EffortChoice::Omit
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
fn test_premature_eof_is_not_reported_as_success() {
    let mut t = Translator::new("gemini", "msg_test");
    // Text streamed, then the CLI died before emitting its terminal `result`.
    t.on_line(
        r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"working on it"}}"#,
    );
    assert_eq!(t.end(), None, "no result event was seen");

    let out = t.finish_with_error();

    // Headers are already committed, so the failure has to travel in-stream.
    // Closing with a bare message_stop would present a crash as a normal turn.
    assert!(out.contains("event: error"), "must emit an SSE error event");
    assert!(out.contains("api_error"));
    assert!(!out.contains("end_turn"), "must not claim a clean end_turn");
    assert!(out.contains("event: message_stop"));
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

/// Build a request whose system prompt names `dir` as the working directory,
/// the way an agent harness states the caller's cwd.
fn request_naming(dir: &std::path::Path) -> serde_json::Value {
    json!({
        "system": format!(
            "You are an agent.\n<env>\nWorking directory: {}\nPlatform: darwin\n</env>",
            dir.display()
        ),
        "messages": [{ "role": "user", "content": "hi" }]
    })
}

#[test]
fn test_workspace_accepts_prompt_path_inside_a_configured_root() {
    let root = std::env::temp_dir().join(format!("shunt-agy-root-{}", std::process::id()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();

    let roots = vec![root.display().to_string()];
    let resolved = resolve_workspace(&request_naming(&project), &roots).unwrap();

    let expected = project.canonicalize().unwrap();
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(resolved, expected);
}

#[test]
fn test_workspace_refuses_prompt_path_outside_every_root() {
    let root = std::env::temp_dir().join(format!("shunt-agy-in-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("shunt-agy-out-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let roots = vec![root.display().to_string()];
    let resolved = resolve_workspace(&request_naming(&outside), &roots).unwrap();

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
    // Falls back to the gateway's own directory rather than honouring a path
    // the operator never authorised.
    assert_ne!(resolved, outside);
    assert!(resolved.is_dir());
}

#[test]
fn test_workspace_refuses_traversal_out_of_a_root() {
    let base = std::env::temp_dir().join(format!("shunt-agy-trav-{}", std::process::id()));
    let root = base.join("allowed");
    let secret = base.join("secret");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&secret).unwrap();

    // Textually prefixed by the root, but canonicalizes outside it.
    let escape = root.join("..").join("secret");
    let roots = vec![root.display().to_string()];
    let resolved = resolve_workspace(&request_naming(&escape), &roots).unwrap();

    std::fs::remove_dir_all(&base).ok();
    assert!(
        !resolved.ends_with("secret"),
        "`..` must not escape an allowed root; got {}",
        resolved.display()
    );
}

#[test]
fn test_workspace_ignores_prompt_path_when_no_roots_configured() {
    let dir = std::env::temp_dir().join(format!("shunt-agy-noroot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // The safe default: without an explicit allowlist, client-controlled text
    // never chooses where a permission-skipping agent runs.
    let resolved = resolve_workspace(&request_naming(&dir), &[]).unwrap();

    std::fs::remove_dir_all(&dir).ok();
    assert_ne!(resolved, dir);
    assert!(resolved.is_dir());
}

#[test]
fn test_stderr_truncation_never_splits_a_utf8_character() {
    use shunt::adapters::antigravity::truncate;

    // A multi-byte character straddling the limit must be dropped whole, not
    // sliced into invalid UTF-8 (which would panic on a naive byte slice).
    let text = "é".repeat(2000);
    let cut = truncate(&text, 2001);
    assert!(cut.len() <= 2001);
    assert!(cut.chars().all(|c| c == 'é'));
    // Short input is returned intact.
    assert_eq!(truncate("short", 2000), "short");
}
