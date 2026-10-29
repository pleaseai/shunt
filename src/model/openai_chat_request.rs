//! Anthropic Messages -> OpenAI Chat Completions request translation.
//!
//! Deliberate whitelist for this slice: text turns only (system/user/assistant
//! string or text-block content), the resolved model, and the stream flag.
//! Tools, images, reasoning, and Responses-only fields are refused instead of
//! silently dropped, so an unsupported turn fails closed with a typed
//! Anthropic-shaped 400 rather than degrading upstream. Plans 13-02 onward
//! expand the whitelist along this skeleton.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{json, Map, Value};

use crate::adapters::AdapterError;

pub(crate) fn bad_request(message: impl Into<String>) -> AdapterError {
    let message = message.into();
    AdapterError {
        message: message.clone(),
        response: Box::new(
            (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "type": "error",
                    "error": {"type": "invalid_request_error", "message": message}
                })),
            )
                .into_response(),
        ),
        failure: None,
    }
}

/// Translate the inbound Anthropic request into an OpenAI Chat Completions body.
///
/// `stream` is decided by the adapter from the inbound flag (never trusted
/// from the translated tree alone) and forces the upstream
/// `stream_options.include_usage` contract so streaming turns keep their
/// usage accounting.
pub fn translate_request(
    request: &Value,
    model: &str,
    stream: bool,
) -> Result<Value, AdapterError> {
    let mut out = Map::new();
    out.insert("model".to_string(), json!(model));
    out.insert("stream".to_string(), json!(stream));
    if stream {
        out.insert("stream_options".to_string(), json!({"include_usage": true}));
    }
    if let Some(tools) = request.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| bad_request("tools must be an array"))?;
        if !tools.is_empty() {
            return Err(bad_request(
                "OpenAI Chat slice does not support Anthropic tools yet",
            ));
        }
    }
    if let Some(tool_choice) = request.get("tool_choice") {
        if !tool_choice_allowed(tool_choice) {
            return Err(bad_request(
                "OpenAI Chat slice does not support Anthropic tool_choice yet",
            ));
        }
    }
    if let Some(thinking) = request.get("thinking") {
        let disabled = thinking.get("type").and_then(Value::as_str) == Some("disabled");
        if !thinking.is_null() && !disabled {
            return Err(bad_request(
                "OpenAI Chat slice does not support Anthropic thinking yet",
            ));
        }
    }
    if let Some(max_tokens) = request.get("max_tokens") {
        let max_tokens = max_tokens
            .as_u64()
            .ok_or_else(|| bad_request("max_tokens must be an unsigned integer"))?;
        // o-series models reject the legacy `max_tokens` field.
        out.insert("max_completion_tokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = request.get("temperature") {
        let temperature = temperature
            .as_f64()
            .ok_or_else(|| bad_request("temperature must be a number"))?;
        out.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.get("top_p") {
        let top_p = top_p
            .as_f64()
            .ok_or_else(|| bad_request("top_p must be a number"))?;
        out.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(stop_sequences) = request.get("stop_sequences") {
        let stop_sequences = stop_sequences
            .as_array()
            .ok_or_else(|| bad_request("stop_sequences must be an array"))?;
        if !stop_sequences.is_empty() {
            let mut stop = Vec::with_capacity(stop_sequences.len());
            for sequence in stop_sequences {
                let sequence = sequence
                    .as_str()
                    .ok_or_else(|| bad_request("stop_sequences must be strings"))?;
                stop.push(sequence);
            }
            out.insert("stop".to_string(), json!(stop));
        }
    }
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| bad_request("messages must be an array"))?;
    // A system prompt does not satisfy Anthropic's non-empty `messages` rule.
    if messages.is_empty() {
        return Err(bad_request("messages must not be empty"));
    }
    let mut translated =
        Vec::with_capacity(messages.len() + usize::from(request.get("system").is_some()));
    if let Some(system) = request.get("system") {
        translated.push(translate_system(system)?);
    }
    for message in messages {
        translated.push(translate_message(message)?);
    }
    out.insert("messages".to_string(), Value::Array(translated));
    Ok(Value::Object(out))
}

/// Anthropic sends `tool_choice` as an object. `auto` and `none` do not
/// require a tool call, so the text-only slice can accept them. A bare
/// `"auto"` string is the shape this translator used to compare against.
fn tool_choice_allowed(choice: &Value) -> bool {
    if choice.is_null() {
        return true;
    }
    if let Some(text) = choice.as_str() {
        return matches!(text, "auto" | "none");
    }
    matches!(
        choice.get("type").and_then(Value::as_str),
        Some("auto" | "none")
    )
}

fn translate_system(system: &Value) -> Result<Value, AdapterError> {
    match system {
        Value::String(text) => Ok(text_with_role("system", text)?),
        Value::Array(blocks) => {
            let text = collect_text_blocks(blocks)?;
            text_with_role("system", &text)
        }
        _ => Err(bad_request(
            "system must be a string or an array of text blocks",
        )),
    }
}

fn translate_message(message: &Value) -> Result<Value, AdapterError> {
    let message = message
        .as_object()
        .ok_or_else(|| bad_request("each message must be an object"))?;
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_request("each message must have a role"))?;
    if !matches!(role, "system" | "user" | "assistant") {
        return Err(bad_request(format!(
            "unsupported OpenAI Chat message role {role}"
        )));
    }
    let content = message
        .get("content")
        .ok_or_else(|| bad_request("each message must have content"))?;
    match content {
        Value::String(text) => text_with_role(role, text),
        Value::Array(blocks) => {
            let text = collect_text_blocks(blocks)?;
            text_with_role(role, &text)
        }
        _ => Err(bad_request(
            "message content must be a string or text blocks",
        )),
    }
}

fn collect_text_blocks(blocks: &[Value]) -> Result<String, AdapterError> {
    let mut text = String::new();
    for block in blocks {
        let block = block
            .as_object()
            .ok_or_else(|| bad_request("content blocks must be objects"))?;
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return Err(bad_request(
                "OpenAI Chat slice supports text content blocks only",
            ));
        }
        let part = block
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_request("text content block must have string text"))?;
        text.push_str(part);
    }
    Ok(text)
}

fn text_with_role(role: &str, text: &str) -> Result<Value, AdapterError> {
    let mut message = Map::new();
    message.insert("role".to_string(), json!(role));
    message.insert("content".to_string(), json!(text));
    Ok(Value::Object(message))
}
