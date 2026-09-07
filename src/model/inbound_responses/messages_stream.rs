//! Anthropic Messages → OpenAI Responses.
//!
//! [`MessagesSseMachine`] is the mirror image of
//! [`crate::model::responses::AnthropicSseMachine`]: it takes the Anthropic SSE
//! events a `kind = "anthropic"` upstream produced and re-emits the turn as the
//! Responses events the inbound Codex endpoint's client expects. One upstream
//! event in, the Responses frames it justifies out — nothing waits for the turn
//! to end.
//!
//! All Responses-side shape lives in [`super::events`]; this module only maps
//! Anthropic's content-block grammar onto it. Anthropic keys its block events
//! by a per-message `index`, so the machine keeps an `index → open item` map
//! and hands the emitter the `output_index` it allocated. Thinking blocks
//! round-trip through [`super::reasoning`], since Anthropic only accepts one
//! back when it carries the signature it issued.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::events::{
    function_call_item, item_id, message_item, reasoning_item, web_search_item, ResponsesEmitter,
    Usage,
};
use super::reasoning::{encode_thinking, Thinking};
use crate::model::responses::ResponseEvent;

/// The `code` on the `response.failed` a stream that stopped before
/// `message_stop` produces.
const TRUNCATED_CODE: &str = "upstream_stream_truncated";

/// What an Anthropic content-block `index` is currently mapped to.
#[derive(Debug, Clone)]
enum Block {
    Text {
        output_index: usize,
    },
    /// Buffered alongside the emitter's own copy because closing the item needs
    /// the thinking text *and* the signature together, and the signature only
    /// arrives in a trailing `signature_delta`.
    Reasoning {
        output_index: usize,
        thinking: String,
        signature: Option<String>,
    },
    Tool {
        output_index: usize,
    },
    /// A hosted `server_tool_use` (web search). It has no Responses item until
    /// it finishes: the query arrives as `input_json_delta` fragments, so the
    /// whole `web_search_call` is emitted at `content_block_stop`.
    WebSearch {
        id: String,
        input: String,
    },
}

#[derive(Debug, Clone)]
pub struct MessagesSseMachine {
    emitter: ResponsesEmitter,
    blocks: HashMap<usize, Block>,
    usage: Usage,
    stop_reason: Option<String>,
}

impl MessagesSseMachine {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            emitter: ResponsesEmitter::new(model),
            blocks: HashMap::new(),
            usage: Usage::default(),
            stop_reason: None,
        }
    }

    /// Whether a terminal Responses event has already been emitted. A caller
    /// that sees this can stop relaying without calling [`Self::finish`].
    pub fn is_terminal(&self) -> bool {
        self.emitter.is_terminal()
    }

    pub fn apply(&mut self, event: ResponseEvent) -> Vec<String> {
        if self.is_terminal() {
            return Vec::new();
        }
        let data = &event.data;
        match event.event.as_deref().unwrap_or("") {
            "message_start" => {
                if let Some(usage) = data.pointer("/message/usage") {
                    self.read_input_usage(usage);
                }
                self.emitter.created()
            }
            "content_block_start" => self.block_start(data),
            "content_block_delta" => self.block_delta(data),
            "content_block_stop" => self.block_stop(block_index(data)),
            "message_delta" => {
                if let Some(reason) = data
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    self.stop_reason = Some(reason);
                }
                if let Some(tokens) = data.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.usage.output_tokens = tokens;
                }
                Vec::new()
            }
            "message_stop" => {
                let mut frames = self.close_open_blocks();
                let usage = self.usage.clone();
                // `max_tokens` is the one Anthropic stop reason that is not a
                // clean end of turn: Responses reports it as an incomplete
                // response, not a completed one.
                frames.push(if self.stop_reason.as_deref() == Some("max_tokens") {
                    self.emitter.incomplete("max_output_tokens", usage)
                } else {
                    self.emitter.completed(usage)
                });
                frames
            }
            // Anthropic delivers in-stream failures as a normal event on a
            // 200 OK body. Surface the same failure on the Responses side and
            // stop: the turn is over either way.
            "error" => {
                let mut frames = self.close_open_blocks();
                let kind = data
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let message = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("the upstream reported an error");
                frames.push(self.emitter.failed(kind, message));
                frames
            }
            _ => Vec::new(),
        }
    }

    /// The upstream stream ended without `message_stop`. Close whatever is open
    /// and fail the response, so the client sees a terminal event rather than a
    /// silently truncated turn.
    pub fn finish(&mut self) -> Vec<String> {
        if self.is_terminal() {
            return Vec::new();
        }
        let mut frames = self.close_open_blocks();
        frames.push(self.emitter.failed(
            TRUNCATED_CODE,
            "the upstream stream ended before the message completed",
        ));
        frames
    }

    fn block_start(&mut self, data: &Value) -> Vec<String> {
        let index = block_index(data);
        let block = data.get("content_block").unwrap_or(data);
        let field = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default();
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let (output_index, frames) = self.emitter.open_message();
                self.blocks.insert(index, Block::Text { output_index });
                frames
            }
            Some("thinking") => {
                let (output_index, frames) = self.emitter.open_reasoning();
                self.blocks.insert(
                    index,
                    Block::Reasoning {
                        output_index,
                        thinking: field("thinking").to_string(),
                        signature: None,
                    },
                );
                frames
            }
            // A redacted block has no text and takes no deltas — its whole
            // payload is the opaque `data`, so open and close it in one step.
            Some("redacted_thinking") => {
                let encrypted = encode_thinking(&Thinking::Redacted {
                    data: field("data").to_string(),
                });
                let (output_index, mut frames) = self.emitter.open_reasoning();
                frames.extend(self.emitter.close_reasoning(output_index, Some(encrypted)));
                frames
            }
            Some("tool_use") => {
                let (output_index, frames) =
                    self.emitter.open_function_call(field("id"), field("name"));
                self.blocks.insert(index, Block::Tool { output_index });
                frames
            }
            Some("server_tool_use") => {
                self.blocks.insert(
                    index,
                    Block::WebSearch {
                        id: field("id").to_string(),
                        input: String::new(),
                    },
                );
                Vec::new()
            }
            // `web_search_tool_result` carries the search results Anthropic
            // already folded into the answer text; Responses has no item for
            // them, so it is dropped rather than invented.
            _ => Vec::new(),
        }
    }

    fn block_delta(&mut self, data: &Value) -> Vec<String> {
        let index = block_index(data);
        let delta = data.get("delta").unwrap_or(data);
        let field = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default();
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match (kind, self.blocks.get_mut(&index)) {
            ("text_delta", Some(Block::Text { output_index })) => {
                let output_index = *output_index;
                vec![self.emitter.text_delta(output_index, field("text"))]
            }
            (
                "thinking_delta",
                Some(Block::Reasoning {
                    output_index,
                    thinking,
                    ..
                }),
            ) => {
                let text = field("thinking");
                thinking.push_str(text);
                let output_index = *output_index;
                vec![self.emitter.reasoning_delta(output_index, text)]
            }
            ("signature_delta", Some(Block::Reasoning { signature, .. })) => {
                signature
                    .get_or_insert_with(String::new)
                    .push_str(field("signature"));
                Vec::new()
            }
            ("input_json_delta", Some(Block::Tool { output_index })) => {
                let output_index = *output_index;
                vec![self
                    .emitter
                    .arguments_delta(output_index, field("partial_json"))]
            }
            ("input_json_delta", Some(Block::WebSearch { input, .. })) => {
                input.push_str(field("partial_json"));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn block_stop(&mut self, index: usize) -> Vec<String> {
        match self.blocks.remove(&index) {
            Some(Block::Text { output_index }) => self.emitter.close_message(output_index),
            Some(Block::Reasoning {
                output_index,
                thinking,
                signature,
            }) => {
                // Without a signature Anthropic would reject the block on the
                // next turn, so there is nothing worth round-tripping.
                let encrypted = signature.map(|signature| {
                    encode_thinking(&Thinking::Signed {
                        thinking,
                        signature,
                    })
                });
                self.emitter.close_reasoning(output_index, encrypted)
            }
            Some(Block::Tool { output_index }) => self.emitter.close_function_call(output_index),
            Some(Block::WebSearch { id, input }) => {
                let query = serde_json::from_str::<Value>(&input)
                    .ok()
                    .and_then(|input| {
                        input
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                let id = (!id.is_empty()).then_some(id);
                self.emitter
                    .web_search_call(id.as_deref(), query.as_deref())
            }
            None => Vec::new(),
        }
    }

    /// Close every block still open, lowest `output_index` first, so a
    /// truncated or failed turn still reports well-formed items.
    fn close_open_blocks(&mut self) -> Vec<String> {
        let mut indexes: Vec<usize> = self.blocks.keys().copied().collect();
        indexes.sort_unstable();
        indexes
            .into_iter()
            .flat_map(|index| self.block_stop(index))
            .collect()
    }

    fn read_input_usage(&mut self, usage: &Value) {
        self.usage = input_usage(usage, self.usage.output_tokens);
    }
}

/// Anthropic reports the cached and cache-written portions of the prompt
/// *outside* `input_tokens`, while Responses reports one total with the cached
/// portion as a detail. Fold them back together — the exact inverse of
/// `AnthropicSseMachine`'s split.
fn input_usage(usage: &Value, output_tokens: u64) -> Usage {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cached = count("cache_read_input_tokens");
    Usage {
        input_tokens: count("input_tokens")
            .saturating_add(cached)
            .saturating_add(count("cache_creation_input_tokens")),
        cached_input_tokens: cached,
        output_tokens: count("output_tokens").max(output_tokens),
        reasoning_tokens: 0,
    }
}

fn block_index(data: &Value) -> usize {
    data.get("index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .try_into()
        .unwrap_or(usize::MAX)
}

/// A non-streaming Anthropic message → the Responses response body. Built from
/// the same item constructors the streaming path emits, so the two paths cannot
/// drift.
pub fn translate_response(message: &Value, model: &str) -> Value {
    let mut emitter = ResponsesEmitter::new(model);
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default();
        let item = match block.get("type").and_then(Value::as_str) {
            Some("text") => message_item(&item_id("msg_"), field("text"), "completed"),
            Some("thinking") => {
                let signature = block.get("signature").and_then(Value::as_str);
                let encrypted = signature.map(|signature| {
                    encode_thinking(&Thinking::Signed {
                        thinking: field("thinking").to_string(),
                        signature: signature.to_string(),
                    })
                });
                reasoning_item(
                    &item_id("rs_"),
                    Some(field("thinking")),
                    encrypted.as_deref(),
                )
            }
            Some("redacted_thinking") => {
                let encrypted = encode_thinking(&Thinking::Redacted {
                    data: field("data").to_string(),
                });
                reasoning_item(&item_id("rs_"), None, Some(&encrypted))
            }
            Some("tool_use") => {
                let arguments = block
                    .get("input")
                    .filter(|input| !input.is_null())
                    .map_or_else(|| "{}".to_string(), Value::to_string);
                function_call_item(
                    &item_id("fc_"),
                    field("id"),
                    field("name"),
                    &arguments,
                    "completed",
                )
            }
            Some("server_tool_use") => {
                let query = block.pointer("/input/query").and_then(Value::as_str);
                let id = field("id");
                let id = if id.is_empty() {
                    item_id("ws_")
                } else {
                    id.to_string()
                };
                web_search_item(&id, query, "completed")
            }
            _ => continue,
        };
        emitter.push_item(item);
    }

    let usage = message
        .get("usage")
        .map(|usage| input_usage(usage, 0))
        .unwrap_or_default();
    let truncated = message.get("stop_reason").and_then(Value::as_str) == Some("max_tokens");
    let status = if truncated { "incomplete" } else { "completed" };
    let mut response = emitter.response_object(status, Some(&usage));
    if truncated {
        response["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    response
}

/// An Anthropic error envelope → the OpenAI error envelope the inbound Codex
/// endpoint's clients parse. A body that is not the shape Anthropic documents
/// is reported as a generic `api_error` rather than echoed back — an
/// unrecognised body may be an intermediary's HTML or a provider payload the
/// client should not be handed.
pub fn translate_error(anthropic_error: &Value) -> Value {
    let kind = anthropic_error
        .pointer("/error/type")
        .and_then(Value::as_str);
    let message = anthropic_error
        .pointer("/error/message")
        .and_then(Value::as_str);
    let (kind, message) = match (kind, message) {
        (Some(kind), Some(message)) => (kind, message),
        _ => ("api_error", "the upstream returned an unrecognised error"),
    };
    json!({"error": {"type": kind, "message": message, "code": null, "param": null}})
}

#[cfg(test)]
mod tests;
