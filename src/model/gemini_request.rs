//! Anthropic Messages -> Gemini generateContent request translation.

use axum::response::IntoResponse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::adapters::AdapterError;

const MAX_SCHEMA_DEPTH: usize = 64;
const GEMINI_3_MODEL_PREFIX: &str = "gemini-3";
const GEMINI_TOOL_USE_ID_PREFIX: &str = "call_gemini_v1_";
// Google documents this exact value for imported/custom Gemini 3 function-call history.
const GEMINI_THOUGHT_SIGNATURE_PLACEHOLDER: &str = "context_engineering_is_the_way to_go";

fn bad_request(message: impl Into<String>) -> AdapterError {
    AdapterError {
        message: message.into(),
        response: Box::new(axum::http::StatusCode::BAD_REQUEST.into_response()),
        failure: None,
    }
}

/// Translate an Anthropic Messages request into a Gemini `generateContent` request body.
///
/// Uses the request's model id for model-specific history validation. Adapters with
/// an alias should call [`translate_request_for_model`] with the resolved upstream id.
pub fn translate_request(request: &Value) -> Result<Value, AdapterError> {
    let model = request.get("model").and_then(Value::as_str).unwrap_or("");
    translate_request_for_model(request, model)
}

/// Translate a request using the resolved Gemini model id.
pub fn translate_request_for_model(request: &Value, model: &str) -> Result<Value, AdapterError> {
    let mut out = Map::new();

    // 1. System instruction
    if let Some(system_instruction) = translate_system_instruction(request) {
        out.insert("systemInstruction".to_string(), system_instruction);
    }

    // 2. Contents (multi-turn history)
    let contents = translate_messages(request, model)?;
    out.insert("contents".to_string(), Value::Array(contents));

    // 3. Generation Config
    let mut gen_config = translate_generation_config(request);
    if let Some(thinking_config) = translate_thinking_config(request) {
        gen_config.insert("thinkingConfig".to_string(), thinking_config);
    }
    if !gen_config.is_empty() {
        out.insert("generationConfig".to_string(), Value::Object(gen_config));
    }

    // 4. Tools & Tool Choice
    if let Some(tools) = translate_tools(request)? {
        out.insert("tools".to_string(), tools);
    }
    if let Some(tool_config) = translate_tool_config(request) {
        out.insert("toolConfig".to_string(), tool_config);
    }

    Ok(Value::Object(out))
}

/// Wrap a Gemini `generateContent` request in the Google Code Assist envelope.
pub fn wrap_code_assist_envelope(model: &str, project: &str, request: Value) -> Value {
    json!({
        "model": model,
        "project": project,
        "request": request
    })
}

fn translate_system_instruction(request: &Value) -> Option<Value> {
    let system = request.get("system")?;
    let mut parts = Vec::new();

    match system {
        Value::String(text) if !text.is_empty() => {
            parts.push(json!({ "text": text }));
        }
        Value::Array(blocks) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(json!({ "text": text }));
                        }
                    }
                }
            }
        }
        _ => {}
    }

    if parts.is_empty() {
        None
    } else {
        Some(json!({ "parts": parts }))
    }
}

fn translate_messages(request: &Value, model: &str) -> Result<Vec<Value>, AdapterError> {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut contents = Vec::new();
    let mut tool_names = HashMap::new();

    for message in messages {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "model",
            _ => "user",
        };

        let mut parts = Vec::new();
        let mut saw_function_call = false;

        if let Some(content) = message.get("content") {
            match content {
                Value::String(text) => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                Value::Array(blocks) => {
                    for block in blocks {
                        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        parts.push(json!({ "text": text }));
                                    }
                                }
                            }
                            "image" => {
                                let source = block
                                    .get("source")
                                    .ok_or_else(|| bad_request("image block is missing source"))?;
                                match source.get("type").and_then(Value::as_str) {
                                    Some("base64") => {
                                        let media_type = source
                                            .get("media_type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("image/png");
                                        let data = source
                                            .get("data")
                                            .and_then(Value::as_str)
                                            .filter(|data| !data.is_empty())
                                            .ok_or_else(|| {
                                                bad_request("base64 image source is missing data")
                                            })?;
                                        parts.push(json!({
                                            "inlineData": {
                                                "mimeType": media_type,
                                                "data": data
                                            }
                                        }));
                                    }
                                    Some("url") => {
                                        return Err(bad_request(
                                            "URL image sources are not supported by the Gemini adapter",
                                        ));
                                    }
                                    _ => {
                                        return Err(bad_request(
                                            "unsupported image source type for Gemini",
                                        ));
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                                let input =
                                    block.get("input").cloned().unwrap_or_else(|| json!({}));
                                if !name.is_empty() {
                                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                                    if !id.is_empty() {
                                        tool_names.insert(id.to_string(), name.to_string());
                                    }
                                    let signature = decode_tool_use_signature(id).or_else(|| {
                                        (!saw_function_call
                                            && model.starts_with(GEMINI_3_MODEL_PREFIX))
                                        .then(|| GEMINI_THOUGHT_SIGNATURE_PLACEHOLDER.to_string())
                                    });
                                    let mut part = json!({
                                        "functionCall": {
                                            "name": name,
                                            "args": input
                                        }
                                    });
                                    if let Some(signature) = signature {
                                        part["thoughtSignature"] = Value::String(signature);
                                    }
                                    parts.push(part);
                                    saw_function_call = true;
                                }
                            }
                            "tool_result" => {
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown_tool");
                                let name = tool_names
                                    .get(tool_use_id)
                                    .map(String::as_str)
                                    .unwrap_or("unknown_tool");
                                let output_val = extract_tool_result_content(block)?;
                                let mut response = Map::new();
                                response.insert("output".to_string(), output_val);
                                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                                    response.insert("error".to_string(), Value::Bool(true));
                                }
                                parts.push(json!({
                                    "functionResponse": {
                                        "name": name,
                                        "response": response
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        if !parts.is_empty() {
            push_content(&mut contents, role, parts);
        }
    }

    Ok(contents)
}

/// Append a translated turn, merging consecutive `user` turns into one.
///
/// Claude Code's `mid-conversation-system-2026-04-07` beta inserts `system`-role
/// messages into `messages`, including between an assistant `tool_use` and the
/// user's `tool_result`. Those fold to `user` here, which would otherwise emit a
/// standalone text turn between a `functionCall` and its `functionResponse` —
/// the Gemini API rejects that ordering. Merging keeps the reminder text and the
/// `functionResponse` in the same turn. `model` turns are never merged: shifting
/// their part indices would misalign thought signatures.
fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if role == "user" {
        if let Some(previous) = contents
            .last_mut()
            .filter(|previous| previous["role"] == "user")
        {
            if let Some(existing) = previous.get_mut("parts").and_then(Value::as_array_mut) {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

fn decode_tool_use_signature(id: &str) -> Option<String> {
    let encoded = id.strip_prefix(GEMINI_TOOL_USE_ID_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    String::from_utf8(bytes)
        .ok()
        .filter(|signature| !signature.is_empty())
}

fn extract_tool_result_content(block: &Value) -> Result<Value, AdapterError> {
    let output = if let Some(content) = block.get("content") {
        match content {
            Value::String(text) => json!(text),
            Value::Array(blocks) => {
                let mut text_parts = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                text_parts.push(text);
                            }
                        }
                        Some("image") | Some("document") => {
                            return Err(bad_request(
                                "rich media tool results are not supported by the Gemini adapter",
                            ));
                        }
                        _ => {}
                    }
                }
                json!(text_parts.join("\n"))
            }
            _ => content.clone(),
        }
    } else {
        json!("")
    };
    Ok(output)
}

fn translate_generation_config(request: &Value) -> Map<String, Value> {
    let mut config = Map::new();

    if let Some(temp) = request.get("temperature").and_then(Value::as_f64) {
        config.insert("temperature".to_string(), json!(temp));
    }
    if let Some(max_tokens) = request.get("max_tokens").and_then(Value::as_u64) {
        config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if let Some(top_p) = request.get("top_p").and_then(Value::as_f64) {
        config.insert("topP".to_string(), json!(top_p));
    }
    if let Some(top_k) = request.get("top_k").and_then(Value::as_u64) {
        config.insert("topK".to_string(), json!(top_k));
    }
    if let Some(stops) = request.get("stop_sequences").and_then(Value::as_array) {
        let stop_strings: Vec<&str> = stops.iter().filter_map(Value::as_str).collect();
        if !stop_strings.is_empty() {
            config.insert("stopSequences".to_string(), json!(stop_strings));
        }
    }

    config
}

fn sanitize_gemini_schema(value: &mut Value, depth: usize) -> Result<(), AdapterError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(bad_request(
            "tool input schema exceeds maximum nesting depth",
        ));
    }
    match value {
        Value::Object(map) => {
            for key in [
                "$schema",
                "propertyNames",
                "$id",
                "$comment",
                "patternProperties",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "const",
            ] {
                map.remove(key);
            }
            for child in map.values_mut() {
                sanitize_gemini_schema(child, depth + 1)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                sanitize_gemini_schema(child, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn translate_tools(request: &Value) -> Result<Option<Value>, AdapterError> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Ok(None);
    };
    if tools.is_empty() {
        return Ok(None);
    }

    let mut function_declarations = Vec::new();

    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut parameters = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        sanitize_gemini_schema(&mut parameters, 0)?;

        function_declarations.push(json!({
            "name": name,
            "description": description,
            "parameters": parameters
        }));
    }

    if function_declarations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(json!([{
            "functionDeclarations": function_declarations
        }])))
    }
}

fn translate_tool_config(request: &Value) -> Option<Value> {
    let tool_choice = request.get("tool_choice")?;
    let choice_type = tool_choice.get("type").and_then(Value::as_str)?;

    match choice_type {
        "auto" => Some(json!({
            "functionCallingConfig": {
                "mode": "AUTO"
            }
        })),
        "any" => Some(json!({
            "functionCallingConfig": {
                "mode": "ANY"
            }
        })),
        "tool" => {
            let name = tool_choice.get("name").and_then(Value::as_str)?;
            Some(json!({
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [name]
                }
            }))
        }
        "none" => Some(json!({
            "functionCallingConfig": {
                "mode": "NONE"
            }
        })),
        _ => None,
    }
}

fn translate_thinking_config(request: &Value) -> Option<Value> {
    let thinking = request.get("thinking")?;
    match thinking.get("type").and_then(Value::as_str) {
        Some("enabled") => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(1024);
            Some(json!({ "thinkingBudget": budget }))
        }
        Some("disabled") => Some(json!({ "thinkingBudget": 0 })),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
