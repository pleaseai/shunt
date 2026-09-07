//! Inbound OpenAI Responses request -> OpenAI Chat Completions request.
//!
//! A `[[server.codex_endpoint.routes]]` entry can point at an OpenAI-compatible
//! backend that never adopted the Responses API — vLLM, LiteLLM, Ollama,
//! DeepSeek, OpenRouter. This module maps the request the Codex CLI sends onto
//! the Chat Completions shape those backends do speak: `input` items become
//! `messages`, `function_call` items become assistant `tool_calls`, and the
//! Responses-only knobs are either renamed or dropped.
//!
//! Translation is JSON-to-JSON on [`serde_json::Value`], mirroring
//! [`crate::model::responses_request`]. Errors never quote the request —
//! inbound bodies are client-controlled and reach logs and Sentry.

use serde_json::{json, Map, Value};

/// Why a request could not be translated. Deliberately content-free: the
/// variants name the shape that was wrong, never the client's payload.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("request body is not a JSON object")]
    NotAnObject,
    #[error("request has no input to translate")]
    MissingInput,
}

/// Translate a Responses request into a Chat Completions request.
///
/// `upstream_model` replaces the inbound `model`, so the routed provider's own
/// model id reaches the wire rather than the id the Codex CLI asked for.
pub fn translate_request(request: &Value, upstream_model: &str) -> Result<Value, TranslateError> {
    if !request.is_object() {
        return Err(TranslateError::NotAnObject);
    }

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));

    let messages = messages(request);
    if messages.is_empty() {
        return Err(TranslateError::MissingInput);
    }
    out.insert("messages".to_string(), Value::Array(messages));

    // An empty `tools: []` is rejected by several OpenAI-compatible backends,
    // and a `tool_choice` without tools is meaningless -- so both are omitted
    // together when no function tool survives translation.
    let tools = tools(request);
    if !tools.is_empty() {
        out.insert("tools".to_string(), Value::Array(tools));
        if let Some(tool_choice) = tool_choice(request) {
            out.insert("tool_choice".to_string(), tool_choice);
        }
    }

    if let Some(response_format) = response_format(request) {
        out.insert("response_format".to_string(), response_format);
    }

    // Only a streaming request gets `stream_options`, which is what makes the
    // final chunk carry `usage` -- Chat Completions omits it otherwise.
    if request.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("stream".to_string(), json!(true));
        out.insert("stream_options".to_string(), json!({"include_usage": true}));
    }

    copy(
        request,
        &mut out,
        "max_output_tokens",
        "max_completion_tokens",
    );
    for key in ["temperature", "top_p", "parallel_tool_calls", "metadata"] {
        copy(request, &mut out, key, key);
    }
    // `safety_identifier` is the Responses successor to `user`; both name the
    // same end user, and Chat Completions only knows the older key.
    if let Some(user) =
        present(request.get("user")).or_else(|| present(request.get("safety_identifier")))
    {
        out.insert("user".to_string(), user.clone());
    }
    if let Some(effort) = present(request.pointer("/reasoning/effort")) {
        out.insert("reasoning_effort".to_string(), effort.clone());
    }
    if let Some(verbosity) = present(request.pointer("/text/verbosity")) {
        out.insert("verbosity".to_string(), verbosity.clone());
    }
    // Dropped, with no Chat Completions equivalent: `store`, `include`,
    // `prompt_cache_key`, `previous_response_id`, `truncation`, `service_tier`,
    // `reasoning.summary` and `background`.

    Ok(Value::Object(out))
}

/// `instructions` plus the translated `input`, in wire order.
fn messages(request: &Value) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    match request.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                push_item(item, &mut messages);
            }
        }
        _ => {}
    }
    messages
}

/// One `input` item -> zero or more messages. `reasoning`, `web_search_call`
/// and item types shunt does not know are dropped: Chat Completions has no
/// field to carry them, and a backend that echoes reasoning does so through its
/// own `reasoning_content` (a documented follow-up, not a request field).
fn push_item(item: &Value, messages: &mut Vec<Value>) {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => push_message(item, messages),
        // Codex sends the first user turn as a bare `{role, content}` item.
        None if item.get("role").is_some() => push_message(item, messages),
        Some("function_call") => push_function_call(item, messages),
        Some("function_call_output") => push_function_call_output(item, messages),
        _ => {}
    }
}

fn push_message(item: &Value, messages: &mut Vec<Value>) {
    let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
        // Chat Completions has no `developer` role on most OSS backends, and a
        // mid-conversation `system` item keeps its position rather than being
        // hoisted, so both land as `system` in place.
        "system" | "developer" => "system",
        other => other,
    };
    messages.push(json!({"role": role, "content": message_content(item.get("content"))}));
}

/// Message content: a string passes through, and a part array is translated
/// part by part. An all-text array collapses back to a plain string -- many OSS
/// backends reject a part array outright, especially on assistant messages.
fn message_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(text)) => json!(text),
        Some(Value::Array(items)) => {
            let parts: Vec<Value> = items.iter().filter_map(content_part).collect();
            if parts.iter().all(is_text_part) {
                json!(join_text(&parts))
            } else {
                Value::Array(parts)
            }
        }
        _ => json!(""),
    }
}

fn is_text_part(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("text")
}

fn join_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One Responses content part -> one Chat Completions content part. A part
/// shunt cannot represent (an `input_file` referenced by id, say) is dropped
/// rather than forwarded in a shape the backend would reject.
fn content_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str)? {
        "input_text" | "output_text" => {
            let text = part.get("text").and_then(Value::as_str)?;
            Some(json!({"type": "text", "text": text}))
        }
        "input_image" => {
            let url = part.get("image_url").and_then(Value::as_str)?;
            let mut image_url = Map::new();
            image_url.insert("url".to_string(), json!(url));
            if let Some(detail) = present(part.get("detail")) {
                image_url.insert("detail".to_string(), detail.clone());
            }
            Some(json!({"type": "image_url", "image_url": Value::Object(image_url)}))
        }
        "input_file" => {
            let file_data = part.get("file_data").and_then(Value::as_str)?;
            let mut file = Map::new();
            if let Some(filename) = present(part.get("filename")) {
                file.insert("filename".to_string(), filename.clone());
            }
            file.insert("file_data".to_string(), json!(file_data));
            Some(json!({"type": "file", "file": Value::Object(file)}))
        }
        _ => None,
    }
}

/// A `function_call` item -> an assistant `tool_calls` entry. Consecutive calls
/// share one assistant message, and a call right after an assistant text
/// message joins that message, because Chat Completions models a turn's text
/// and its tool calls as one assistant message rather than as separate items.
fn push_function_call(item: &Value, messages: &mut Vec<Value>) {
    let call = json!({
        "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
        "type": "function",
        "function": {
            "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
            "arguments": arguments(item.get("arguments")),
        }
    });
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(calls) = tool_calls_mut(last) {
                calls.push(call);
                return;
            }
        }
    }
    messages.push(json!({"role": "assistant", "tool_calls": [call]}));
}

fn tool_calls_mut(message: &mut Value) -> Option<&mut Vec<Value>> {
    message
        .as_object_mut()?
        .entry("tool_calls")
        .or_insert_with(|| json!([]))
        .as_array_mut()
}

/// Chat Completions carries tool-call arguments as a JSON-encoded string.
fn arguments(value: Option<&Value>) -> String {
    match present(value) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "{}".to_string(),
    }
}

fn push_function_call_output(item: &Value, messages: &mut Vec<Value>) {
    messages.push(json!({
        "role": "tool",
        "tool_call_id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
        "content": output_text(item.get("output")),
    }));
}

/// A tool result's `output` as the flat string a `tool` message needs.
fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => join_text(parts),
        _ => String::new(),
    }
}

/// Responses declares a function tool flat; Chat Completions nests it under
/// `function`. Built-in tool types (`web_search`, `local_shell`, ...) have no
/// Chat Completions equivalent and are dropped.
fn tools(request: &Value) -> Vec<Value> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    tools.iter().filter_map(function_tool).collect()
}

fn function_tool(tool: &Value) -> Option<Value> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let mut function = Map::new();
    function.insert(
        "name".to_string(),
        json!(tool.get("name").and_then(Value::as_str)?),
    );
    if let Some(description) = present(tool.get("description")) {
        function.insert("description".to_string(), description.clone());
    }
    // A function tool without `parameters` still needs a schema here: several
    // backends reject a function whose `parameters` key is missing.
    function.insert(
        "parameters".to_string(),
        present(tool.get("parameters"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    );
    if let Some(strict) = present(tool.get("strict")) {
        function.insert("strict".to_string(), strict.clone());
    }
    Some(json!({"type": "function", "function": Value::Object(function)}))
}

fn tool_choice(request: &Value) -> Option<Value> {
    match present(request.get("tool_choice"))? {
        Value::String(mode) if matches!(mode.as_str(), "auto" | "none" | "required") => {
            Some(json!(mode))
        }
        Value::Object(choice) => match choice.get("type").and_then(Value::as_str)? {
            "function" => {
                let name = choice.get("name").and_then(Value::as_str)?;
                Some(json!({"type": "function", "function": {"name": name}}))
            }
            // `allowed_tools` narrows the callable set, which Chat Completions
            // cannot express; its `mode` ("auto"/"required") is the part that
            // survives.
            "allowed_tools" => Some(json!(choice.get("mode").and_then(Value::as_str)?)),
            _ => None,
        },
        _ => None,
    }
}

/// Responses `text.format` -> Chat Completions `response_format`. A
/// `{"type": "text"}` format is the default on both sides, so it maps to
/// nothing at all.
fn response_format(request: &Value) -> Option<Value> {
    let format = request.pointer("/text/format")?;
    match format.get("type").and_then(Value::as_str)? {
        "json_schema" => {
            let mut schema = Map::new();
            schema.insert(
                "name".to_string(),
                json!(format.get("name").and_then(Value::as_str)?),
            );
            schema.insert("schema".to_string(), present(format.get("schema"))?.clone());
            if let Some(strict) = present(format.get("strict")) {
                schema.insert("strict".to_string(), strict.clone());
            }
            if let Some(description) = present(format.get("description")) {
                schema.insert("description".to_string(), description.clone());
            }
            Some(json!({"type": "json_schema", "json_schema": Value::Object(schema)}))
        }
        "json_object" => Some(json!({"type": "json_object"})),
        _ => None,
    }
}

/// A key the client actually set. An explicit `null` reads as absent — Codex
/// spells "unset" that way for several optional keys, and forwarding the null
/// makes strict backends 400.
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

fn copy(request: &Value, out: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = present(request.get(from)) {
        out.insert(to.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests;
