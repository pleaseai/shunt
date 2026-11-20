use serde_json::json;

use super::openai_chat_request::translate_request;
use super::openai_chat_response::OpenAiChatSseMachine;

fn user_request() -> serde_json::Value {
    json!({
        "messages": [{"role": "user", "content": "hi"}]
    })
}

#[test]
fn translates_sampling_controls_and_completion_token_cap() {
    let request = json!({
        "max_tokens": 64,
        "temperature": 0.2,
        "top_p": 0.9,
        "stop_sequences": ["END"],
        "thinking": {"type": "disabled"},
        "tools": [],
        "tool_choice": {"type": "auto"},
        "system": "be brief",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let translated = translate_request(&request, "gpt-5", false).expect("text request");
    assert_eq!(translated["max_completion_tokens"], 64);
    assert!(translated.get("max_tokens").is_none());
    assert_eq!(translated["temperature"], 0.2);
    assert_eq!(translated["top_p"], 0.9);
    assert_eq!(translated["stop"], json!(["END"]));
    assert_eq!(translated["messages"][0]["role"], "system");
    assert_eq!(translated["messages"][0]["content"], "be brief");
    assert_eq!(translated["messages"][1]["content"], "hi");
}

#[test]
fn rejects_controls_the_text_slice_cannot_honor() {
    let cases = [
        (
            json!({"tools": {"name": "bad"}, "messages": [{"role": "user", "content": "hi"}]}),
            "tools must be an array",
        ),
        (
            json!({"tools": [{"name": "lookup"}], "messages": [{"role": "user", "content": "hi"}]}),
            "does not support Anthropic tools",
        ),
        (
            json!({"tool_choice": {"type": "any"}, "messages": [{"role": "user", "content": "hi"}]}),
            "tool_choice",
        ),
        (
            json!({"tool_choice": {"type": "tool", "name": "lookup"}, "messages": [{"role": "user", "content": "hi"}]}),
            "tool_choice",
        ),
        (
            json!({"thinking": {"type": "enabled"}, "messages": [{"role": "user", "content": "hi"}]}),
            "thinking",
        ),
        (
            json!({"system": "only", "messages": []}),
            "messages must not be empty",
        ),
        (
            json!({"temperature": "hot", "messages": [{"role": "user", "content": "hi"}]}),
            "temperature must be a number",
        ),
        (
            json!({"stop_sequences": [1], "messages": [{"role": "user", "content": "hi"}]}),
            "stop_sequences must be strings",
        ),
    ];
    for (request, needle) in cases {
        let error = translate_request(&request, "gpt-5", false).expect_err("must fail closed");
        assert!(
            error.message.contains(needle),
            "expected {needle} in {}",
            error.message
        );
    }
}

#[test]
fn accepts_non_forcing_tool_choice_shapes() {
    for choice in [
        json!("auto"),
        json!("none"),
        json!(null),
        json!({"type": "none"}),
    ] {
        let mut request = user_request();
        request["tool_choice"] = choice;
        translate_request(&request, "gpt-5", true).expect("non-forcing tool_choice");
    }
}

#[test]
fn content_filter_does_not_complete_as_end_turn() {
    let mut machine = OpenAiChatSseMachine::new_for_upstream("gpt-5");
    let chunk = json!({
        "choices": [{
            "message": {"role": "assistant", "content": "partial"},
            "finish_reason": "content_filter"
        }]
    });
    let error = machine.process_chunk_checked(&chunk).expect_err("filter");
    assert!(error.to_string().contains("content_filter"));
    assert!(machine.transport_close_checked().is_err());
    assert!(machine.final_json_checked().is_err());
}

#[test]
fn malformed_usage_trailer_blocks_a_later_success() {
    let mut machine = OpenAiChatSseMachine::new_streaming_for_upstream("gpt-5");
    machine
        .process_chunk_checked(&json!({
            "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": null
        }))
        .expect("ordinary null usage stays nonterminal");
    let error = machine
        .process_chunk_checked(&json!({"choices": [], "usage": null}))
        .expect_err("trailer needs a usage object");
    assert!(error.to_string().contains("usage object"));
    assert!(machine.transport_close_checked().is_err());
}

#[test]
fn missing_trailer_choices_block_a_later_success() {
    let mut machine = OpenAiChatSseMachine::new_streaming_for_upstream("gpt-5");
    machine
        .process_chunk_checked(&json!({
            "choices": [{"delta": {"content": "ok"}, "finish_reason": "length"}]
        }))
        .unwrap();
    assert!(machine.process_chunk_checked(&json!({})).is_err());
    assert!(machine.transport_close_checked().is_err());
}

#[test]
fn usage_trailer_is_applied_before_close() {
    let mut machine = OpenAiChatSseMachine::new_for_upstream("gpt-5");
    machine
        .process_chunk_checked(&json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
    let events = machine
        .process_chunk_checked(&json!({
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4}
        }))
        .unwrap();
    assert!(events.is_empty());
    machine.transport_close_checked().unwrap();
    let final_json = machine.final_json_checked().unwrap();
    assert_eq!(final_json["usage"]["input_tokens"], 3);
    assert_eq!(final_json["usage"]["output_tokens"], 4);
    assert_eq!(final_json["stop_reason"], "end_turn");
}
