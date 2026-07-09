//! Local token counting for `count_tokens` on `responses`-routed models.
//!
//! The OpenAI/Codex Responses API has no server-side token-count endpoint (only
//! Anthropic does), so an opt-in provider (`count_tokens = "tiktoken"`) has shunt
//! count the request locally with tiktoken's o200k_base encoder. This is far
//! closer than Claude Code's char/4 fallback, but is still an approximation: the
//! backend's billed count also includes reasoning tokens, image/tool-schema
//! encoding, and cache accounting that a text-only local count can't see.

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
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return 0;
    };

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
