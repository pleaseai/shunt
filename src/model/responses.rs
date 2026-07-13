use std::collections::HashMap;

use axum::http::StatusCode;
use serde_json::{json, Value};

pub use crate::model::responses_request::{encode_reasoning_signature, translate_request};

#[derive(Debug, Clone)]
pub struct ResponseEvent {
    pub event: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone)]
struct OpenBlock {
    index: usize,
    kind: BlockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool,
    Reasoning,
}

#[derive(Debug, Clone)]
pub struct AnthropicSseMachine {
    id: String,
    model: String,
    started: bool,
    stopped: bool,
    index: usize,
    open: Option<OpenBlock>,
    saw_tool: bool,
    thinking_enabled: bool,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    content: Vec<Value>,
    text_buffer: String,
    text_citations: Vec<Value>,
    tool_buffer: Option<ToolBuffer>,
    reasoning: Option<ReasoningBuffer>,
    web_search_indexes: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct ToolBuffer {
    id: String,
    name: String,
    json: String,
}

/// Accumulates a Responses `reasoning` output item so it can be surfaced as an
/// Anthropic thinking block. `signature` packs the item's id + encrypted_content
/// (set at `output_item.done`) so the next turn can round-trip it (see
/// [`encode_reasoning_signature`]).
#[derive(Debug, Clone)]
struct ReasoningBuffer {
    id: String,
    summary: String,
    signature: Option<String>,
}

impl AnthropicSseMachine {
    pub fn new(model: impl Into<String>, thinking_enabled: bool) -> Self {
        Self {
            id: "msg_responses".to_string(),
            model: model.into(),
            started: false,
            stopped: false,
            index: 0,
            open: None,
            saw_tool: false,
            thinking_enabled,
            input_tokens: 0,
            cache_read_tokens: 0,
            output_tokens: 0,
            content: Vec::new(),
            text_buffer: String::new(),
            text_citations: Vec::new(),
            tool_buffer: None,
            reasoning: None,
            web_search_indexes: HashMap::new(),
        }
    }

    pub fn apply(&mut self, event: ResponseEvent) -> Vec<String> {
        if self.stopped {
            return Vec::new();
        }
        let name = event.event.as_deref().unwrap_or("");
        match name {
            "response.created" | "response.in_progress" => self.start(&event.data),
            "response.output_item.added" => self.output_item_added(&event.data),
            "response.output_text.delta" => self.text_delta(&event.data),
            "response.output_text.annotation.added" => self.annotation_added(&event.data),
            "response.output_text.done" | "response.content_part.done" => {
                self.close_current(BlockKind::Text)
            }
            "response.reasoning_summary_text.delta" => self.reasoning_delta(&event.data),
            "response.function_call_arguments.delta" => self.arguments_delta(&event.data),
            "response.function_call_arguments.done" => self.close_current(BlockKind::Tool),
            "response.output_item.done" => self.output_item_done(&event.data),
            "response.completed" | "response.done" => self.complete(&event.data),
            "error" | "response.failed" => {
                self.stopped = true;
                vec![sse(
                    "error",
                    &map_error_value(&event.data, StatusCode::BAD_GATEWAY),
                )]
            }
            _ => Vec::new(),
        }
    }

    pub fn finish(&mut self) -> Vec<String> {
        if self.stopped {
            return Vec::new();
        }
        let mut out = self.close_any();
        out.extend(self.stop_events("end_turn"));
        out
    }

    pub fn final_json(&mut self) -> Value {
        if !self.stopped {
            let _ = self.finish();
        }
        json!({
            "id": self.id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": self.content,
            "stop_reason": if self.saw_tool { "tool_use" } else { "end_turn" },
            "stop_sequence": null,
            "usage": self.usage_value(),
        })
    }

    fn start(&mut self, data: &Value) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if let Some(id) = data.pointer("/response/id").or_else(|| data.get("id")) {
            if let Some(id) = id.as_str() {
                self.id = id.to_string();
            }
        }
        vec![
            sse(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                }),
            ),
            sse("ping", &json!({"type": "ping"})),
        ]
    }

    fn output_item_added(&mut self, data: &Value) -> Vec<String> {
        let item = data.get("item").unwrap_or(data);
        match item.get("type").and_then(Value::as_str) {
            Some("message") => self.open_text(),
            Some("function_call") => self.open_tool(item),
            Some("reasoning") => self.reasoning_added(item),
            _ => Vec::new(),
        }
    }

    /// `response.output_item.done` closes whichever item just finished. Reasoning
    /// items need special handling (stamp the round-trip signature); function_call
    /// and message items just close.
    fn output_item_done(&mut self, data: &Value) -> Vec<String> {
        let item = data.get("item").unwrap_or(data);
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            return self.reasoning_done(item);
        }
        if item.get("type").and_then(Value::as_str) == Some("web_search_call") {
            return self.web_search_done(item);
        }
        self.close_any()
    }

    /// Record the reasoning item's id; defer opening the thinking block until the
    /// first summary delta (or `output_item.done` when there is encrypted content),
    /// so a reasoning item with neither summary nor encrypted content emits nothing.
    fn reasoning_added(&mut self, item: &Value) -> Vec<String> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.reasoning = Some(ReasoningBuffer {
            id,
            summary: String::new(),
            signature: None,
        });
        Vec::new()
    }

    fn open_reasoning(&mut self) -> Vec<String> {
        let mut out = self.close_any();
        if self.reasoning.is_none() {
            self.reasoning = Some(ReasoningBuffer {
                id: String::new(),
                summary: String::new(),
                signature: None,
            });
        }
        self.open = Some(OpenBlock {
            index: self.index,
            kind: BlockKind::Reasoning,
        });
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        ));
        out
    }

    fn reasoning_delta(&mut self, data: &Value) -> Vec<String> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
        if delta.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.open.as_ref().map(|block| block.kind) != Some(BlockKind::Reasoning) {
            out.extend(self.open_reasoning());
        }
        if let Some(reasoning) = &mut self.reasoning {
            reasoning.summary.push_str(delta);
        }
        out.push(sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        ));
        out
    }

    fn reasoning_done(&mut self, item: &Value) -> Vec<String> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let encrypted = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_open = self.open.as_ref().map(|block| block.kind) == Some(BlockKind::Reasoning);
        // Nothing to show (no summary streamed) and nothing to round-trip.
        if !is_open && encrypted.is_empty() {
            self.reasoning = None;
            return Vec::new();
        }
        let mut out = Vec::new();
        if !is_open {
            // Open an empty thinking block purely to carry the round-trip signature.
            out.extend(self.open_reasoning());
        }
        if !encrypted.is_empty() {
            // Prefer the id captured at output_item.added; fall back to the id on
            // this done event so the round-trip keeps a real reasoning-item id even
            // if the added event was missed or carried none.
            let id = self
                .reasoning
                .as_ref()
                .map(|reasoning| reasoning.id.clone())
                .filter(|id| !id.is_empty())
                .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            let signature = encode_reasoning_signature(&id, encrypted);
            if let Some(reasoning) = &mut self.reasoning {
                reasoning.signature = Some(signature.clone());
            }
            out.push(sse(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.open_index(),
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            ));
        }
        out.extend(self.close_any());
        out
    }

    fn open_text(&mut self) -> Vec<String> {
        let mut out = self.close_any();
        self.open = Some(OpenBlock {
            index: self.index,
            kind: BlockKind::Text,
        });
        self.text_buffer.clear();
        self.text_citations.clear();
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        out
    }

    fn open_tool(&mut self, item: &Value) -> Vec<String> {
        let mut out = self.close_any();
        let id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.saw_tool = true;
        self.tool_buffer = Some(ToolBuffer {
            id: id.clone(),
            name: name.clone(),
            json: String::new(),
        });
        self.open = Some(OpenBlock {
            index: self.index,
            kind: BlockKind::Tool,
        });
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
            }),
        ));
        out
    }

    fn text_delta(&mut self, data: &Value) -> Vec<String> {
        let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
        if delta.is_empty() {
            return Vec::new();
        }
        self.text_buffer.push_str(delta);
        vec![sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "text_delta", "text": delta}
            }),
        )]
    }

    fn annotation_added(&mut self, data: &Value) -> Vec<String> {
        let annotation = data.get("annotation").unwrap_or(data);
        if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.open.as_ref().map(|block| block.kind) != Some(BlockKind::Text) {
            out.extend(self.open_text());
        }
        let url = annotation.get("url").and_then(Value::as_str).unwrap_or("");
        let encrypted_index = annotation
            .get("encrypted_index")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.web_search_indexes.get(url).cloned())
            .map(Value::String)
            .unwrap_or(Value::Null);
        let title = annotation
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("");
        let cited_text = annotation
            .get("cited_text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut citation = json!({
            "type": "web_search_result_location",
            "url": url,
            "title": title,
            "cited_text": cited_text,
            "encrypted_index": encrypted_index,
        });
        citation
            .as_object_mut()
            .expect("citation is an object")
            .retain(|_, value| !value.is_null());
        self.text_citations.push(citation.clone());
        out.push(sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "citations_delta", "citation": citation}
            }),
        ));
        out
    }

    fn web_search_done(&mut self, item: &Value) -> Vec<String> {
        let mut out = self.close_any();
        let id = item
            .get("id")
            .or_else(|| item.get("call_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let input = item
            .get("action")
            .cloned()
            .or_else(|| item.get("input").cloned())
            .unwrap_or_else(|| json!({}));
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "server_tool_use", "id": id, "name": "web_search", "input": input}
            }),
        ));
        self.content.push(json!({
            "type": "server_tool_use", "id": id, "name": "web_search", "input": input
        }));
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.index}),
        ));
        self.index += 1;

        let results = item
            .get("results")
            .or_else(|| item.get("output"))
            .filter(|results| results.is_array())
            .cloned()
            .unwrap_or_else(|| json!([]));
        if let Some(results) = results.as_array() {
            for result in results {
                if let (Some(url), Some(encrypted_content)) = (
                    result.get("url").and_then(Value::as_str),
                    result.get("encrypted_content").and_then(Value::as_str),
                ) {
                    self.web_search_indexes
                        .insert(url.to_string(), encrypted_content.to_string());
                }
            }
        }
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "web_search_tool_result", "tool_use_id": id, "content": results}
            }),
        ));
        self.content.push(json!({
            "type": "web_search_tool_result", "tool_use_id": id, "content": results
        }));
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.index}),
        ));
        self.index += 1;
        out
    }

    fn arguments_delta(&mut self, data: &Value) -> Vec<String> {
        let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
        if let Some(tool) = &mut self.tool_buffer {
            tool.json.push_str(delta);
        }
        vec![sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "input_json_delta", "partial_json": delta}
            }),
        )]
    }

    fn close_current(&mut self, expected: BlockKind) -> Vec<String> {
        if self.open.as_ref().map(|block| block.kind) != Some(expected) {
            return Vec::new();
        }
        self.close_any()
    }

    fn close_any(&mut self) -> Vec<String> {
        let Some(open) = self.open.take() else {
            return Vec::new();
        };
        match open.kind {
            BlockKind::Text => {
                let mut block = json!({"type": "text", "text": self.text_buffer});
                if !self.text_citations.is_empty() {
                    block["citations"] = json!(self.text_citations);
                }
                self.content.push(block);
                self.text_buffer.clear();
                self.text_citations.clear();
            }
            BlockKind::Tool => {
                if let Some(tool) = self.tool_buffer.take() {
                    let input = serde_json::from_str(&tool.json).unwrap_or_else(|_| json!({}));
                    self.content.push(json!({
                        "type": "tool_use",
                        "id": tool.id,
                        "name": tool.name,
                        "input": input
                    }));
                }
            }
            BlockKind::Reasoning => {
                if let Some(reasoning) = self.reasoning.take() {
                    let mut block = json!({"type": "thinking", "thinking": reasoning.summary});
                    if let Some(signature) = reasoning.signature {
                        block["signature"] = json!(signature);
                    }
                    self.content.push(block);
                }
            }
        }
        self.index += 1;
        vec![sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": open.index}),
        )]
    }

    fn complete(&mut self, data: &Value) -> Vec<String> {
        self.read_usage(data);
        let mut out = self.close_any();
        let stop_reason = if self.saw_tool {
            "tool_use"
        } else {
            "end_turn"
        };
        out.extend(self.stop_events(stop_reason));
        out
    }

    fn stop_events(&mut self, stop_reason: &str) -> Vec<String> {
        self.stopped = true;
        vec![
            sse(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                    // Carry input_tokens here (not message_start): the Responses
                    // API only reports usage at response.completed, so this is the
                    // first point shunt knows the prompt size. The Anthropic SDK
                    // merges message_delta usage into the message, which is what
                    // Claude Code reads for its context-window indicator.
                    "usage": self.usage_value()
                }),
            ),
            sse("message_stop", &json!({"type": "message_stop"})),
        ]
    }

    /// Anthropic-shaped usage. Claude Code's context indicator sums
    /// input_tokens + cache_read + cache_creation, so the split must preserve the
    /// total. OpenAI's `input_tokens` already includes cached tokens, so
    /// cache_read is peeled off and input_tokens holds the uncached remainder.
    fn usage_value(&self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "cache_read_input_tokens": self.cache_read_tokens,
            "cache_creation_input_tokens": 0,
            "output_tokens": self.output_tokens,
        })
    }

    fn read_usage(&mut self, data: &Value) {
        let Some(usage) = data
            .pointer("/response/usage")
            .or_else(|| data.get("usage"))
        else {
            return;
        };
        if let Some(tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = tokens;
        }
        // OpenAI `input_tokens` counts total prompt tokens including cached ones;
        // peel the cached portion into cache_read so the sum still equals the
        // prompt size Claude Code charts against the context window.
        let cached = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(total_input) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.cache_read_tokens = cached.min(total_input);
            self.input_tokens = total_input - self.cache_read_tokens;
        }
    }

    fn open_index(&self) -> usize {
        self.open
            .as_ref()
            .map(|block| block.index)
            .unwrap_or(self.index)
    }
}

pub fn parse_sse_events(input: &str) -> Vec<ResponseEvent> {
    input
        .split("\n\n")
        .filter_map(|frame| {
            let mut event = None;
            let mut data = Vec::new();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.trim_start());
                }
            }
            let data = data.join("\n");
            if data.is_empty() || data == "[DONE]" {
                return None;
            }
            serde_json::from_str(&data)
                .ok()
                .map(|data| ResponseEvent { event, data })
        })
        .collect()
}

pub fn map_error_value(value: &Value, status: StatusCode) -> Value {
    let message = error_message(value);
    let message = context_overflow_message(value, &message).unwrap_or(message);
    json!({
        "type": "error",
        "error": {
            "type": anthropic_error_type(status),
            "message": message
        }
    })
}

pub fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::BAD_REQUEST => "invalid_request_error",
        _ => "api_error",
    }
}

fn error_message(value: &Value) -> String {
    // OpenAI Responses errors use {"error":{"message":...}} or {"message":...};
    // streaming `response.failed` events nest it at {"response":{"error":...}};
    // the ChatGPT Codex backend uses {"detail":...}; xAI puts the human-readable
    // reason in a top-level STRING `error` (e.g. a 402 out-of-credits body:
    // {"error":"...upgrade at grok.com/supergrok","code":"..."}). Surface whichever
    // is present so the client sees the real reason instead of a generic fallback.
    value
        .pointer("/error/message")
        .or_else(|| value.pointer("/response/error/message"))
        .or_else(|| value.get("message"))
        .or_else(|| value.get("detail"))
        .or_else(|| value.get("error").filter(|error| error.is_string()))
        .and_then(Value::as_str)
        .unwrap_or("upstream request failed")
        .to_string()
}

/// Rewrite upstream context-overflow errors into Anthropic's wording.
///
/// Claude Code's automatic compact-and-retry only fires when the error
/// message contains the phrase "prompt is too long" (matched
/// case-insensitively), and it parses "N tokens > M maximum" from it to
/// size the retry. Upstream providers phrase the same failure in their own
/// words, which would otherwise strand the session until a manual /compact.
fn context_overflow_message(value: &Value, message: &str) -> Option<String> {
    let code = value
        .pointer("/error/code")
        .or_else(|| value.pointer("/response/error/code"))
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let lower = message.to_lowercase();
    // "exceeds the limit" alone also appears in quota/rate errors ("exceeds the
    // limit of 1000000 tokens per minute"); requiring "prompt" or "token count"
    // pins that branch to prompt-size messages.
    let is_overflow = code == "context_length_exceeded"
        || lower.contains("maximum context length")
        || (lower.contains("context window") && lower.contains("exceed"))
        || (lower.contains("exceeds the limit")
            && (lower.contains("prompt") || lower.contains("token count")));
    if !is_overflow {
        return None;
    }
    // Token counts and window sizes are the only large integers in these
    // messages (small ones come from model names like "gpt-5.2"), and the
    // overflowing count is always the larger of the two. Commas and
    // underscores are digit-group separators ("272,000"), not delimiters.
    let mut numbers: Vec<i64> = Vec::new();
    let mut current: Option<i64> = None;
    for ch in message.chars().filter(|ch| !matches!(ch, ',' | '_')) {
        if let Some(digit) = ch.to_digit(10) {
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(i64::from(digit)),
            );
        } else if let Some(number) = current.take() {
            if number >= 1000 {
                numbers.push(number);
            }
        }
    }
    if let Some(number) = current {
        if number >= 1000 {
            numbers.push(number);
        }
    }
    match (numbers.iter().max(), numbers.iter().min()) {
        (Some(&actual), Some(&limit)) if actual > limit => Some(format!(
            "prompt is too long: {actual} tokens > {limit} maximum"
        )),
        _ => Some("prompt is too long".to_string()),
    }
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}
