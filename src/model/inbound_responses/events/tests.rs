use super::*;

use crate::model::responses::parse_sse_events;

/// Parse emitted frames back into `(event name, data)` pairs, reusing the
/// production SSE parser rather than a second one written for tests.
fn parsed(frames: &[String]) -> Vec<(String, Value)> {
    parse_sse_events(&frames.concat())
        .into_iter()
        .map(|event| (event.event.unwrap_or_default(), event.data))
        .collect()
}

fn names(frames: &[String]) -> Vec<String> {
    parsed(frames).into_iter().map(|(name, _)| name).collect()
}

/// The `data` of the single frame whose event name matches.
fn frame(frames: &[String], name: &str) -> Value {
    parsed(frames)
        .into_iter()
        .find(|(event, _)| event == name)
        .unwrap_or_else(|| panic!("no {name} frame in {:?}", names(frames)))
        .1
}

/// A full text turn: created, one message, completed.
fn text_turn() -> (ResponsesEmitter, Vec<String>) {
    let mut emitter = ResponsesEmitter::new("claude-test");
    let mut frames = emitter.created();
    let (index, opened) = emitter.open_message();
    frames.extend(opened);
    frames.push(emitter.text_delta(index, "Hello, "));
    frames.push(emitter.text_delta(index, "world"));
    frames.extend(emitter.close_message(index));
    frames.push(emitter.completed(Usage {
        input_tokens: 30,
        cached_input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 0,
    }));
    (emitter, frames)
}

#[test]
fn every_event_carries_the_next_sequence_number_and_its_own_type() {
    let (_, frames) = text_turn();
    let events = parsed(&frames);
    assert!(!events.is_empty());
    for (position, (name, data)) in events.iter().enumerate() {
        assert_eq!(data["sequence_number"], json!(position as u64), "at {name}");
        assert_eq!(data["type"], json!(name), "at {name}");
    }
}

#[test]
fn a_message_opens_deltas_and_closes_with_the_accumulated_text() {
    let (_, frames) = text_turn();
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
    let done = frame(&frames, "response.output_item.done");
    let id = done["item"]["id"].as_str().expect("the item carries an id");
    assert!(id.starts_with("msg_"), "unexpected message id {id}");
    assert_eq!(
        done["item"],
        json!({
            "id": id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello, world", "annotations": []}],
        })
    );
    assert_eq!(done["output_index"], json!(0));
    assert_eq!(
        frame(&frames, "response.output_text.done")["text"],
        "Hello, world"
    );
}

#[test]
fn a_function_call_with_no_argument_deltas_closes_with_an_empty_object() {
    let mut emitter = ResponsesEmitter::new("claude-test");
    let (index, mut frames) = emitter.open_function_call("toolu_1", "read_file");
    frames.extend(emitter.close_function_call(index));
    let done = frame(&frames, "response.output_item.done");
    assert_eq!(done["item"]["arguments"], "{}");
    assert_eq!(done["item"]["call_id"], "toolu_1");
    assert_eq!(done["item"]["name"], "read_file");
    assert_eq!(
        frame(&frames, "response.function_call_arguments.done")["arguments"],
        "{}"
    );
}

#[test]
fn reasoning_closes_with_its_summary_and_optional_encrypted_content() {
    let mut emitter = ResponsesEmitter::new("claude-test");
    let (index, mut frames) = emitter.open_reasoning();
    frames.push(emitter.reasoning_delta(index, "weighing options"));
    frames.extend(emitter.close_reasoning(index, Some("opaque-blob".to_string())));
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "weighing options"}])
    );
    assert_eq!(item["encrypted_content"], "opaque-blob");

    // A reasoning item that never took a delta has no summary to show, and
    // omits `encrypted_content` entirely rather than carrying a null.
    let mut emitter = ResponsesEmitter::new("claude-test");
    let (index, mut frames) = emitter.open_reasoning();
    frames.extend(emitter.close_reasoning(index, None));
    let item = frame(&frames, "response.output_item.done")["item"].clone();
    assert_eq!(item["summary"], json!([]));
    assert_eq!(item.get("encrypted_content"), None);
}

#[test]
fn completed_reports_every_finished_item_and_the_usage_totals() {
    let mut emitter = ResponsesEmitter::new("claude-test");
    let (reasoning, _) = emitter.open_reasoning();
    let _ = emitter.reasoning_delta(reasoning, "think");
    let _ = emitter.close_reasoning(reasoning, None);
    let (message, _) = emitter.open_message();
    let _ = emitter.text_delta(message, "done");
    let _ = emitter.close_message(message);
    let completed = emitter.completed(Usage {
        input_tokens: 120,
        cached_input_tokens: 100,
        output_tokens: 7,
        reasoning_tokens: 3,
    });
    let response = frame(&[completed], "response.completed")["response"].clone();
    assert_eq!(response["status"], "completed");
    assert_eq!(response["model"], "claude-test");
    let kinds: Vec<&str> = response["output"]
        .as_array()
        .expect("output is an array")
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, vec!["reasoning", "message"]);
    assert_eq!(
        response["usage"],
        json!({
            "input_tokens": 120,
            "input_tokens_details": {"cached_tokens": 100},
            "output_tokens": 7,
            "output_tokens_details": {"reasoning_tokens": 3},
            "total_tokens": 127,
        })
    );
}

#[test]
fn usage_clamps_cached_tokens_to_the_input_total() {
    // An upstream that counts more cached than prompt tokens would otherwise
    // hand the client a usage object that contradicts itself.
    let usage = Usage {
        input_tokens: 40,
        cached_input_tokens: 64,
        output_tokens: 1,
        reasoning_tokens: 0,
    }
    .to_value();
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], 40);
    assert_eq!(usage["input_tokens"], 40);
}

#[test]
fn a_terminal_event_after_failed_is_suppressed() {
    let mut emitter = ResponsesEmitter::new("claude-test");
    let failed = emitter.failed("rate_limit_error", "slow down");
    let response = frame(&[failed], "response.failed")["response"].clone();
    assert_eq!(response["status"], "failed");
    assert_eq!(
        response["error"],
        json!({"code": "rate_limit_error", "message": "slow down"})
    );
    assert!(emitter.is_terminal());
    assert_eq!(emitter.completed(Usage::default()), "");
    assert_eq!(
        emitter.incomplete("max_output_tokens", Usage::default()),
        ""
    );
}

#[test]
fn closing_an_index_that_is_not_open_emits_nothing() {
    let mut emitter = ResponsesEmitter::new("claude-test");
    let (index, _) = emitter.open_message();
    // Wrong kind for the open index, and an index that was never allocated.
    assert!(emitter.close_reasoning(index, None).is_empty());
    assert!(emitter.close_function_call(index).is_empty());
    assert!(emitter.close_message(index + 7).is_empty());
    assert_eq!(emitter.text_delta(index + 7, "x"), "");
    assert_eq!(emitter.open_indexes(), vec![index]);
    assert!(!emitter.close_message(index).is_empty());
    assert!(emitter.open_indexes().is_empty());
}
