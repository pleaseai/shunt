use std::collections::HashMap;

use axum::http::StatusCode;
use serde_json::{json, Value};

use crate::model::responses_request::TOOL_SEARCH_NAME;
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
    /// Prompt-size estimate (local tiktoken) surfaced in the `message_start`
    /// `usage.input_tokens`. The Responses API only reports real usage at
    /// `response.completed`, so `message_start` would otherwise carry `0`.
    /// Native Anthropic puts a real `input_tokens` there, and Claude Code's
    /// per-subagent progress tracker reads usage from that first (yield-time)
    /// snapshot — so a `0` leaves codex subagents showing 0 context in the agent
    /// panel. Seeding an estimate mirrors Anthropic; the accurate value still
    /// lands in the terminal `message_delta` (see [`Self::usage_value`]).
    input_tokens_estimate: u64,
    /// Whether real prompt usage was observed from a `response.completed`
    /// event. Distinguishes "upstream reported `input_tokens: 0`" from "the
    /// stream ended before usage arrived", so [`Self::usage_value`] substitutes
    /// the estimate only in the latter case.
    usage_observed: bool,
    content: Vec<Value>,
    text_buffer: String,
    text_citations: Vec<Value>,
    tool_buffer: Option<ToolBuffer>,
    reasoning: Option<ReasoningBuffer>,
    web_search_indexes: HashMap<String, String>,
    /// Whether the request used the native `tool_search` protocol (issue #82).
    /// Only then is an upstream `tool_search_call` item surfaced as a `ToolSearch`
    /// `tool_use`; under the shim the upstream never emits one.
    tool_search_native: bool,
    /// The mapped Anthropic error envelope from a backend-sent `error` /
    /// `response.failed` event (issue #113). Backends deliver these as normal
    /// `Ok` events on a `200 OK` stream (rate-limit, content-policy refusal)
    /// rather than a non-2xx HTTP status. The streaming paths
    /// emit the envelope inline as an SSE `error` event and stop; the
    /// non-streaming JSON collectors take it here (via
    /// [`Self::take_backend_error`]) so they can return a gateway error instead
    /// of a `200 OK` carrying the partial/empty content accumulated so far.
    backend_error: Option<Value>,
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
    pub fn new(model: impl Into<String>, thinking_enabled: bool, tool_search_native: bool) -> Self {
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
            input_tokens_estimate: 0,
            usage_observed: false,
            content: Vec::new(),
            text_buffer: String::new(),
            text_citations: Vec::new(),
            tool_buffer: None,
            reasoning: None,
            web_search_indexes: HashMap::new(),
            tool_search_native,
            backend_error: None,
        }
    }

    /// Seed the `message_start` prompt-size estimate (see
    /// [`Self::input_tokens_estimate`]). The streaming paths set this from a
    /// local tiktoken count of the request; it defaults to `0` (unknown).
    #[must_use]
    pub fn with_input_estimate(mut self, input_tokens: u64) -> Self {
        self.input_tokens_estimate = input_tokens;
        self
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
                let value = map_error_value(&event.data, StatusCode::BAD_GATEWAY);
                // Build the SSE event first (borrowing `value`), then move
                // ownership into `backend_error` — avoids cloning the envelope.
                let sse_event = sse("error", &value);
                self.backend_error = Some(value);
                vec![sse_event]
            }
            _ => Vec::new(),
        }
    }

    /// Take the mapped Anthropic error envelope if a backend `error` /
    /// `response.failed` event was applied, else `None`. Moves ownership out of
    /// the machine so the non-streaming JSON collectors can hand it straight to
    /// the response body without cloning; the event is terminal, so the machine
    /// is dropped right after (issue #113).
    pub fn take_backend_error(&mut self) -> Option<Value> {
        self.backend_error.take()
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
                        // Seed the prompt-size estimate here (Responses reports
                        // real usage only at completion). Mirrors Anthropic so
                        // Claude Code's subagent progress tracker, which reads
                        // this first snapshot, shows nonzero context; the
                        // accurate total still arrives in `message_delta`.
                        "usage": {"input_tokens": self.input_tokens_estimate, "output_tokens": 0}
                    }
                }),
            ),
            sse("ping", &json!({"type": "ping"})),
        ]
    }

    fn output_item_added(&mut self, data: &Value) -> Vec<String> {
        let item = data.get("item").unwrap_or(data);
        match item.get("type").and_then(Value::as_str) {
            Some("message") => Vec::new(),
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
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => self.reasoning_done(item),
            Some("web_search_call") => self.web_search_done(item),
            // The native tool_search_call carries its full `arguments` (a JSON
            // object) only at `done` — codex ignores the `added`/delta events too
            // — so the whole ToolSearch tool_use is emitted here in one shot.
            Some("tool_search_call") if self.tool_search_native => self.tool_search_call_done(item),
            _ => self.close_any(),
        }
    }

    /// An upstream `tool_search_call` -> an Anthropic `tool_use` for `ToolSearch`,
    /// so Claude Code runs its inventory search and returns `tool_reference`s. The
    /// `call_id` becomes the tool_use id (round-tripping back to the request's
    /// `tool_search_call`/`tool_search_output`), and `arguments` (a JSON object)
    /// is emitted as one `input_json_delta`. Mirrors `web_search_done`'s
    /// open/delta/stop shape, and records the block for the non-streaming path.
    fn tool_search_call_done(&mut self, item: &Value) -> Vec<String> {
        let mut out = self.close_any();
        // Claude Code needs a non-empty tool_use id to match the tool_result it
        // sends back; if upstream ever omits `call_id`, fall back to a synthetic
        // per-block id rather than emit an empty (invalid) one.
        let id = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("toolu_ts_{}", self.index));
        // Anthropic requires tool_use `input` to be a JSON object; if upstream
        // omits `arguments` or sends a non-object, fall back to `{}` rather than
        // forward an invalid input.
        let arguments = item
            .get("arguments")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        self.saw_tool = true;
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "tool_use", "id": id, "name": TOOL_SEARCH_NAME, "input": {}}
            }),
        ));
        out.push(sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.index,
                "delta": {"type": "input_json_delta", "partial_json": arguments.to_string()}
            }),
        ));
        self.content.push(json!({
            "type": "tool_use", "id": id, "name": TOOL_SEARCH_NAME, "input": arguments
        }));
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.index}),
        ));
        self.index += 1;
        out
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
        // Do NOT clear `text_citations` here: `annotation_added` can buffer a
        // citation before the first `text_delta` opens the block, and
        // `close_any` already clears the buffer when a text block closes. A
        // clear at open time would drop those pre-delta citations.
        self.text_buffer.clear();
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        // Flush any citations buffered before this block opened. `annotation_added`
        // stores a citation but can't stream its `citations_delta` while no text
        // block is open, so emit them now that the block exists — otherwise
        // streaming clients only ever see them in the final reconstructed block.
        for citation in &self.text_citations {
            out.push(sse(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.index,
                    "delta": {"type": "citations_delta", "citation": citation}
                }),
            ));
        }
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
        let mut out = Vec::new();
        if self.open.as_ref().map(|block| block.kind) != Some(BlockKind::Text) {
            out.extend(self.open_text());
        }
        self.text_buffer.push_str(delta);
        out.push(sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "text_delta", "text": delta}
            }),
        ));
        out
    }

    fn annotation_added(&mut self, data: &Value) -> Vec<String> {
        let annotation = data.get("annotation").unwrap_or(data);
        if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
            return Vec::new();
        }
        let mut out = Vec::new();
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
        if self.open.as_ref().map(|block| block.kind) != Some(BlockKind::Text) {
            return out;
        }
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
                // Drop a whitespace-only text block from the reconstructed
                // `content` (Anthropic rejects empty text blocks), but still
                // emit the matching `content_block_stop` below and advance the
                // index: `open_text` already streamed a `content_block_start`
                // for this block, so suppressing the stop would leave an
                // unbalanced block and make the next block reuse this index.
                if !self.text_buffer.trim().is_empty() {
                    let mut block = json!({"type": "text", "text": self.text_buffer});
                    if !self.text_citations.is_empty() {
                        block["citations"] = json!(self.text_citations);
                    }
                    self.content.push(block);
                }
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
        // Fall back to the message_start estimate only when real usage was never
        // observed — i.e. the stream ended before response.completed, so
        // read_usage never ran. Without this a truncated turn's final usage would
        // drop back to 0 and undo the seed message_start already reported. The
        // explicit `usage_observed` flag (not a zero-check) means a genuine
        // upstream `input_tokens: 0` is still reported as 0, not overwritten by
        // the estimate. On non-streaming machines the estimate is never seeded
        // (it stays 0), so this preserves the real-0 behaviour.
        let input_tokens = if self.usage_observed {
            self.input_tokens
        } else {
            self.input_tokens_estimate
        };
        json!({
            "input_tokens": input_tokens,
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
            self.usage_observed = true;
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

// HTTP 529 ("upstream overloaded") has no named constant in the `http`
// crate — it isn't in the IANA registry. Anthropic uses it to mean "the
// upstream is at capacity"; Claude Code backs off and retries on it instead
// of failing the turn, so it must reach the client as its own status rather
// than folding into a generic `api_error`.

/// Single source of truth mapping an upstream HTTP status to both the Anthropic
/// error envelope's `error.type` and the client-facing HTTP status.
///
/// Deriving both from one table makes their shared invariant unbreakable: a
/// status can never be given a specific `error.type` (e.g. `permission_error`)
/// while its client-facing status silently falls back to `502`, which would
/// ship a self-contradictory envelope. `anthropic_error_type` and
/// `client_facing_status` are thin projections of this table.
///
/// The standard error statuses (400/401/403/413/429/500/501/502/503/504 and the
/// non-registry 529 overload) reach the client unchanged so status-based client
/// behavior (the `529` overload backoff, distinguishing `503`/`500` from a
/// generic gateway failure) sees the real upstream signal. Anything outside that
/// set collapses to `(api_error, 502)` rather than leaking an unexpected
/// upstream status verbatim. See `docs/gateway-protocol.md#error-envelopes`.
/// The second tuple element is whether the upstream `status` reaches the client
/// unchanged (`true`) or collapses to `502` (`false`); `client_facing_status`
/// projects it. Encoding it as a flag rather than repeating the `StatusCode` in
/// both the pattern and the return keeps each row's status listed once.
fn mapped_error(status: StatusCode) -> (&'static str, bool) {
    match status {
        StatusCode::BAD_REQUEST => ("invalid_request_error", true),
        StatusCode::UNAUTHORIZED => ("authentication_error", true),
        StatusCode::FORBIDDEN => ("permission_error", true),
        StatusCode::PAYLOAD_TOO_LARGE => ("request_too_large", true),
        StatusCode::TOO_MANY_REQUESTS => ("rate_limit_error", true),
        StatusCode::INTERNAL_SERVER_ERROR => ("api_error", true),
        StatusCode::NOT_IMPLEMENTED => ("not_supported", true),
        StatusCode::BAD_GATEWAY => ("api_error", true),
        StatusCode::SERVICE_UNAVAILABLE => ("api_error", true),
        StatusCode::GATEWAY_TIMEOUT => ("api_error", true),
        _ if status.as_u16() == 529 => ("overloaded_error", true),
        _ => ("api_error", false),
    }
}

/// Map an upstream HTTP status to the Anthropic error envelope's `error.type`,
/// per the table in `docs/gateway-protocol.md#error-envelopes`. Shared by every
/// translated backend (Responses/Codex, xAI, Cursor) so they surface the same
/// vocabulary the Anthropic-direct path streams verbatim. Projection of
/// `mapped_error`.
pub fn anthropic_error_type(status: StatusCode) -> &'static str {
    mapped_error(status).0
}

/// Client-facing HTTP status for a mapped upstream error. The standard error
/// statuses (400/401/403/413/429/500/501/502/503/504 and the non-registry 529
/// overload) reach the client unchanged; anything else collapses to `502`
/// rather than leaking an unexpected upstream status verbatim. Projection of
/// `mapped_error`, the shared source of truth this and `error.type` derive from.
pub fn client_facing_status(status: StatusCode) -> StatusCode {
    if mapped_error(status).1 {
        status
    } else {
        StatusCode::BAD_GATEWAY
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
pub fn context_overflow_message(value: &Value, message: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: Value) -> ResponseEvent {
        ResponseEvent {
            event: Some(name.to_string()),
            data,
        }
    }

    #[test]
    fn tool_only_turn_does_not_emit_empty_text_block() {
        let mut machine = AnthropicSseMachine::new("test", false, false);
        let mut output = Vec::new();
        output.extend(machine.apply(event(
            "response.output_item.added",
            json!({"item": {"type": "function_call", "call_id": "call_1", "name": "do_work"}}),
        )));
        output.extend(machine.apply(event(
            "response.function_call_arguments.delta",
            json!({"delta": "{}"}),
        )));
        output.extend(machine.apply(event("response.function_call_arguments.done", json!({}))));
        output.extend(machine.apply(event(
            "response.output_item.done",
            json!({"item": {"type": "function_call"}}),
        )));
        output.extend(machine.finish());
        let final_json = machine.final_json();

        assert!(output.iter().all(|frame| {
            !frame.contains("\"type\":\"text\"") && !frame.contains("\"text\":\"\"")
        }));
        assert!(final_json["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "text"));
    }

    #[test]
    fn reasoning_only_turn_does_not_emit_empty_text_block() {
        let mut machine = AnthropicSseMachine::new("test", true, false);
        let mut output = machine.apply(event(
            "response.output_item.added",
            json!({"item": {"type": "reasoning", "id": "reason_1"}}),
        ));
        output.extend(machine.apply(event(
            "response.reasoning_summary_text.delta",
            json!({"delta": "Thinking"}),
        )));
        output.extend(machine.apply(event(
            "response.output_item.done",
            json!({"item": {"type": "reasoning", "id": "reason_1"}}),
        )));
        output.extend(machine.finish());
        let final_json = machine.final_json();

        assert!(output.iter().all(|frame| {
            !frame.contains("\"type\":\"text\"") && !frame.contains("\"text\":\"\"")
        }));
        assert!(final_json["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "text"));
    }

    #[test]
    fn text_turn_still_streams_text_content() {
        let mut machine = AnthropicSseMachine::new("test", false, false);
        let mut output = machine.apply(event(
            "response.output_item.added",
            json!({"item": {"type": "message"}}),
        ));
        output.extend(machine.apply(event(
            "response.output_text.delta",
            json!({"delta": "Hello"}),
        )));
        output.extend(machine.apply(event("response.output_text.done", json!({}))));
        output.extend(machine.finish());
        let final_json = machine.final_json();

        assert!(output.iter().any(|frame| frame.contains("text_delta")));
        assert_eq!(
            final_json["content"][0],
            json!({"type": "text", "text": "Hello"})
        );
    }

    #[test]
    fn whitespace_only_text_keeps_the_sse_stream_balanced() {
        // A whitespace-only delta still opens a text block (open_text streams a
        // content_block_start), so closing it must emit the matching
        // content_block_stop and advance the index. Otherwise the following
        // tool block reuses the same index and the first block is never closed.
        let mut machine = AnthropicSseMachine::new("test", false, false);
        let mut output = machine.apply(event(
            "response.output_item.added",
            json!({"item": {"type": "message"}}),
        ));
        output.extend(machine.apply(event("response.output_text.delta", json!({"delta": "  "}))));
        output.extend(machine.apply(event(
            "response.output_item.added",
            json!({"item": {"type": "function_call", "call_id": "call_1", "name": "do_work"}}),
        )));
        output.extend(machine.apply(event(
            "response.function_call_arguments.delta",
            json!({"delta": "{}"}),
        )));
        output.extend(machine.apply(event("response.function_call_arguments.done", json!({}))));
        output.extend(machine.apply(event(
            "response.output_item.done",
            json!({"item": {"type": "function_call"}}),
        )));
        output.extend(machine.finish());
        let final_json = machine.final_json();

        let starts = output
            .iter()
            .filter(|frame| frame.contains("event: content_block_start"))
            .count();
        let stops = output
            .iter()
            .filter(|frame| frame.contains("event: content_block_stop"))
            .count();
        assert_eq!(starts, stops, "every content_block_start needs a stop");

        // No two content_block_start frames may share an index.
        let start_indexes: Vec<&str> = output
            .iter()
            .filter(|frame| frame.contains("event: content_block_start"))
            .map(|frame| {
                let marker = "\"index\":";
                let start = frame.find(marker).unwrap() + marker.len();
                let rest = &frame[start..];
                let end = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect();
        let mut unique = start_indexes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            start_indexes.len(),
            unique.len(),
            "content_block_start indexes must be distinct"
        );

        // The whitespace-only text block is still dropped from `content`.
        assert!(final_json["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "text"));
    }

    #[test]
    fn citation_before_first_text_delta_is_preserved() {
        // `annotation.added` can arrive before the first `output_text.delta`.
        // It buffers the citation in `text_citations`; the subsequent
        // `open_text` must not clear that buffer, or the pre-delta citation is
        // lost from the reconstructed text block.
        let mut machine = AnthropicSseMachine::new("test", false, false);
        let mut output = machine.apply(event(
            "response.output_item.added",
            json!({"item": {"type": "message"}}),
        ));
        output.extend(machine.apply(event(
            "response.output_text.annotation.added",
            json!({"annotation": {
                "type": "url_citation",
                "url": "https://example.com",
                "title": "Example",
                "cited_text": "quoted"
            }}),
        )));
        output.extend(machine.apply(event(
            "response.output_text.delta",
            json!({"delta": "Hello"}),
        )));
        output.extend(machine.apply(event("response.output_text.done", json!({}))));
        output.extend(machine.finish());
        let final_json = machine.final_json();

        // The buffered citation survives into the final reconstructed block...
        assert_eq!(
            final_json["content"][0],
            json!({
                "type": "text",
                "text": "Hello",
                "citations": [{
                    "type": "web_search_result_location",
                    "url": "https://example.com",
                    "title": "Example",
                    "cited_text": "quoted"
                }]
            })
        );
        // ...and streaming clients also receive it as a `citations_delta`, flushed
        // when the lazily-opened text block starts.
        assert!(
            output
                .iter()
                .any(|frame| frame.contains("citations_delta")
                    && frame.contains("https://example.com")),
            "pre-delta citation must be streamed as a citations_delta frame"
        );
    }
}
