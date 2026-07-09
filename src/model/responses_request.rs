use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
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
    // With store:false the Responses backend forgets each turn's reasoning, so ask
    // for the encrypted reasoning blob and echo it back next turn (see input_items).
    // Only when the client enabled extended thinking, which is what lets Claude Code
    // round-trip the thinking blocks that carry the blob (see model/responses.rs).
    if thinking_enabled(&request) {
        out.insert(
            "include".to_string(),
            json!(["reasoning.encrypted_content"]),
        );
    }
    if let Some(cache_key) = prompt_cache_key(&request) {
        out.insert("prompt_cache_key".to_string(), json!(cache_key));
    }
    // Anthropic `max_tokens` caps output; the Responses equivalent is
    // `max_output_tokens`. Forward it so the client's cap is respected instead of
    // falling back to the model default.
    if let Some(max_tokens) = request.get("max_tokens").and_then(Value::as_u64) {
        out.insert("max_output_tokens".to_string(), json!(max_tokens));
    }
    out.insert("store".to_string(), json!(false));
    out.insert("stream".to_string(), json!(true));
    Ok(Value::Object(out))
}

/// A stable per-conversation key so the Responses backend routes every turn of a
/// session to the same prompt cache (codex uses its thread_id here). Claude Code
/// packs `{device_id, account_uuid, session_id}` as a JSON string in
/// `metadata.user_id`; `session_id` is the per-conversation id. Falls back to a
/// hash of the raw user_id, or nothing when the client sends no metadata.
fn prompt_cache_key(request: &Value) -> Option<String> {
    let user_id = request
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .filter(|user_id| !user_id.is_empty())?;
    if let Ok(parsed) = serde_json::from_str::<Value>(user_id) {
        if let Some(session) = parsed
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|session| !session.is_empty())
        {
            return Some(format!("shunt-{session}"));
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(user_id, &mut hasher);
    Some(format!("shunt-{:016x}", std::hash::Hasher::finish(&hasher)))
}

/// Whether the client requested extended thinking. Gates reasoning round-tripping:
/// Claude Code only echoes assistant thinking blocks back when thinking is enabled.
fn thinking_enabled(request: &Value) -> bool {
    request.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled")
}

/// Pack a Responses reasoning item's id + encrypted_content into the opaque
/// `signature` of an Anthropic thinking block. Claude Code round-trips signatures
/// verbatim, so the next turn can [`decode_reasoning_signature`] it back into a
/// Responses `reasoning` input item — preserving chain-of-thought under store:false.
pub fn encode_reasoning_signature(id: &str, encrypted_content: &str) -> String {
    let payload = json!({"id": id, "enc": encrypted_content});
    URL_SAFE_NO_PAD.encode(payload.to_string())
}

/// Inverse of [`encode_reasoning_signature`]. Returns `None` for signatures shunt
/// did not produce (e.g. a genuine Anthropic thinking signature), which are dropped
/// rather than forwarded — the Responses backend rejects reasoning it never issued.
fn decode_reasoning_signature(signature: &str) -> Option<(String, String)> {
    let bytes = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let enc = value.get("enc").and_then(Value::as_str)?.to_string();
    Some((id, enc))
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
                Some("document") => document_part(&block, &mut pending),
                Some("tool_use") => tool_use_item(&mut out, role, &mut pending, &block),
                Some("tool_result") => tool_result_item(&mut out, role, &mut pending, &block),
                Some("thinking") => reasoning_item(&mut out, role, &mut pending, &block),
                Some("redacted_thinking") => {
                    redacted_reasoning_item(&mut out, role, &mut pending, &block)
                }
                _ => {}
            }
        }
        flush_message(&mut out, role, &mut pending);
    }
    out
}

/// Claude Code sends `output_config.effort` (low|medium|high|xhigh|max) when
/// the model advertises effort support or `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`
/// is set (needed for custom gateway ids like gpt-5.6-sol). Map it to the
/// Responses `reasoning.effort`.
///
/// Which levels the ChatGPT/Codex backend accepts is per-model, listed in
/// openai/codex `codex-rs/models-manager/models.json` (`supported_reasoning_levels`):
/// the gpt-5.6 family accepts up to `max` (sol/terra even `ultra`), while the
/// gpt-5.5/5.4/5.2 slugs cap at `xhigh`. So `max` passes through for a model that
/// supports it and folds to `xhigh` otherwise. (Claude Code never emits `ultra`.)
fn map_effort(effort: &str, model: &str) -> String {
    if effort == "max" && !supports_max_effort(model) {
        "xhigh".to_string()
    } else {
        effort.to_string()
    }
}

/// Whether `model` accepts the `max` reasoning level, per codex models.json.
/// The gpt-5.6 family does; earlier slugs cap at `xhigh`.
fn supports_max_effort(model: &str) -> bool {
    model.contains("gpt-5.6")
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
    if let Some(item) = image_content_item(block) {
        pending.push(item);
    }
}

fn document_part(block: &Value, pending: &mut Vec<Value>) {
    if let Some(item) = file_content_item(block) {
        pending.push(item);
    }
}

/// Anthropic `image` block -> Responses `input_image`. `image_url` accepts both
/// a passthrough URL and a base64 `data:` URI, so both source shapes map to it.
fn image_content_item(block: &Value) -> Option<Value> {
    let image_url = source_url(block.get("source")?, "image/png")?;
    Some(json!({"type": "input_image", "image_url": image_url}))
}

/// Anthropic `document` block (e.g. a PDF) -> Responses `input_file`. Unlike
/// images, `input_file` splits by key: `file_url` for a URL source, `file_data`
/// for base64. Source shapes shunt can't represent are dropped (return None)
/// rather than forwarded as an empty, invalid `data:` URI.
fn file_content_item(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let mut item = match source.get("type").and_then(Value::as_str) {
        Some("url") => {
            let url = source.get("url").and_then(Value::as_str)?;
            json!({"type": "input_file", "file_url": url})
        }
        Some("base64") | None => {
            let data = source.get("data").and_then(Value::as_str)?;
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("application/pdf");
            json!({"type": "input_file", "file_data": format!("data:{media_type};base64,{data}")})
        }
        _ => return None,
    };
    if let Some(filename) = block.get("title").and_then(Value::as_str) {
        item["filename"] = json!(filename);
    }
    Some(item)
}

/// Resolve an Anthropic block `source` to a URL string: a passthrough `url`
/// source, or a base64 `data:` URI. Returns None for a base64 source missing its
/// data, or any source shape shunt can't represent — the caller drops the block
/// rather than emitting a malformed empty `data:` URI.
fn source_url(source: &Value, default_media_type: &str) -> Option<String> {
    match source.get("type").and_then(Value::as_str) {
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("base64") | None => {
            let data = source.get("data").and_then(Value::as_str)?;
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or(default_media_type);
            Some(format!("data:{media_type};base64,{data}"))
        }
        _ => None,
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

/// An assistant `thinking` block carries a Responses reasoning item's state in its
/// signature (stamped by shunt on the way out). Decode it back into a `reasoning`
/// input item so the backend keeps its chain-of-thought under store:false. Blocks
/// whose signature shunt did not produce are dropped — never forwarded.
fn reasoning_item(out: &mut Vec<Value>, role: &str, pending: &mut Vec<Value>, block: &Value) {
    let Some(signature) = block.get("signature").and_then(Value::as_str) else {
        return;
    };
    let Some((id, encrypted_content)) = decode_reasoning_signature(signature) else {
        return;
    };
    flush_message(out, role, pending);
    let summary = block.get("thinking").and_then(Value::as_str).unwrap_or("");
    out.push(reasoning_input_item(&id, &encrypted_content, summary));
}

/// A `redacted_thinking` block is the opaque fallback vehicle for the same reasoning
/// state, carried in `data` instead of a signature.
fn redacted_reasoning_item(
    out: &mut Vec<Value>,
    role: &str,
    pending: &mut Vec<Value>,
    block: &Value,
) {
    let Some(data) = block.get("data").and_then(Value::as_str) else {
        return;
    };
    let Some((id, encrypted_content)) = decode_reasoning_signature(data) else {
        return;
    };
    flush_message(out, role, pending);
    out.push(reasoning_input_item(&id, &encrypted_content, ""));
}

fn reasoning_input_item(id: &str, encrypted_content: &str, summary: &str) -> Value {
    let summary = if summary.is_empty() {
        json!([])
    } else {
        json!([{"type": "summary_text", "text": summary}])
    };
    let mut item = json!({
        "type": "reasoning",
        "summary": summary,
        "encrypted_content": encrypted_content,
    });
    if !id.is_empty() {
        item["id"] = json!(id);
    }
    item
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

/// The `output` of a `function_call_output`. Text-only results collapse to a plain
/// string (what most tools return); results carrying an image or document are sent
/// as the Responses content-item array (text/image/file) so they are not dropped.
fn tool_result_output(block: &Value) -> Value {
    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
    match block.get("content") {
        Some(Value::String(text)) => json!(text),
        Some(Value::Array(blocks)) => {
            let has_rich = blocks.iter().any(|inner| {
                matches!(
                    inner.get("type").and_then(Value::as_str),
                    Some("image") | Some("document")
                )
            });
            if has_rich {
                let mut items = blocks
                    .iter()
                    .filter_map(|inner| match inner.get("type").and_then(Value::as_str) {
                        Some("text") => inner
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| json!({"type": "input_text", "text": text})),
                        Some("image") => image_content_item(inner),
                        Some("document") => file_content_item(inner),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                // Preserve the failure signal the text-only branch conveys: a
                // failed result with no text would otherwise reach the model as
                // media alone, with no indication it errored.
                let has_text = items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("input_text")
                        && item
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                });
                if is_error && !has_text {
                    items.insert(
                        0,
                        json!({"type": "input_text", "text": "Tool execution failed"}),
                    );
                }
                return Value::Array(items);
            }
            let text = blocks
                .iter()
                .filter(|inner| inner.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|inner| inner.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() && is_error {
                json!("Tool execution failed")
            } else {
                json!(text)
            }
        }
        _ if is_error => json!("Tool execution failed"),
        _ => json!(""),
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
    if let Some(effort) = request
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
    {
        return map_effort(effort, &route.upstream_model);
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

    use super::{effort, input_items};
    use crate::routing::{AdapterKind, Route};

    fn codex_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.5".to_string(),
            upstream_model: "gpt-5.5".to_string(),
            effort: None,
        }
    }

    fn codex_route_model(model: &str) -> Route {
        Route {
            upstream_model: model.to_string(),
            model: model.to_string(),
            ..codex_route()
        }
    }

    #[test]
    fn maps_output_config_effort_to_reasoning_effort() {
        // gpt-5.5 caps at xhigh, so `max` folds down (per codex models.json).
        for (level, expected) in [
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "xhigh"),
            ("max", "xhigh"),
        ] {
            let request = json!({"output_config": {"effort": level}});
            assert_eq!(effort(&request, &codex_route()), expected, "level={level}");
        }
    }

    #[test]
    fn passes_max_effort_through_for_gpt_5_6() {
        // gpt-5.6* accept `max` natively, so it must not fold to xhigh.
        let request = json!({"output_config": {"effort": "max"}});
        assert_eq!(effort(&request, &codex_route_model("gpt-5.6-sol")), "max");
        assert_eq!(effort(&request, &codex_route_model("gpt-5.6-luna")), "max");
    }

    #[test]
    fn route_effort_overrides_request_effort() {
        let mut route = codex_route();
        route.effort = Some("high".to_string());
        let request = json!({"output_config": {"effort": "low"}});
        assert_eq!(effort(&request, &route), "high");
    }

    #[test]
    fn falls_back_to_medium_without_effort_or_thinking() {
        let request = json!({"messages": []});
        assert_eq!(effort(&request, &codex_route()), "medium");
    }

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
