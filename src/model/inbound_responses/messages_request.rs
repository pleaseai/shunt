//! Responses request -> Anthropic Messages request.
//!
//! The mirror image of [`crate::model::responses_request`]: that module turns
//! what Claude Code sends into a Responses request, this one turns what the
//! Codex CLI sends into a Messages request, so a
//! `[[server.codex_endpoint.routes]]` entry can point an inbound Responses
//! client at a `kind = "anthropic"` upstream.
//!
//! Two Anthropic constraints shape the fold in [`fold_input`]. Messages must
//! alternate user/assistant, while Responses `input` is a flat item list where
//! several consecutive items belong to the same turn (a `reasoning`, its
//! `function_call`, another `function_call`), so adjacent same-role items merge
//! into one message with several content blocks. And a `thinking` block must
//! lead the assistant message it belongs to, so decoded reasoning is buffered
//! separately from the turn's other blocks and prepended on flush.
//!
//! Request content is never named in an error or a log line here: the body is
//! client-controlled and the logs are shipped off-box.

use serde_json::{json, Map, Value};

use super::reasoning::{decode_thinking, thinking_block};

/// `max_tokens` is required by Anthropic and optional in Responses, so an
/// inbound request that omits `max_output_tokens` gets this cap rather than a
/// 400 from the upstream.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// The `budget_tokens` each `reasoning.effort` level asks for, before the
/// `max_tokens` clamp in [`thinking`].
const EFFORT_BUDGETS: &[(&str, u64)] = &[
    ("low", 2048),
    ("medium", 8192),
    ("high", 16384),
    ("xhigh", 32768),
];

/// The floor Anthropic enforces on `thinking.budget_tokens`, and the headroom
/// `max_tokens` must keep above the budget for the answer itself.
const MIN_THINKING_BUDGET: u64 = 1024;

/// Responses tool types that name the hosted web-search tool.
const WEB_SEARCH_TYPES: &[&str] = &["web_search", "web_search_preview", "web_search_2025_08_26"];

/// The Anthropic hosted web-search tool the Responses ones map to.
const WEB_SEARCH_TYPE: &str = "web_search_20250305";

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The body parsed as JSON but is not a JSON object.
    #[error("request body is not a JSON object")]
    NotAnObject,
    /// Nothing in `input` survived translation into an Anthropic message.
    #[error("request has no translatable input")]
    MissingInput,
}

/// `upstream_model` is the model id the route resolved; the inbound `model` is
/// replaced by it.
pub fn translate_request(request: &Value, upstream_model: &str) -> Result<Value, TranslateError> {
    if !request.is_object() {
        return Err(TranslateError::NotAnObject);
    }

    // `instructions` leads the system prompt; system/developer input items
    // append to it in order (they never become Anthropic messages).
    let mut system = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        if !instructions.trim().is_empty() {
            system.push(instructions.to_string());
        }
    }
    let messages = fold_input(request, &mut system);
    if messages.is_empty() {
        return Err(TranslateError::MissingInput);
    }

    let max_tokens = request
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let tools = tools(request);
    // A `tool_choice` with no tools is meaningless, so it rides with them.
    let tool_choice = (!tools.is_empty()).then(|| tool_choice(request)).flatten();
    let mut thinking = thinking(request, max_tokens);
    if thinking.is_some() && forces_a_tool(tool_choice.as_ref()) {
        // Anthropic rejects extended thinking alongside a forced tool choice.
        // The explicit choice is the stronger signal, so thinking gives way.
        tracing::debug!("dropped thinking: the request forces a tool choice");
        thinking = None;
    }

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    if !system.is_empty() {
        out.insert("system".to_string(), json!(system.join("\n")));
    }
    out.insert("messages".to_string(), Value::Array(messages));
    out.insert("max_tokens".to_string(), json!(max_tokens));
    if let Some(stream) = request.get("stream").and_then(Value::as_bool) {
        out.insert("stream".to_string(), json!(stream));
    }
    // Anthropic rejects temperature/top_p when extended thinking is enabled.
    if thinking.is_none() {
        for key in ["temperature", "top_p"] {
            if let Some(value) = request.get(key).filter(|value| value.is_number()) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    if let Some(user_id) = request.pointer("/metadata/user_id").and_then(Value::as_str) {
        out.insert("metadata".to_string(), json!({"user_id": user_id}));
    }
    if !tools.is_empty() {
        out.insert("tools".to_string(), Value::Array(tools));
        if let Some(tool_choice) = tool_choice {
            out.insert("tool_choice".to_string(), tool_choice);
        }
    }
    if let Some(thinking) = thinking {
        out.insert("thinking".to_string(), thinking);
    }
    if let Some(output_format) = output_format(request) {
        out.insert("output_format".to_string(), output_format);
    }
    Ok(Value::Object(out))
}

/// The Anthropic messages under construction. Blocks accumulate into the
/// current turn and flush into a message when the role changes or the input
/// ends, which is what merges consecutive same-role Responses items.
#[derive(Default)]
struct Turns {
    messages: Vec<Value>,
    role: Option<&'static str>,
    /// Decoded `thinking` blocks for the open assistant turn, kept apart so
    /// they can lead the message's content.
    thinking: Vec<Value>,
    blocks: Vec<Value>,
}

impl Turns {
    fn open(&mut self, role: &'static str) {
        if self.role != Some(role) {
            self.flush();
            self.role = Some(role);
        }
    }

    fn push(&mut self, role: &'static str, block: Value) {
        self.open(role);
        self.blocks.push(block);
    }

    fn push_thinking(&mut self, block: Value) {
        self.open("assistant");
        self.thinking.push(block);
    }

    fn flush(&mut self) {
        let role = self.role.take();
        let mut content = std::mem::take(&mut self.thinking);
        let blocks = std::mem::take(&mut self.blocks);
        // An empty turn is dropped, which also covers the reasoning item that
        // no assistant content follows: Anthropic rejects an assistant message
        // holding nothing but a thinking block.
        if blocks.is_empty() {
            return;
        }
        let Some(role) = role else {
            return;
        };
        content.extend(blocks);
        self.messages
            .push(json!({"role": role, "content": Value::Array(content)}));
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush();
        self.messages
    }
}

/// Fold `input` into Anthropic messages, appending any system/developer item's
/// text to `system` instead.
fn fold_input(request: &Value, system: &mut Vec<String>) -> Vec<Value> {
    let mut turns = Turns::default();
    match request.get("input") {
        Some(Value::String(text)) => {
            if let Some(block) = text_block(text) {
                turns.push("user", block);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                fold_item(item, system, &mut turns);
            }
        }
        _ => {}
    }
    turns.finish()
}

fn fold_item(item: &Value, system: &mut Vec<String>, turns: &mut Turns) {
    // An item with no `type` but a `role` is a message; anything still
    // unrecognized (`web_search_call`, a future item kind) is dropped rather
    // than failing a turn the client can otherwise complete.
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(if item.get("role").is_some() {
            "message"
        } else {
            ""
        });
    match kind {
        "message" => fold_message(item, system, turns),
        "function_call" => turns.push("assistant", tool_use_block(item)),
        "function_call_output" => turns.push("user", tool_result_block(item)),
        "reasoning" => {
            // Reasoning shunt did not encode (a genuine OpenAI item) decodes to
            // None and is dropped: Anthropic rejects thinking it never signed.
            if let Some(block) = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .and_then(decode_thinking)
            {
                turns.push_thinking(thinking_block(block));
            }
        }
        _ => {}
    }
}

fn fold_message(item: &Value, system: &mut Vec<String>, turns: &mut Turns) {
    let content = item.get("content");
    match item.get("role").and_then(Value::as_str).unwrap_or("user") {
        "system" | "developer" => {
            let text = joined_text(content);
            if !text.trim().is_empty() {
                system.push(text);
            }
        }
        "assistant" => {
            for block in message_blocks(content, "output_text", |_| None) {
                turns.push("assistant", block);
            }
        }
        _ => {
            for block in message_blocks(content, "input_text", user_part) {
                turns.push("user", block);
            }
        }
    }
}

/// The content blocks of a `message` item. `content` is either a bare string or
/// an array of parts; `text_type` names the part type carrying this role's
/// text, and `rich` translates the non-text parts the role admits.
fn message_blocks(
    content: Option<&Value>,
    text_type: &str,
    rich: fn(&Value) -> Option<Value>,
) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => text_block(text).into_iter().collect(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some(kind) if kind == text_type => part
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(text_block),
                _ => rich(part),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The image and file parts only a user message carries.
fn user_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") => image_block(part),
        Some("input_file") => document_block(part),
        _ => None,
    }
}

/// Flatten a message content value to plain text, for the system fold.
fn joined_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// An Anthropic `text` block, or None for text that carries nothing — an empty
/// assistant message is rejected outright.
fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"type": "text", "text": text}))
}

/// Split a `data:<media_type>;base64,<data>` URL. Only base64 data URLs are
/// representable as an Anthropic base64 source.
fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let (meta, data) = url.strip_prefix("data:")?.split_once(',')?;
    Some((meta.strip_suffix(";base64")?, data))
}

/// Responses `input_image` -> Anthropic `image`. A data URL becomes a base64
/// source, an http(s) one a url source; any other reference is dropped rather
/// than forwarded as a source the upstream cannot fetch.
fn image_block(part: &Value) -> Option<Value> {
    let url = part.get("image_url").and_then(Value::as_str)?;
    if let Some((media_type, data)) = split_data_url(url) {
        return Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }));
    }
    (url.starts_with("https://") || url.starts_with("http://"))
        .then(|| json!({"type": "image", "source": {"type": "url", "url": url}}))
}

/// Responses `input_file` -> Anthropic `document`. `file_data` holds a data URL
/// (a PDF, in practice), `file_url` a fetchable one.
fn document_block(part: &Value) -> Option<Value> {
    if let Some(file_data) = part.get("file_data").and_then(Value::as_str) {
        let (media_type, data) = split_data_url(file_data)?;
        return Some(json!({
            "type": "document",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }));
    }
    let url = part.get("file_url").and_then(Value::as_str)?;
    Some(json!({"type": "document", "source": {"type": "url", "url": url}}))
}

/// Responses `function_call` -> Anthropic `tool_use`. `arguments` is a
/// JSON-encoded string upstream and a JSON object here; arguments that do not
/// parse to an object become `{}` rather than failing the whole turn.
fn tool_use_block(item: &Value) -> Value {
    let input = item
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
        "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
        "input": input,
    })
}

/// Responses `function_call_output` -> Anthropic `tool_result`. A plain-string
/// output stays a string (what most tools return); a parts array becomes the
/// text/image blocks Anthropic accepts inside a tool result.
fn tool_result_block(item: &Value) -> Value {
    let content = match item.get("output") {
        Some(Value::String(text)) => json!(text),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(text_block),
                    Some("input_image") => image_block(part),
                    _ => None,
                })
                .collect(),
        ),
        _ => json!(""),
    };
    json!({
        "type": "tool_result",
        "tool_use_id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
        "content": content,
    })
}

/// Responses `tools` -> Anthropic `tools`. Function tools carry over; the
/// hosted web-search tool maps to Anthropic's own; every other built-in
/// (`code_interpreter`, `file_search`, …) has no Anthropic equivalent and is
/// dropped rather than registered as a function the client cannot run.
fn tools(request: &Value) -> Vec<Value> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| match tool.get("type").and_then(Value::as_str) {
            Some("function") => Some(function_tool(tool)),
            Some(kind) if WEB_SEARCH_TYPES.contains(&kind) => Some(web_search_tool(tool)),
            _ => None,
        })
        .collect()
}

fn function_tool(tool: &Value) -> Value {
    // `strict` has no Anthropic counterpart and is dropped.
    let mut out = json!({
        "name": tool.get("name").and_then(Value::as_str).unwrap_or(""),
        "input_schema": tool
            .get("parameters")
            .filter(|schema| schema.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    });
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        out["description"] = json!(description);
    }
    out
}

fn web_search_tool(tool: &Value) -> Value {
    let mut out = json!({"type": WEB_SEARCH_TYPE, "name": "web_search"});
    if let Some(allowed) = tool
        .pointer("/filters/allowed_domains")
        .filter(|domains| domains.is_array())
    {
        out["allowed_domains"] = allowed.clone();
    }
    if let Some(location) = tool.get("user_location").filter(|value| value.is_object()) {
        out["user_location"] = location.clone();
    }
    out
}

/// Responses `tool_choice` (plus `parallel_tool_calls`) -> Anthropic
/// `tool_choice`. Only called when at least one tool survived translation.
fn tool_choice(request: &Value) -> Option<Value> {
    let mut choice = match request.get("tool_choice") {
        Some(Value::String(mode)) => named_choice(mode),
        Some(Value::Object(_)) => {
            let choice = request.get("tool_choice")?;
            match choice.get("type").and_then(Value::as_str)? {
                "function" => choice
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| json!({"type": "tool", "name": name})),
                // `allowed_tools` restricts the callable set, which Anthropic
                // cannot express; its `mode` still carries the auto/required
                // decision, so keep that much.
                "allowed_tools" => {
                    named_choice(choice.get("mode").and_then(Value::as_str).unwrap_or("auto"))
                }
                mode => named_choice(mode),
            }
        }
        _ => None,
    };
    if request.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
        // Anthropic takes the flag on the choice object, and accepts it only on
        // the choices that can call a tool.
        let mut with_flag = choice.unwrap_or_else(|| json!({"type": "auto"}));
        if matches!(
            with_flag.get("type").and_then(Value::as_str),
            Some("auto" | "any" | "tool")
        ) {
            with_flag["disable_parallel_tool_use"] = json!(true);
        }
        choice = Some(with_flag);
    }
    choice
}

fn named_choice(mode: &str) -> Option<Value> {
    match mode {
        "auto" => Some(json!({"type": "auto"})),
        "none" => Some(json!({"type": "none"})),
        "required" => Some(json!({"type": "any"})),
        _ => None,
    }
}

/// Whether the resolved choice pins the model to calling a tool, which
/// Anthropic will not accept together with extended thinking.
fn forces_a_tool(choice: Option<&Value>) -> bool {
    matches!(
        choice
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str),
        Some("any" | "tool")
    )
}

/// Responses `reasoning.effort` -> Anthropic `thinking`. `none`/`minimal`/an
/// absent effort ask for no extended thinking. The budget is clamped to leave
/// `MIN_THINKING_BUDGET` of `max_tokens` for the answer, and thinking is
/// omitted entirely when that leaves less than Anthropic's floor.
fn thinking(request: &Value, max_tokens: u64) -> Option<Value> {
    let effort = request
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)?;
    let (_, budget) = EFFORT_BUDGETS.iter().find(|(level, _)| *level == effort)?;
    let budget = (*budget).min(max_tokens.saturating_sub(MIN_THINKING_BUDGET));
    (budget >= MIN_THINKING_BUDGET).then(|| json!({"type": "enabled", "budget_tokens": budget}))
}

/// Responses `text.format` -> Anthropic `output_format`. `json_object` and
/// `text` have no Anthropic counterpart and ask for nothing.
fn output_format(request: &Value) -> Option<Value> {
    let format = request.pointer("/text/format")?;
    if format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return None;
    }
    let schema = format.get("schema")?;
    Some(json!({"type": "json_schema", "schema": schema}))
}

#[cfg(test)]
mod tests;
