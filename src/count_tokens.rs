//! Local token counting for `count_tokens` on `responses`-routed models.
//!
//! The OpenAI/Codex Responses API has no server-side token-count endpoint (only
//! Anthropic does), so an opt-in provider (`count_tokens = "tiktoken"`) has shunt
//! count the request locally with tiktoken's o200k_base encoder. This is far
//! closer than Claude Code's char/4 fallback, but is still an approximation: the
//! backend's billed count also includes reasoning tokens, image/tool-schema
//! encoding, and cache accounting that a text-only local count can't see.
//!
//! [`count_responses_input_tokens`] applies the same count to an OpenAI
//! Responses request body, for the spend floor of a Codex endpoint turn that
//! ended without reported usage.

use std::sync::OnceLock;

use serde_json::Value;
use tiktoken_rs::{o200k_base, CoreBPE};

/// Rough per-message framing overhead (role markers etc.), mirroring the small
/// constant OpenAI's own token-counting guidance adds per chat message.
const PER_MESSAGE_OVERHEAD: u64 = 4;
/// Rough per-tool framing overhead on top of the tool's own text.
const PER_TOOL_OVERHEAD: u64 = 8;

fn encoder() -> &'static CoreBPE {
    static ENCODER: OnceLock<CoreBPE> = OnceLock::new();
    ENCODER.get_or_init(|| o200k_base().expect("o200k_base vocab is bundled with tiktoken-rs"))
}

/// Approximate the prompt token count of an Anthropic `count_tokens` request
/// body. Returns `0` for an unparseable body (the caller still answers 200; a
/// zero count just tells Claude Code the prompt is empty).
pub fn count_input_tokens(body: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(body)
        .map(|request| count_input_tokens_value(&request))
        .unwrap_or(0)
}

/// Same as [`count_input_tokens`] but over an already-parsed request, so a
/// caller that has parsed the body once (e.g. the responses adapter's `forward`)
/// need not deserialize the same buffer again.
pub fn count_input_tokens_value(request: &Value) -> u64 {
    let mut text = String::new();
    let mut overhead: u64 = 0;

    push_system_text(request.get("system"), &mut text);

    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages {
            overhead += PER_MESSAGE_OVERHEAD;
            push_content_text(message.get("content"), &mut text);
        }
    }

    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            overhead += PER_TOOL_OVERHEAD;
            push_str_field(tool.get("name"), &mut text);
            push_str_field(tool.get("description"), &mut text);
            if let Some(schema) = tool.get("input_schema") {
                text.push_str(&schema.to_string());
                text.push('\n');
            }
        }
    }

    encoder().encode_ordinary(&text).len() as u64 + overhead
}

/// Approximate the prompt token count of an OpenAI Responses request body —
/// the inbound Codex endpoint's turn. Returns `0` for an unparseable body.
///
/// The spend floor bills this as a turn's input when the upstream never
/// reported its usage (a turn cut before `response.completed`): Responses
/// reports input only at completion, so there is no earlier count to fall
/// back on.
pub fn count_responses_input_tokens(body: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(body)
        .map(|request| count_responses_input_tokens_value(&request))
        .unwrap_or(0)
}

/// Same as [`count_responses_input_tokens`] but over an already-parsed request.
///
/// Counts `instructions`, the text of each `input` item (or a bare string
/// `input`), and each tool's serialized JSON. An item kind with no readable
/// text (`web_search_call`, `image_generation_call`, ...) adds only its
/// per-item overhead.
pub fn count_responses_input_tokens_value(request: &Value) -> u64 {
    let mut text = String::new();
    let mut overhead: u64 = 0;

    push_str_field(request.get("instructions"), &mut text);

    match request.get("input") {
        Some(Value::String(input)) => {
            overhead += PER_MESSAGE_OVERHEAD;
            text.push_str(input);
            text.push('\n');
        }
        Some(Value::Array(items)) => {
            for item in items {
                overhead += PER_MESSAGE_OVERHEAD;
                push_input_item_text(item, &mut text);
            }
        }
        _ => {}
    }

    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            overhead += PER_TOOL_OVERHEAD;
            text.push_str(&tool.to_string());
            text.push('\n');
        }
    }

    encoder().encode_ordinary(&text).len() as u64 + overhead
}

/// One Responses `input` item's text: a message's content, a function call's
/// `arguments`, a function call output's `output`, or a reasoning item's
/// `summary` and `content` text (its `encrypted_content` is opaque). An item
/// with no `type` is an easy-input message.
fn push_input_item_text(item: &Value, out: &mut String) {
    match item.get("type").and_then(Value::as_str) {
        Some("message") | None => push_parts_text(item.get("content"), out),
        Some("function_call") => push_str_field(item.get("arguments"), out),
        Some("custom_tool_call") => push_str_field(item.get("input"), out),
        Some("function_call_output" | "custom_tool_call_output") => {
            push_parts_text(item.get("output"), out);
        }
        Some("reasoning") => {
            push_parts_text(item.get("summary"), out);
            push_parts_text(item.get("content"), out);
        }
        Some(_) => {}
    }
}

/// A string, or an array of parts whose `text` is collected (`input_text`,
/// `output_text`, `summary_text`, ...).
fn push_parts_text(value: Option<&Value>, out: &mut String) {
    match value {
        Some(Value::String(text)) => {
            out.push_str(text);
            out.push('\n');
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                push_str_field(part.get("text"), out);
            }
        }
        _ => {}
    }
}

fn push_str_field(value: Option<&Value>, out: &mut String) {
    if let Some(text) = value.and_then(Value::as_str) {
        out.push_str(text);
        out.push('\n');
    }
}

/// `system` is a string or an array of `{type:"text", text}` blocks.
fn push_system_text(system: Option<&Value>, out: &mut String) {
    match system {
        Some(Value::String(text)) => {
            out.push_str(text);
            out.push('\n');
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                push_str_field(block.get("text"), out);
            }
        }
        _ => {}
    }
}

/// A message `content` is a string or an array of blocks. Collect the natural-
/// language text a token count should include: text, tool_use inputs (serialized
/// JSON), and tool_result content.
fn push_content_text(content: Option<&Value>, out: &mut String) {
    match content {
        Some(Value::String(text)) => {
            out.push_str(text);
            out.push('\n');
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => push_str_field(block.get("text"), out),
                    Some("tool_use") => {
                        push_str_field(block.get("name"), out);
                        if let Some(input) = block.get("input") {
                            out.push_str(&input.to_string());
                            out.push('\n');
                        }
                    }
                    Some("tool_result") => push_content_text(block.get("content"), out),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_system_messages_and_tools() {
        let body = json!({
            "model": "gpt-5.6-sol",
            "system": "You are a helpful assistant.",
            "messages": [
                {"role": "user", "content": "Write a haiku about the sea."},
                {"role": "assistant", "content": [{"type": "text", "text": "Sure."}]}
            ],
            "tools": [{
                "name": "get_weather",
                "description": "Look up the weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
            }]
        });
        let n = count_input_tokens(&serde_json::to_vec(&body).unwrap());
        // Two messages (+8 overhead) and one tool (+8) plus real encoded text.
        assert!(n > 20, "expected a non-trivial count, got {n}");
    }

    #[test]
    fn empty_and_unparseable_bodies_are_safe() {
        assert_eq!(count_input_tokens(b"not json"), 0);
        let empty = json!({"model": "gpt-5.6-sol", "messages": []});
        assert_eq!(count_input_tokens(&serde_json::to_vec(&empty).unwrap()), 0);
    }

    fn tokens(text: &str) -> u64 {
        encoder().encode_ordinary(text).len() as u64
    }

    #[test]
    fn a_responses_body_counts_instructions_every_item_kind_and_tools() {
        let tool = json!({"type": "function", "name": "shell",
                          "parameters": {"type": "object"}});
        let body = json!({
            "model": "gpt-5.6-sol",
            "instructions": "be brief",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "message", "role": "assistant", "content": "sure"},
                {"role": "user", "content": [{"type": "output_text", "text": "untyped"}]},
                {"type": "function_call", "name": "shell", "call_id": "c1",
                 "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
                {"type": "reasoning",
                 "summary": [{"type": "summary_text", "text": "thinking"}]},
                {"type": "reasoning", "summary": [],
                 "content": [{"type": "reasoning_text", "text": "raw chain"}],
                 "encrypted_content": "opaque"},
                {"type": "web_search_call", "id": "ws_1"}
            ],
            "tools": [tool]
        });
        let text = format!(
            "be brief\nlist files\nsure\nuntyped\n{{\"cmd\":\"ls\"}}\na.txt\nthinking\nraw chain\n{tool}\n"
        );
        assert_eq!(
            count_responses_input_tokens(&serde_json::to_vec(&body).unwrap()),
            tokens(&text) + 8 * PER_MESSAGE_OVERHEAD + PER_TOOL_OVERHEAD
        );
    }

    #[test]
    fn custom_tool_calls_and_their_outputs_are_counted() {
        let body = json!({"input": [
            {"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch",
             "input": "*** Begin Patch"},
            {"type": "custom_tool_call_output", "call_id": "c1", "output": "Done!"},
            {"type": "custom_tool_call_output", "call_id": "c2",
             "output": [{"type": "input_text", "text": "parts"}]}
        ]});
        assert_eq!(
            count_responses_input_tokens(&serde_json::to_vec(&body).unwrap()),
            tokens("*** Begin Patch\nDone!\nparts\n") + 3 * PER_MESSAGE_OVERHEAD
        );
    }

    #[test]
    fn a_string_input_and_unparseable_responses_bodies() {
        let body = json!({"input": "hello there"});
        assert_eq!(
            count_responses_input_tokens(&serde_json::to_vec(&body).unwrap()),
            tokens("hello there\n") + PER_MESSAGE_OVERHEAD
        );
        assert_eq!(count_responses_input_tokens(b"not json"), 0);
        assert_eq!(count_responses_input_tokens(b"{}"), 0);
    }

    #[test]
    fn more_text_yields_more_tokens() {
        let short = json!({"messages": [{"role": "user", "content": "hi"}]});
        let long = json!({"messages": [{"role": "user",
            "content": "hi ".repeat(500)}]});
        let short_n = count_input_tokens(&serde_json::to_vec(&short).unwrap());
        let long_n = count_input_tokens(&serde_json::to_vec(&long).unwrap());
        assert!(long_n > short_n);
    }
}
