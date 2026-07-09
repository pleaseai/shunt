use serde_json::{json, Map, Value};

use crate::routing::Route;

pub fn translate_request(body: &[u8], route: &Route) -> Result<Value, serde_json::Error> {
    let request: Value = serde_json::from_slice(body)?;
    let mut out = Map::new();
    out.insert("model".to_string(), json!(route.upstream_model));
    if let Some(instructions) = instructions(&request) {
        out.insert("instructions".to_string(), json!(instructions));
    }
    out.insert("input".to_string(), json!(input_items(&request)));
    if let Some(tools) = tools(&request) {
        out.insert("tools".to_string(), tools);
    }
    if let Some(tool_choice) = tool_choice(&request) {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(value) = request.get("parallel_tool_calls") {
        out.insert("parallel_tool_calls".to_string(), value.clone());
    }
    out.insert(
        "reasoning".to_string(),
        json!({"effort": effort(&request, route), "summary": "auto"}),
    );
    out.insert("text".to_string(), json!({"verbosity": "medium"}));
    out.insert("store".to_string(), json!(false));
    out.insert("stream".to_string(), json!(true));
    Ok(Value::Object(out))
}

fn instructions(request: &Value) -> Option<String> {
    match request.get("system")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn input_items(request: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return out;
    };
    for message in messages {
        let role = normalize_role(
            message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user"),
        );
        let blocks = content_blocks(message.get("content"));
        let mut pending = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => text_part(role, &block, &mut pending),
                Some("image") => image_part(&block, &mut pending),
                Some("tool_use") => tool_use_item(&mut out, role, &mut pending, &block),
                Some("tool_result") => tool_result_item(&mut out, role, &mut pending, &block),
                _ => {}
            }
        }
        flush_message(&mut out, role, &mut pending);
    }
    out
}

/// Claude Code sends mid-conversation `system`-role messages (e.g. SessionStart
/// hook output, the agent catalog) in the `messages` array. The ChatGPT Codex
/// backend rejects them (`{"detail":"System messages are not allowed"}`), while
/// the Responses convention for system-level turns is `developer`, which the
/// backend accepts. Map `system` -> `developer` so the content is preserved
/// rather than dropped; verified live against the ChatGPT Codex backend.
fn normalize_role(role: &str) -> &str {
    if role == "system" {
        "developer"
    } else {
        role
    }
}

fn text_part(role: &str, block: &Value, pending: &mut Vec<Value>) {
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            let kind = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            pending.push(json!({"type": kind, "text": text}));
        }
    }
}

fn image_part(block: &Value, pending: &mut Vec<Value>) {
    if let Some(source) = block.get("source") {
        let media_type = source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        let data = source.get("data").and_then(Value::as_str).unwrap_or("");
        pending.push(json!({
            "type": "input_image",
            "image_url": format!("data:{media_type};base64,{data}")
        }));
    }
}

fn tool_use_item(out: &mut Vec<Value>, role: &str, pending: &mut Vec<Value>, block: &Value) {
    flush_message(out, role, pending);
    out.push(json!({
        "type": "function_call",
        "call_id": block.get("id").and_then(Value::as_str).unwrap_or(""),
        "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
        "arguments": block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".to_string())
    }));
}

fn tool_result_item(out: &mut Vec<Value>, role: &str, pending: &mut Vec<Value>, block: &Value) {
    flush_message(out, role, pending);
    out.push(json!({
        "type": "function_call_output",
        "call_id": block.get("tool_use_id").and_then(Value::as_str).unwrap_or(""),
        "output": tool_result_output(block)
    }));
}

fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => Vec::new(),
    }
}

fn flush_message(out: &mut Vec<Value>, role: &str, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    out.push(json!({"type": "message", "role": role, "content": pending}));
    pending.clear();
}

fn tool_result_output(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() && block.get("is_error").and_then(Value::as_bool) == Some(true) {
                "Tool execution failed".to_string()
            } else {
                text
            }
        }
        _ if block.get("is_error").and_then(Value::as_bool) == Some(true) => {
            "Tool execution failed".to_string()
        }
        _ => String::new(),
    }
}

fn tools(request: &Value) -> Option<Value> {
    let tools = request.get("tools")?.as_array()?;
    Some(Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": normalize_schema(tool.get("input_schema").cloned().unwrap_or_else(|| json!({})))
                })
            })
            .collect(),
    ))
}

fn normalize_schema(schema: Value) -> Value {
    let mut object = match schema {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    object.insert("type".to_string(), json!("object"));
    object
        .entry("properties".to_string())
        .or_insert_with(|| json!({}));
    if !object.get("required").is_some_and(Value::is_array) {
        object.remove("required");
    }
    object
        .entry("additionalProperties".to_string())
        .or_insert_with(|| json!(true));
    Value::Object(object)
}

fn tool_choice(request: &Value) -> Option<Value> {
    let has_tools = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    match request.get("tool_choice") {
        Some(choice) => match choice.get("type").and_then(Value::as_str) {
            Some("auto") => Some(json!("auto")),
            Some("none") => Some(json!("none")),
            Some("any") => Some(json!("required")),
            Some("tool") => Some(json!({
                "type": "function",
                "name": choice.get("name").and_then(Value::as_str).unwrap_or("")
            })),
            _ => None,
        },
        None if has_tools => Some(json!("auto")),
        None => None,
    }
}

fn effort(request: &Value, route: &Route) -> String {
    if let Some(effort) = &route.effort {
        return effort.clone();
    }
    if request.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled") {
        return "high".to_string();
    }
    let model = &route.upstream_model;
    if model.ends_with("-xhigh") {
        "xhigh"
    } else if model.ends_with("-high") {
        "high"
    } else if model.ends_with("-medium") {
        "medium"
    } else if model.ends_with("-spark") || model.ends_with("-low") {
        "low"
    } else {
        "medium"
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::input_items;

    #[test]
    fn maps_system_role_message_to_developer() {
        // Claude Code sends mid-conversation system messages; the ChatGPT Codex
        // backend rejects role "system" but accepts "developer".
        let request = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": "SessionStart hook output"}
            ]
        });

        let items = input_items(&request);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["role"], "developer");
        assert_eq!(items[1]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["content"][0]["text"], "SessionStart hook output");
    }

    #[test]
    fn preserves_user_and_assistant_roles() {
        let request = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ]
        });

        let items = input_items(&request);

        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
    }
}
