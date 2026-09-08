use super::*;

use crate::model::inbound_responses::reasoning::decode_thinking;
use crate::model::responses::parse_sse_events;

/// Feed a raw Anthropic SSE body through the machine, one upstream event at a
/// time, and collect every Responses frame it produced.
fn drive(machine: &mut MessagesSseMachine, body: &str) -> Vec<String> {
    parse_sse_events(body)
        .into_iter()
        .flat_map(|event| machine.apply(event))
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

const TEXT_TURN: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":10}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi \"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"there\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: ping\ndata: {\"type\":\"ping\"}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

#[test]
fn a_text_turn_becomes_the_full_responses_event_sequence() {
    let mut machine = MessagesSseMachine::new("claude-sonnet-4-6");
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
    assert_eq!(response["model"], "claude-sonnet-4-6");
    // Anthropic reports the cached and cache-written prompt outside
    // `input_tokens`; Responses folds them into one total with the cached
    // portion as a detail.
    assert_eq!(response["usage"]["input_tokens"], 150);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        40
    );
    assert_eq!(response["usage"]["output_tokens"], 7);
    assert!(machine.is_terminal());
}

#[test]
fn a_tool_use_turn_concatenates_its_argument_fragments() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    );
    let deltas: Vec<String> = parsed(&frames)
        .into_iter()
        .filter(|(name, _)| name == "response.function_call_arguments.delta")
        .map(|(_, data)| data["delta"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(deltas, vec!["{\"path\":", "\"a.rs\"}"]);
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["call_id"], "toolu_9");
    assert_eq!(item["name"], "read_file");
    assert_eq!(item["arguments"], "{\"path\":\"a.rs\"}");
    assert_eq!(
        frame(&frames, "response.function_call_arguments.done")["arguments"],
        "{\"path\":\"a.rs\"}"
    );
}

#[test]
fn a_signed_thinking_block_round_trips_through_encrypted_content() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step one\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ),
    );
    assert_eq!(
        frame(&frames, "response.reasoning_summary_text.delta")["delta"],
        "step one"
    );
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(item["type"], "reasoning");
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "step one"}])
    );
    let encrypted = item["encrypted_content"]
        .as_str()
        .expect("a signed thinking block carries encrypted_content");
    assert_eq!(
        decode_thinking(encrypted),
        Some(Thinking::Signed {
            thinking: "step one".to_string(),
            signature: "sig-xyz".to_string(),
        })
    );
}

#[test]
fn a_redacted_thinking_block_opens_and_closes_in_one_step() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ),
    );
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(item["type"], "reasoning");
    // No visible summary — the payload lives entirely in encrypted_content.
    assert_eq!(item["summary"], json!([]));
    assert_eq!(
        decode_thinking(item["encrypted_content"].as_str().unwrap_or_default()),
        Some(Thinking::Redacted {
            data: "opaque".to_string()
        })
    );
    // The trailing content_block_stop for an already-closed block is inert.
    assert_eq!(
        names(&frames)
            .iter()
            .filter(|n| *n == "response.output_item.done")
            .count(),
        1
    );
}

#[test]
fn interleaved_blocks_keep_their_order_in_the_completed_output() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            // Blocks 0 and 1 are both open while their deltas arrive, and
            // block 2 opens before block 1 closes, so every delta and stop
            // has to be routed by `index` rather than to "the current block".
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"running\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\" it\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"bash\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    );
    let response = frame(&frames, "response.completed")["response"].clone();
    assert_eq!(
        output_types(&response),
        vec!["reasoning", "message", "function_call"]
    );
    // Each delta landed on its own item, not on whichever block opened last.
    assert_eq!(response["output"][1]["content"][0]["text"], "running it");
    assert_eq!(response["output"][0]["summary"][0]["text"], "plan");
}

#[test]
fn a_web_search_block_becomes_a_completed_web_search_call() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srvtoolu_1\",\"name\":\"web_search\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"rust sse\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srvtoolu_1\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    );
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(item["id"], "srvtoolu_1");
    assert_eq!(item["type"], "web_search_call");
    assert_eq!(
        item["action"],
        json!({"type": "search", "query": "rust sse"})
    );
    assert!(names(&frames).contains(&"response.web_search_call.completed".to_string()));
    // The tool result block has no Responses counterpart and adds no item.
    let response = frame(&frames, "response.completed")["response"].clone();
    assert_eq!(output_types(&response), vec!["web_search_call"]);
}

#[test]
fn a_max_tokens_stop_reason_yields_response_incomplete() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":4096}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    );
    let response = frame(&frames, "response.incomplete")["response"].clone();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert_eq!(response["usage"]["output_tokens"], 4096);
    assert!(!names(&frames).contains(&"response.completed".to_string()));
}

#[test]
fn a_context_window_stop_reason_yields_response_incomplete() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"model_context_window_exceeded\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    );
    let response = frame(&frames, "response.incomplete")["response"].clone();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "model_context_window_exceeded"})
    );
    assert!(!names(&frames).contains(&"response.completed".to_string()));
}

#[test]
fn an_error_before_message_start_still_emits_the_startup_frames() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    // The client is owed the startup frames even when the turn never began.
    assert_eq!(
        names(&frames),
        vec![
            "response.created",
            "response.in_progress",
            "response.failed",
        ]
    );
}

#[test]
fn a_repeated_message_start_emits_the_startup_frames_once() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n";
    let frames = drive(&mut machine, &format!("{start}{start}"));
    assert_eq!(
        names(&frames),
        vec!["response.created", "response.in_progress"]
    );
}

#[test]
fn an_error_event_fails_the_response_and_ends_the_turn() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let frames = drive(
        &mut machine,
        concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        ),
    );
    // The open text block is closed before the failure, so the partial answer
    // is still a well-formed item.
    assert_eq!(
        frame(&frames, "response.output_text.done")["text"],
        "partial"
    );
    let response = frame(&frames, "response.failed")["response"].clone();
    assert_eq!(response["status"], "failed");
    assert_eq!(
        response["error"],
        json!({"code": "overloaded_error", "message": "Overloaded"})
    );
    assert!(machine.is_terminal());
    // Everything after the failure is dropped, including a late message_stop.
    assert!(drive(
        &mut machine,
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    )
    .is_empty());
    assert!(machine.finish().is_empty());
}

#[test]
fn finish_closes_open_items_and_fails_a_truncated_stream() {
    let mut machine = MessagesSseMachine::new("claude-test");
    let mut frames = drive(
        &mut machine,
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cut\"}}\n\n",
        ),
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
fn translate_response_maps_a_text_and_tool_use_message() {
    let response = translate_response(
        &json!({
            "id": "msg_1",
            "role": "assistant",
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Reading it."},
                {"type": "tool_use", "id": "toolu_3", "name": "read_file", "input": {"path": "a.rs"}},
            ],
            "usage": {"input_tokens": 20, "cache_read_input_tokens": 5, "output_tokens": 11},
        }),
        "claude-test",
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
    assert_eq!(output[1]["call_id"], "toolu_3");
    assert_eq!(output[1]["arguments"], "{\"path\":\"a.rs\"}");
    assert_eq!(response["usage"]["input_tokens"], 25);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        5
    );
    assert_eq!(response["usage"]["output_tokens"], 11);
}

#[test]
fn translate_response_reports_a_max_tokens_message_as_incomplete() {
    let response = translate_response(
        &json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "cut"}]}),
        "claude-test",
    );
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
}

#[test]
fn translate_response_reports_a_context_window_message_as_incomplete() {
    let response = translate_response(
        &json!({
            "stop_reason": "model_context_window_exceeded",
            "content": [{"type": "text", "text": "cut"}],
        }),
        "claude-test",
    );
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "model_context_window_exceeded"})
    );
}

#[test]
fn translate_error_maps_the_anthropic_envelope_and_hides_unknown_bodies() {
    assert_eq!(
        translate_error(&json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "max_tokens is required"},
        })),
        json!({"error": {
            "type": "invalid_request_error",
            "message": "max_tokens is required",
            "code": null,
            "param": null,
        }})
    );
    let unknown = translate_error(&json!({"detail": "upstream said something else"}));
    assert_eq!(unknown["error"]["type"], "api_error");
    assert!(!unknown.to_string().contains("upstream said something else"));
}
