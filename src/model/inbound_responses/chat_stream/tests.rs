use super::*;

use crate::model::responses::parse_sse_events;

/// Feed a stream of SSE `data:` payloads through the machine, one chunk at a
/// time, and collect every Responses frame it produced.
fn drive(machine: &mut ChatSseMachine, chunks: &[&str]) -> Vec<String> {
    chunks
        .iter()
        .flat_map(|chunk| machine.apply(chunk))
        .collect()
}

fn parsed(frames: &[String]) -> Vec<(String, Value)> {
    parse_sse_events(&frames.concat())
        .into_iter()
        .map(|event| (event.event.unwrap_or_default(), event.data))
        .collect()
}

fn names(frames: &[String]) -> Vec<String> {
    parsed(frames).into_iter().map(|(name, _)| name).collect()
}

fn frame(frames: &[String], name: &str) -> Value {
    parsed(frames)
        .into_iter()
        .find(|(event, _)| event == name)
        .unwrap_or_else(|| panic!("no {name} frame in {:?}", names(frames)))
        .1
}

/// The `type` of each item in a terminal event's `response.output`.
fn output_types(response: &Value) -> Vec<String> {
    response["output"]
        .as_array()
        .expect("output is an array")
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

const TEXT_TURN: &[&str] = &[
    r#"{"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
    r#"{"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hi "}}]}"#,
    r#"{"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"there"}}]}"#,
    r#"{"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    r#"{"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":150,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens":7,"completion_tokens_details":{"reasoning_tokens":3}}}"#,
    "[DONE]",
];

#[test]
fn a_text_turn_becomes_the_full_responses_event_sequence() {
    let mut machine = ChatSseMachine::new("deepseek-chat");
    let frames = drive(&mut machine, TEXT_TURN);
    assert_eq!(
        names(&frames),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        frame(&frames, "response.output_text.done")["text"],
        "Hi there"
    );
    let response = frame(&frames, "response.completed")["response"].clone();
    assert_eq!(response["status"], "completed");
    assert_eq!(response["model"], "deepseek-chat");
    // The usage chunk arrives after the one carrying `finish_reason`, so the
    // terminal event has to wait for `[DONE]` to report it.
    assert_eq!(response["usage"]["input_tokens"], 150);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        40
    );
    assert_eq!(response["usage"]["output_tokens"], 7);
    assert_eq!(
        response["usage"]["output_tokens_details"]["reasoning_tokens"],
        3
    );
    assert!(machine.is_terminal());
}

#[test]
fn a_tool_call_concatenates_its_argument_fragments() {
    let mut machine = ChatSseMachine::new("chat-test");
    let frames = drive(
        &mut machine,
        &[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    );
    let deltas: Vec<String> = parsed(&frames)
        .into_iter()
        .filter(|(name, _)| name == "response.function_call_arguments.delta")
        .map(|(_, data)| data["delta"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(deltas, vec!["{\"path\":", "\"a.rs\"}"]);
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["call_id"], "call_9");
    assert_eq!(item["name"], "read_file");
    assert_eq!(item["arguments"], "{\"path\":\"a.rs\"}");
    assert_eq!(
        frame(&frames, "response.function_call_arguments.done")["arguments"],
        "{\"path\":\"a.rs\"}"
    );
}

#[test]
fn parallel_tool_calls_keep_their_own_ids_and_arguments() {
    let mut machine = ChatSseMachine::new("chat-test");
    let frames = drive(
        &mut machine,
        &[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"ls","arguments":"{\"dir\":\".\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"cat","arguments":"{\"file\":\"a\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    );
    let response = frame(&frames, "response.completed")["response"].clone();
    assert_eq!(
        output_types(&response),
        vec!["function_call", "function_call"]
    );
    let output = response["output"].as_array().expect("output is an array");
    assert_eq!(output[0]["call_id"], "call_a");
    assert_eq!(output[0]["name"], "ls");
    assert_eq!(output[0]["arguments"], "{\"dir\":\".\"}");
    assert_eq!(output[1]["call_id"], "call_b");
    assert_eq!(output[1]["name"], "cat");
    assert_eq!(output[1]["arguments"], "{\"file\":\"a\"}");
}

#[test]
fn reasoning_deltas_open_a_reasoning_item_before_the_message() {
    let mut machine = ChatSseMachine::new("chat-test");
    let frames = drive(
        &mut machine,
        &[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"weighing "}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"it up"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Answer."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ],
    );
    assert_eq!(
        frame(&frames, "response.reasoning_summary_text.done")["text"],
        "weighing it up"
    );
    let response = frame(&frames, "response.completed")["response"].clone();
    assert_eq!(output_types(&response), vec!["reasoning", "message"]);
    // Chat Completions never signs its reasoning, so there is nothing to
    // round-trip as encrypted_content.
    assert!(response["output"][0].get("encrypted_content").is_none());
}

#[test]
fn a_length_finish_reason_yields_response_incomplete() {
    let mut machine = ChatSseMachine::new("chat-test");
    let frames = drive(
        &mut machine,
        &[
            r#"{"choices":[{"index":0,"delta":{"content":"cut"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
            "[DONE]",
        ],
    );
    let response = frame(&frames, "response.incomplete")["response"].clone();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert!(!names(&frames).contains(&"response.completed".to_string()));
}

#[test]
fn an_error_chunk_fails_the_response_and_ends_the_turn() {
    let mut machine = ChatSseMachine::new("chat-test");
    let frames = drive(
        &mut machine,
        &[
            r#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
            r#"{"error":{"type":"server_error","code":"internal_error","message":"backend blew up"}}"#,
        ],
    );
    // The open message is closed before the failure, so the partial answer is
    // still a well-formed item.
    assert_eq!(
        frame(&frames, "response.output_text.done")["text"],
        "partial"
    );
    let response = frame(&frames, "response.failed")["response"].clone();
    assert_eq!(response["status"], "failed");
    assert_eq!(
        response["error"],
        json!({"code": "internal_error", "message": "backend blew up"})
    );
    assert!(machine.is_terminal());
    // Everything after the failure is dropped, including a late `[DONE]`.
    assert!(drive(&mut machine, &["[DONE]"]).is_empty());
    assert!(machine.finish().is_empty());
}

#[test]
fn finish_closes_open_items_and_fails_a_truncated_stream() {
    let mut machine = ChatSseMachine::new("chat-test");
    let mut frames = drive(
        &mut machine,
        &[r#"{"choices":[{"index":0,"delta":{"content":"cut"}}]}"#],
    );
    frames.extend(machine.finish());
    assert_eq!(
        frame(&frames, "response.output_item.done")["item"]["status"],
        "completed"
    );
    let response = frame(&frames, "response.failed")["response"].clone();
    assert_eq!(response["error"]["code"], TRUNCATED_CODE);
    assert_eq!(output_types(&response), vec!["message"]);
    assert!(machine.is_terminal());
}

#[test]
fn an_unparsable_chunk_is_ignored() {
    let mut machine = ChatSseMachine::new("chat-test");
    assert!(drive(&mut machine, &["{not json"]).is_empty());
    // The turn continues as if the chunk had never arrived.
    let frames = drive(
        &mut machine,
        &[
            r#"{"choices":[{"index":0,"delta":{"content":"ok"}}]}"#,
            "[DONE]",
        ],
    );
    assert_eq!(names(&frames)[0], "response.created");
    assert_eq!(frame(&frames, "response.output_text.done")["text"], "ok");
}

#[test]
fn translate_response_maps_text_and_tool_calls() {
    let response = translate_response(
        &json!({
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "Reading it.",
                    "tool_calls": [{
                        "id": "call_3",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"},
                    }],
                },
            }],
            "usage": {
                "prompt_tokens": 25,
                "prompt_tokens_details": {"cached_tokens": 5},
                "completion_tokens": 11,
            },
        }),
        "chat-test",
    );
    assert_eq!(response["object"], "response");
    assert_eq!(response["status"], "completed");
    assert!(response["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("resp_")));
    let output = response["output"].as_array().expect("output is an array");
    assert_eq!(
        output[0]["content"],
        json!([{"type": "output_text", "text": "Reading it.", "annotations": []}])
    );
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "call_3");
    assert_eq!(output[1]["arguments"], "{\"path\":\"a.rs\"}");
    assert_eq!(response["usage"]["input_tokens"], 25);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        5
    );
    assert_eq!(response["usage"]["output_tokens"], 11);
}

#[test]
fn translate_response_reports_a_length_completion_as_incomplete() {
    let response = translate_response(
        &json!({
            "choices": [{"finish_reason": "length", "message": {"content": "cut"}}],
        }),
        "chat-test",
    );
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
}

#[test]
fn translate_error_passes_the_openai_envelope_and_hides_unknown_bodies() {
    assert_eq!(
        translate_error(&json!({
            "error": {"type": "invalid_request_error", "message": "model not found", "param": "model"},
        })),
        json!({"error": {
            "type": "invalid_request_error",
            "message": "model not found",
            "code": null,
            "param": "model",
        }})
    );
    let unknown = translate_error(&json!({"detail": "upstream said something else"}));
    assert_eq!(unknown["error"]["type"], "api_error");
    assert!(!unknown.to_string().contains("upstream said something else"));
}
