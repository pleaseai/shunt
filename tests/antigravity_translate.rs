use serde_json::json;
use shunt::adapters::antigravity::{
    extract_antigravity_prompt, find_agy_binary, format_antigravity_json, format_antigravity_sse,
};

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
fn test_format_antigravity_json() {
    let json_val = format_antigravity_json("gemini-3.1-pro", "Hello from Antigravity!");
    assert_eq!(json_val["model"], "gemini-3.1-pro");
    assert_eq!(json_val["content"][0]["text"], "Hello from Antigravity!");
    assert_eq!(json_val["stop_reason"], "end_turn");
}

#[test]
fn test_format_antigravity_sse() {
    let sse_text = format_antigravity_sse("gemini-3.1-pro", "Streaming Hello!");
    assert!(sse_text.contains("event: message_start"));
    assert!(sse_text.contains("event: content_block_delta"));
    assert!(sse_text.contains("Streaming Hello!"));
    assert!(sse_text.contains("event: message_stop"));
}
