//! The Responses SSE surface every inbound-Codex translator emits.
//!
//! [`ResponsesEmitter`] owns the response envelope (`resp_…` id, model,
//! `created_at`, the accumulated `output` array) and the event grammar the
//! Codex CLI parses, so the upstream-specific machines
//! ([`super::messages_stream`], [`super::chat_stream`]) only have to decide
//! *when* an item opens, takes a delta, or closes. It is deliberately
//! protocol-neutral: nothing here knows what an Anthropic or a Chat
//! Completions event looks like.
//!
//! Every method returns already-serialised SSE frames — a `Vec<String>` when a
//! step justifies several, a `String` for the one-frame deltas — so a caller
//! can write them to the wire the moment the upstream event that justified
//! them arrived. Nothing is buffered until the turn ends.
//!
//! Two invariants the machines rely on:
//!
//! - Every emitted event carries a `sequence_number` that increases by one
//!   across the whole response, and a `type` equal to its SSE event name.
//! - Misuse is inert, not a panic: closing an `output_index` that is not open
//!   (or is open as a different kind) emits nothing, and a terminal event
//!   after a terminal event returns an empty frame. A translator hitting a
//!   malformed upstream stream can therefore close defensively.

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

mod items;
pub use items::{function_call_item, item_id, message_item, reasoning_item, web_search_item};
use items::{summary_part, text_part};

/// Responses token usage, in the shape [`ResponsesEmitter`] reports it.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    /// The `usage` object of a Responses envelope. `total_tokens` sums the
    /// prompt and completion totals; the cached and reasoning counts are
    /// subsets reported in the details objects, so they are not added again.
    /// A cached count is clamped to `input_tokens`, so an upstream that
    /// reports more cached than prompt tokens cannot break that invariant.
    pub fn to_value(&self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "input_tokens_details": {
                "cached_tokens": self.cached_input_tokens.min(self.input_tokens)
            },
            "output_tokens": self.output_tokens,
            "output_tokens_details": {"reasoning_tokens": self.reasoning_tokens},
            "total_tokens": self.input_tokens.saturating_add(self.output_tokens),
        })
    }
}

/// Which kind of item an `output_index` is currently open as, plus whatever
/// text has accumulated for it (message text, reasoning summary, or the
/// function call's argument JSON).
#[derive(Debug, Clone)]
enum OpenItem {
    Message {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
        summary: String,
        saw_delta: bool,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Clone)]
pub struct ResponsesEmitter {
    id: String,
    model: String,
    created_at: u64,
    seq: u64,
    /// One slot per allocated `output_index`, filled when that item closes.
    /// Reserving the slot at open time keeps `response.completed`'s `output`
    /// in the order items were opened even if they close out of order.
    output: Vec<Option<Value>>,
    open: HashMap<usize, OpenItem>,
    /// Set by the first terminal event so a later one is suppressed.
    terminal: bool,
}

impl ResponsesEmitter {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            id: format!("resp_{}", Uuid::new_v4().simple()),
            model: model.into(),
            created_at: unix_now(),
            seq: 0,
            output: Vec::new(),
            open: HashMap::new(),
            terminal: false,
        }
    }

    /// The response id this turn reports, e.g. for logging.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// `response.created` + `response.in_progress`, the two frames the Codex
    /// CLI waits for before it renders anything.
    pub fn created(&mut self) -> Vec<String> {
        let response = self.response_object("in_progress", None);
        vec![
            self.emit("response.created", json!({"response": response.clone()})),
            self.emit("response.in_progress", json!({"response": response})),
        ]
    }

    /// Open an assistant `message` item with a single `output_text` part.
    pub fn open_message(&mut self) -> (usize, Vec<String>) {
        let id = item_id("msg_");
        let output_index = self.allocate(OpenItem::Message {
            id: id.clone(),
            text: String::new(),
        });
        let item = message_item(&id, "", "in_progress");
        let frames = vec![
            self.emit(
                "response.output_item.added",
                json!({"output_index": output_index, "item": item}),
            ),
            self.emit(
                "response.content_part.added",
                json!({
                    "item_id": id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": text_part(""),
                }),
            ),
        ];
        (output_index, frames)
    }

    pub fn text_delta(&mut self, output_index: usize, delta: &str) -> String {
        let Some(OpenItem::Message { id, text }) = self.open.get_mut(&output_index) else {
            return String::new();
        };
        text.push_str(delta);
        let id = id.clone();
        self.emit(
            "response.output_text.delta",
            json!({
                "item_id": id,
                "output_index": output_index,
                "content_index": 0,
                "delta": delta,
            }),
        )
    }

    pub fn close_message(&mut self, output_index: usize) -> Vec<String> {
        let Some(OpenItem::Message { id, text }) = self.take_open(output_index, |item| {
            matches!(item, OpenItem::Message { .. })
        }) else {
            return Vec::new();
        };
        let item = message_item(&id, &text, "completed");
        self.output[output_index] = Some(item.clone());
        vec![
            self.emit(
                "response.output_text.done",
                json!({
                    "item_id": id,
                    "output_index": output_index,
                    "content_index": 0,
                    "text": text,
                }),
            ),
            self.emit(
                "response.content_part.done",
                json!({
                    "item_id": id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": text_part(&text),
                }),
            ),
            self.emit(
                "response.output_item.done",
                json!({"output_index": output_index, "item": item}),
            ),
        ]
    }

    /// Open a `reasoning` item with a single `summary_text` part.
    pub fn open_reasoning(&mut self) -> (usize, Vec<String>) {
        let id = item_id("rs_");
        let output_index = self.allocate(OpenItem::Reasoning {
            id: id.clone(),
            summary: String::new(),
            saw_delta: false,
        });
        let item = reasoning_item(&id, None, None);
        let frames = vec![
            self.emit(
                "response.output_item.added",
                json!({"output_index": output_index, "item": item}),
            ),
            self.emit(
                "response.reasoning_summary_part.added",
                json!({
                    "item_id": id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "part": summary_part(""),
                }),
            ),
        ];
        (output_index, frames)
    }

    pub fn reasoning_delta(&mut self, output_index: usize, delta: &str) -> String {
        let Some(OpenItem::Reasoning {
            id,
            summary,
            saw_delta,
        }) = self.open.get_mut(&output_index)
        else {
            return String::new();
        };
        summary.push_str(delta);
        *saw_delta = true;
        let id = id.clone();
        self.emit(
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": id,
                "output_index": output_index,
                "summary_index": 0,
                "delta": delta,
            }),
        )
    }

    /// Close a `reasoning` item, optionally stamping the opaque
    /// `encrypted_content` that round-trips the upstream's own thinking block
    /// (see [`super::reasoning`]).
    ///
    /// The summary part frames are always emitted so every
    /// `reasoning_summary_part.added` has a matching `done`, but the item's
    /// `summary` array stays empty when no delta ever arrived — a redacted
    /// thinking block carries its payload in `encrypted_content` alone and has
    /// no text to show.
    pub fn close_reasoning(
        &mut self,
        output_index: usize,
        encrypted_content: Option<String>,
    ) -> Vec<String> {
        let Some(OpenItem::Reasoning {
            id,
            summary,
            saw_delta,
        }) = self.take_open(output_index, |item| {
            matches!(item, OpenItem::Reasoning { .. })
        })
        else {
            return Vec::new();
        };
        let item = reasoning_item(
            &id,
            saw_delta.then_some(summary.as_str()),
            encrypted_content.as_deref(),
        );
        self.output[output_index] = Some(item.clone());
        vec![
            self.emit(
                "response.reasoning_summary_text.done",
                json!({
                    "item_id": id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "text": summary,
                }),
            ),
            self.emit(
                "response.reasoning_summary_part.done",
                json!({
                    "item_id": id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "part": summary_part(&summary),
                }),
            ),
            self.emit(
                "response.output_item.done",
                json!({"output_index": output_index, "item": item}),
            ),
        ]
    }

    /// Open a `function_call` item. `call_id` is the id the client must echo
    /// back on the tool result; `name` is the tool.
    pub fn open_function_call(&mut self, call_id: &str, name: &str) -> (usize, Vec<String>) {
        let id = item_id("fc_");
        let output_index = self.allocate(OpenItem::FunctionCall {
            id: id.clone(),
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: String::new(),
        });
        let item = function_call_item(&id, call_id, name, "", "in_progress");
        let frames = vec![self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        )];
        (output_index, frames)
    }

    pub fn arguments_delta(&mut self, output_index: usize, delta: &str) -> String {
        let Some(OpenItem::FunctionCall { id, arguments, .. }) = self.open.get_mut(&output_index)
        else {
            return String::new();
        };
        arguments.push_str(delta);
        let id = id.clone();
        self.emit(
            "response.function_call_arguments.delta",
            json!({"item_id": id, "output_index": output_index, "delta": delta}),
        )
    }

    pub fn close_function_call(&mut self, output_index: usize) -> Vec<String> {
        let Some(OpenItem::FunctionCall {
            id,
            call_id,
            name,
            arguments,
        }) = self.take_open(output_index, |item| {
            matches!(item, OpenItem::FunctionCall { .. })
        })
        else {
            return Vec::new();
        };
        // A tool the upstream called with no input streams no argument delta at
        // all; the Codex CLI parses `arguments` as JSON, so an empty string has
        // to become an empty object.
        let arguments = if arguments.is_empty() {
            "{}".to_string()
        } else {
            arguments
        };
        let item = function_call_item(&id, &call_id, &name, &arguments, "completed");
        self.output[output_index] = Some(item.clone());
        vec![
            self.emit(
                "response.function_call_arguments.done",
                json!({"item_id": id, "output_index": output_index, "arguments": arguments}),
            ),
            self.emit(
                "response.output_item.done",
                json!({"output_index": output_index, "item": item}),
            ),
        ]
    }

    /// Emit a complete `web_search_call` item in one shot. The upstreams that
    /// surface a hosted web search only report it once it has already run, so
    /// there is nothing to keep open between frames.
    pub fn web_search_call(&mut self, id: Option<&str>, query: Option<&str>) -> Vec<String> {
        let id = id.map_or_else(|| item_id("ws_"), str::to_string);
        let output_index = self.output.len();
        self.output.push(None);
        let done = web_search_item(&id, query, "completed");
        self.output[output_index] = Some(done.clone());
        let progress = |emitter: &mut Self, event: &str| {
            emitter.emit(event, json!({"item_id": id, "output_index": output_index}))
        };
        vec![
            self.emit(
                "response.output_item.added",
                json!({
                    "output_index": output_index,
                    "item": web_search_item(&id, query, "in_progress"),
                }),
            ),
            progress(self, "response.web_search_call.in_progress"),
            progress(self, "response.web_search_call.searching"),
            progress(self, "response.web_search_call.completed"),
            self.emit(
                "response.output_item.done",
                json!({"output_index": output_index, "item": done}),
            ),
        ]
    }

    /// Append an already-finished item to the response's `output`. The
    /// non-streaming translators build their whole response this way, reusing
    /// the same item constructors the streaming path emits.
    pub fn push_item(&mut self, item: Value) -> usize {
        let output_index = self.output.len();
        self.output.push(Some(item));
        output_index
    }

    pub fn completed(&mut self, usage: Usage) -> String {
        self.terminate("response.completed", "completed", Some(usage), |_| {})
    }

    pub fn incomplete(&mut self, reason: &str, usage: Usage) -> String {
        let reason = reason.to_string();
        self.terminate(
            "response.incomplete",
            "incomplete",
            Some(usage),
            |response| {
                response["incomplete_details"] = json!({"reason": reason});
            },
        )
    }

    pub fn failed(&mut self, code: &str, message: &str) -> String {
        let error = json!({"code": code, "message": message});
        self.terminate("response.failed", "failed", None, |response| {
            response["error"] = error;
        })
    }

    /// The response envelope itself, without any SSE framing — what the
    /// non-streaming translators return as their whole response body.
    pub fn response_object(&self, status: &str, usage: Option<&Usage>) -> Value {
        let mut response = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": self.output.iter().flatten().cloned().collect::<Vec<_>>(),
        });
        if let Some(usage) = usage {
            response["usage"] = usage.to_value();
        }
        response
    }

    /// Whether a terminal event has already been emitted.
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// The `output_index`es still open, lowest first — what a translator
    /// closes when the upstream ends mid-item.
    pub fn open_indexes(&self) -> Vec<usize> {
        let mut indexes: Vec<usize> = self.open.keys().copied().collect();
        indexes.sort_unstable();
        indexes
    }

    fn terminate(
        &mut self,
        event: &str,
        status: &str,
        usage: Option<Usage>,
        decorate: impl FnOnce(&mut Value),
    ) -> String {
        if self.terminal {
            return String::new();
        }
        self.terminal = true;
        let mut response = self.response_object(status, usage.as_ref());
        decorate(&mut response);
        self.emit(event, json!({"response": response}))
    }

    fn allocate(&mut self, item: OpenItem) -> usize {
        let output_index = self.output.len();
        self.output.push(None);
        self.open.insert(output_index, item);
        output_index
    }

    /// Remove the item open at `output_index` only when it is the kind the
    /// caller is closing. A mismatched close must leave the item open, not
    /// silently drop it — the emitter's misuse-is-inert contract.
    fn take_open(
        &mut self,
        output_index: usize,
        wanted: fn(&OpenItem) -> bool,
    ) -> Option<OpenItem> {
        self.open
            .get(&output_index)
            .is_some_and(wanted)
            .then(|| self.open.remove(&output_index))
            .flatten()
    }

    /// Stamp the event `type` and the response-wide `sequence_number` onto a
    /// payload and frame it. The single place `seq` advances.
    fn emit(&mut self, name: &str, mut data: Value) -> String {
        if let Some(object) = data.as_object_mut() {
            object.insert("type".to_string(), json!(name));
            object.insert("sequence_number".to_string(), json!(self.seq));
        }
        self.seq += 1;
        sse(name, &data)
    }
}

/// Frame one Responses event, matching `crate::model::responses`'s framing.
pub fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
