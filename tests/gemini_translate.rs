use serde_json::json;
use shunt::model::gemini::{map_gemini_error, translate_gemini_error_val, GeminiSseMachine};
use shunt::model::gemini_request::{translate_request, wrap_code_assist_envelope};

#[test]
fn test_translate_plain_user_message() {
    let request = json!({
        "messages": [
            { "role": "user", "content": "Hello, Gemini!" }
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert!(translated.get("thinkingConfig").is_none());
    assert_eq!(translated["contents"][0]["role"], "user");
    assert_eq!(
        translated["contents"][0]["parts"][0]["text"],
        "Hello, Gemini!"
    );
}

#[test]
fn test_translate_system_and_tools() {
    let request = json!({
        "system": "System instructions for Gemini.",
        "messages": [
            { "role": "user", "content": "Fetch data" }
        ],
        "tools": [
            {
                "name": "fetch_data",
                "description": "Fetch data by key",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" }
                    }
                }
            }
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["systemInstruction"]["parts"][0]["text"],
        "System instructions for Gemini."
    );
    let tools = &translated["tools"][0]["functionDeclarations"];
    assert_eq!(tools[0]["name"], "fetch_data");
    assert_eq!(tools[0]["description"], "Fetch data by key");
}

#[test]
fn test_wrap_code_assist_envelope() {
    let inner = json!({ "contents": [] });
    let envelope = wrap_code_assist_envelope("gemini-3.1-pro-preview", "test-project-999", inner);

    assert_eq!(envelope["model"], "gemini-3.1-pro-preview");
    assert_eq!(envelope["project"], "test-project-999");
    assert!(envelope.get("request").is_some());
}

#[test]
fn test_gemini_sse_machine_text_and_finish() {
    let mut machine = GeminiSseMachine::new("gemini-3-flash-preview");

    let chunk = json!({
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello world!"}],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 3
        }
    });

    let events = machine.process_chunk(&chunk);
    assert_eq!(events[0].event, "message_start");
    assert_eq!(events[0].data["message"]["model"], "gemini-3-flash-preview");
    assert_eq!(events[1].event, "content_block_start");
    assert_eq!(events[2].event, "content_block_delta");
    assert_eq!(events[2].data["delta"]["text"], "Hello world!");
    assert_eq!(events[3].event, "content_block_stop");
    assert_eq!(events[4].event, "message_delta");
    assert_eq!(events[4].data["delta"]["stop_reason"], "end_turn");
    assert_eq!(events[4].data["usage"]["output_tokens"], 3);
    assert_eq!(events[5].event, "message_stop");
}

#[test]
fn test_gemini_error_translation() {
    let err_val = json!({
        "code": 429,
        "message": "Resource has been exhausted (e.g. check quota).",
        "status": "RESOURCE_EXHAUSTED"
    });

    let err_env = translate_gemini_error_val(&err_val);
    assert_eq!(err_env["type"], "error");
    assert_eq!(err_env["error"]["type"], "rate_limit_error");

    let mapped = map_gemini_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &err_val.to_string());
    assert_eq!(
        mapped.message,
        "Resource has been exhausted (e.g. check quota)."
    );
}

#[test]
fn test_tool_result_uses_original_function_name() {
    let request = json!({
        "messages": [
            { "role": "user", "content": "Check the weather" },
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "get_weather",
                    "input": { "city": "Paris" }
                }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "sunny"
                }]
            }
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["contents"][2]["parts"][0]["functionResponse"]["name"],
        "get_weather"
    );
}

#[test]
fn test_disabled_thinking_sends_zero_budget() {
    let request = json!({
        "thinking": { "type": "disabled" },
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        0
    );
    assert!(translated.get("thinkingConfig").is_none());
}

#[test]
fn test_gemini_sse_machine_finishes_on_eof() {
    let mut machine = GeminiSseMachine::new("gemini-3-flash-preview");
    let chunk = json!({
        "candidates": [{
            "content": { "parts": [{ "text": "partial" }], "role": "model" }
        }]
    });
    let _ = machine.process_chunk(&chunk);

    let mut events = Vec::new();
    machine.finish(&mut events);

    assert_eq!(events[0].event, "content_block_stop");
    assert_eq!(events[1].event, "message_delta");
    assert_eq!(events[1].data["delta"]["stop_reason"], "end_turn");
    assert_eq!(events[2].event, "message_stop");

    let mut duplicate_events = Vec::new();
    machine.finish(&mut duplicate_events);
    assert!(duplicate_events.is_empty());
}

#[test]
fn test_gemini_sse_machine_non_streaming_accumulation() {
    let mut machine = GeminiSseMachine::new("gemini-2.5-pro");

    // Chunk 1: text part 1
    let chunk1 = json!({
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello "}],
                "role": "model"
            }
        }]
    });
    let _ = machine.process_chunk(&chunk1);

    // Chunk 2: text part 2 + tool_use
    let chunk2 = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {"text": "world!"},
                    {
                        "functionCall": {
                            "name": "read_file",
                            "args": { "path": "main.rs" }
                        }
                    }
                ],
                "role": "model"
            },
            "finishReason": "STOP"
        }]
    });
    let _ = machine.process_chunk(&chunk2);

    let final_json = machine.final_json();
    assert_eq!(final_json["type"], "message");
    assert_eq!(final_json["model"], "gemini-2.5-pro");
    assert_eq!(final_json["stop_reason"], "tool_use");

    let content = final_json["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "Hello world!");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["name"], "read_file");
    assert_eq!(content[1]["input"]["path"], "main.rs");
}
