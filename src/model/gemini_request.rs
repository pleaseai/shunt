//! Anthropic Messages -> Gemini generateContent request translation.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::adapters::AdapterError;

/// Translate an Anthropic Messages request into a Gemini `generateContent` request body.
///
/// Decoupled from the Code Assist envelope (`{model, project, request}`); callers
/// targeting Code Assist can wrap the resulting JSON payload with [`wrap_code_assist_envelope`].
pub fn translate_request(request: &Value) -> Result<Value, AdapterError> {
    let mut out = Map::new();

    // 1. System instruction
    if let Some(system_instruction) = translate_system_instruction(request) {
        out.insert("systemInstruction".to_string(), system_instruction);
    }

    // 2. Contents (multi-turn history)
    let contents = translate_messages(request)?;
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
    if let Some(tools) = translate_tools(request) {
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

fn translate_messages(request: &Value) -> Result<Vec<Value>, AdapterError> {
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
                                if let Some(source) = block.get("source") {
                                    if source.get("type").and_then(Value::as_str) == Some("base64")
                                    {
                                        let media_type = source
                                            .get("media_type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("image/png");
                                        let data = source
                                            .get("data")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        if !data.is_empty() {
                                            parts.push(json!({
                                                "inlineData": {
                                                    "mimeType": media_type,
                                                    "data": data
                                                }
                                            }));
                                        }
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                                let input =
                                    block.get("input").cloned().unwrap_or_else(|| json!({}));
                                if !name.is_empty() {
                                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                                        tool_names.insert(id.to_string(), name.to_string());
                                    }
                                    parts.push(json!({
                                        "functionCall": {
                                            "name": name,
                                            "args": input
                                        }
                                    }));
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
                                let output_val = extract_tool_result_content(block);
                                parts.push(json!({
                                    "functionResponse": {
                                        "name": name,
                                        "response": {
                                            "output": output_val
                                        }
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
            contents.push(json!({
                "role": role,
                "parts": parts
            }));
        }
    }

    Ok(contents)
}

fn extract_tool_result_content(block: &Value) -> Value {
    if let Some(content) = block.get("content") {
        match content {
            Value::String(text) => json!(text),
            Value::Array(blocks) => {
                let mut text_parts = Vec::new();
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            text_parts.push(t);
                        }
                    }
                }
                if !text_parts.is_empty() {
                    json!(text_parts.join("\n"))
                } else {
                    content.clone()
                }
            }
            _ => content.clone(),
        }
    } else {
        json!("")
    }
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

fn sanitize_gemini_schema(val: &mut Value) {
    match val {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("propertyNames");
            map.remove("$id");
            map.remove("$comment");
            map.remove("patternProperties");
            map.remove("exclusiveMinimum");
            map.remove("exclusiveMaximum");
            map.remove("const");
            for (_k, v) in map.iter_mut() {
                sanitize_gemini_schema(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                sanitize_gemini_schema(v);
            }
        }
        _ => {}
    }
}

fn translate_tools(request: &Value) -> Option<Value> {
    let tools = request.get("tools")?.as_array()?;
    if tools.is_empty() {
        return None;
    }

    let mut function_declarations = Vec::new();

    for tool in tools {
        let name = tool.get("name")?.as_str()?;
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut parameters = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        sanitize_gemini_schema(&mut parameters);

        function_declarations.push(json!({
            "name": name,
            "description": description,
            "parameters": parameters
        }));
    }

    if function_declarations.is_empty() {
        None
    } else {
        Some(json!([{
            "functionDeclarations": function_declarations
        }]))
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
mod tests {
    use super::*;

    #[test]
    fn translate_plain_text_user_message() {
        let input = json!({
            "messages": [
                { "role": "user", "content": "Hello Gemini!" }
            ]
        });

        let result = translate_request(&input).unwrap();
        assert!(result.get("thinkingConfig").is_none());
        assert_eq!(result["contents"][0]["role"], "user");
        assert_eq!(result["contents"][0]["parts"][0]["text"], "Hello Gemini!");
    }

    #[test]
    fn translate_system_prompt_and_generation_config() {
        let input = json!({
            "system": "You are a Rust expert.",
            "temperature": 0.5,
            "max_tokens": 2048,
            "messages": [
                { "role": "user", "content": "Explain async/await." }
            ]
        });

        let result = translate_request(&input).unwrap();
        assert_eq!(
            result["systemInstruction"]["parts"][0]["text"],
            "You are a Rust expert."
        );
        assert_eq!(result["generationConfig"]["temperature"], 0.5);
        assert_eq!(result["generationConfig"]["maxOutputTokens"], 2048);
    }

    #[test]
    fn translate_tools_and_tool_choice() {
        let input = json!({
            "messages": [
                { "role": "user", "content": "Check weather in Tokyo" }
            ],
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Get current weather",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "location": { "type": "string" }
                        }
                    }
                }
            ],
            "tool_choice": {
                "type": "tool",
                "name": "get_weather"
            }
        });

        let result = translate_request(&input).unwrap();
        let decls = &result["tools"][0]["functionDeclarations"];
        assert_eq!(decls[0]["name"], "get_weather");
        assert_eq!(result["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(
            result["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "get_weather"
        );
    }

    #[test]
    fn translate_extended_thinking() {
        let input = json!({
            "messages": [
                { "role": "user", "content": "Solve math puzzle" }
            ],
            "thinking": {
                "type": "enabled",
                "budget_tokens": 4096
            }
        });

        let result = translate_request(&input).unwrap();
        assert_eq!(
            result["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
        assert!(result.get("thinkingConfig").is_none());
    }

    #[test]
    fn sanitize_tool_input_schema_removes_unsupported_keys() {
        let input = json!({
            "messages": [
                { "role": "user", "content": "Run tool" }
            ],
            "tools": [
                {
                    "name": "sample_tool",
                    "description": "Tool description",
                    "input_schema": {
                        "$schema": "http://json-schema.org/draft-07/schema#",
                        "type": "object",
                        "properties": {
                            "arg1": {
                                "type": "string",
                                "propertyNames": { "pattern": "^[a-z]+$" }
                            }
                        }
                    }
                }
            ]
        });

        let result = translate_request(&input).unwrap();
        let params = &result["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params["properties"]["arg1"].get("propertyNames").is_none());
    }

    #[test]
    fn wrap_envelope_creates_code_assist_shape() {
        let inner = json!({ "contents": [] });
        let wrapped = wrap_code_assist_envelope("gemini-3-flash-preview", "test-proj-789", inner);

        assert_eq!(wrapped["model"], "gemini-3-flash-preview");
        assert_eq!(wrapped["project"], "test-proj-789");
        assert!(wrapped.get("request").is_some());
    }
}
