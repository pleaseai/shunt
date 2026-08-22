//! Translation from the Antigravity CLI's `--output-format stream-json` event
//! stream into Anthropic Messages SSE.
//!
//! `agy` runs its own agent loop, so a single Messages turn maps to many CLI
//! events. The shapes we consume (verified against `agy` output) are:
//!
//! - `{"event":"init","init":{...}}` — session opened.
//! - `{"event":"step_update","step_update":{"step_type":"tool",...}}` — a tool
//!   call started or finished. These carry no assistant text; they are the
//!   agent working. They can be many seconds apart, which is why they become
//!   SSE `ping` frames rather than being dropped: a silent connection is what
//!   makes a client's no-progress watchdog give up on the turn.
//! - `{"event":"step_update","step_update":{"step_type":"agent_response",
//!   "text_delta":"...","usage":{...}}}` — assistant text and token counts.
//! - `{"event":"result","result":{"status":"SUCCESS","response":"...",
//!   "usage":{...}}}` — terminal event with the full reply and final usage.

use serde_json::{json, Value};

/// Token counts reported by `agy`, mapped onto the Anthropic usage shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgyUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

impl AgyUsage {
    fn read_from(&mut self, usage: &Value) {
        let field = |key: &str| usage.get(key).and_then(Value::as_u64);
        // Every event reports cumulative totals for the run, so a later event
        // supersedes an earlier one; missing fields keep the previous value.
        if let Some(v) = field("input_tokens") {
            self.input_tokens = v;
        }
        if let Some(v) = field("output_tokens") {
            self.output_tokens = v;
        }
        if let Some(v) = field("cache_read_tokens") {
            self.cache_read_tokens = v;
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_input_tokens": self.cache_read_tokens,
        })
    }
}

/// Terminal state of an `agy` run, extracted from its `result` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgyEnd {
    Success,
    Failed(String),
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Does this `agy` error report a handoff to a recipient print mode never had?
///
/// The CLI's tool surface includes `send_message`, and a subagent-style prompt
/// ("report your findings back to the main agent") is enough for the model to
/// use it to deliver its reply. Print mode registers no inbox, so the call
/// fails and `agy` marks the whole run `ERROR` — after the answer has already
/// been streamed in full. Shunt's caller then sees a complete response closed
/// by an SSE `error`, which reads as a truncated turn.
///
/// Matched by shape rather than by name: `main`, `team-lead` and `user` have
/// all been observed, sometimes behind a `CORTEX_STEP_TYPE_GENERIC:` wrapper.
/// Deliberately narrow — a run that failed for any other reason after emitting
/// some text really is partial, and must keep failing. Keyed on an upstream
/// error string, so it should be dropped once `agy` offers a way to withhold
/// the tool at spawn time.
fn is_undelivered_handoff(message: &str) -> bool {
    let Some((_, rest)) = message.split_once("recipient \"") else {
        return false;
    };
    rest.split_once('"')
        .is_some_and(|(_, tail)| tail.trim_start().starts_with("not found"))
}

/// Incremental `agy` stream-json → Anthropic SSE translator.
///
/// Feed it one JSON line at a time with [`Translator::on_line`] and flush with
/// [`Translator::finish`]. It is deliberately tolerant: unknown events and
/// unparseable lines are ignored rather than failing the turn, because the CLI
/// is free to add event types we do not model.
#[derive(Debug)]
pub struct Translator {
    model: String,
    message_id: String,
    started: bool,
    block_open: bool,
    text: String,
    usage: AgyUsage,
    end: Option<AgyEnd>,
}

impl Translator {
    pub fn new(model: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            message_id: message_id.into(),
            started: false,
            block_open: false,
            text: String::new(),
            usage: AgyUsage::default(),
            end: None,
        }
    }

    /// Assistant text seen so far (the `agy` reply).
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn usage(&self) -> AgyUsage {
        self.usage
    }

    /// `None` until the CLI emits its terminal `result` event.
    pub fn end(&self) -> Option<&AgyEnd> {
        self.end.as_ref()
    }

    fn message_start(&mut self) -> String {
        self.started = true;
        sse(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": self.usage.to_json(),
                }
            }),
        )
    }

    fn push_text(&mut self, delta: &str) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.message_start());
        }
        if !self.block_open {
            self.block_open = true;
            out.push_str(&sse(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" }
                }),
            ));
        }
        self.text.push_str(delta);
        out.push_str(&sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": delta }
            }),
        ));
        out
    }

    /// Append assistant text that did not come from the CLI's own stream, such
    /// as a terminal error the caller must see in the message body because the
    /// SSE headers have already been sent.
    pub fn on_text(&mut self, text: &str) -> String {
        self.push_text(text)
    }

    /// Translate one line of `agy` stream-json output into SSE bytes.
    ///
    /// Returns an empty string when the line carries no client-visible change.
    pub fn on_line(&mut self, line: &str) -> String {
        let line = line.trim();
        if line.is_empty() {
            return String::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return String::new();
        };
        let kind = event
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match kind {
            "init" => {
                if self.started {
                    String::new()
                } else {
                    self.message_start()
                }
            }
            "step_update" => {
                let step = event.get("step_update").unwrap_or(&Value::Null);
                if let Some(usage) = step.get("usage") {
                    self.usage.read_from(usage);
                }
                if let Some(delta) = step.get("text_delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        return self.push_text(delta);
                    }
                }
                // Tool traffic carries no assistant text. Emit a heartbeat so
                // the connection keeps producing bytes while the agent works.
                if step.get("step_type").and_then(Value::as_str) == Some("tool") {
                    let mut out = String::new();
                    if !self.started {
                        out.push_str(&self.message_start());
                    }
                    out.push_str(&sse("ping", &json!({ "type": "ping" })));
                    return out;
                }
                String::new()
            }
            "result" => {
                let result = event.get("result").unwrap_or(&Value::Null);
                if let Some(usage) = result.get("usage") {
                    self.usage.read_from(usage);
                }
                let status = result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("SUCCESS");
                if status != "SUCCESS" {
                    let message = result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("agy reported a failed run")
                        .to_string();
                    // An undelivered handoff is bookkeeping that failed after
                    // the reply itself was streamed, so failing the turn would
                    // report a complete answer as a server error. Text is
                    // required: with nothing streamed the reply only ever
                    // existed inside the message that could not be delivered,
                    // and the run genuinely produced nothing.
                    if self.text.is_empty() || !is_undelivered_handoff(&message) {
                        self.end = Some(AgyEnd::Failed(message));
                        return String::new();
                    }
                }
                self.end = Some(AgyEnd::Success);
                // `result.response` repeats text already delivered through
                // `text_delta`. Only fall back to it when the run streamed no
                // deltas at all, otherwise the reply would be duplicated.
                let mut out = String::new();
                if self.text.is_empty() {
                    if let Some(response) = result.get("response").and_then(Value::as_str) {
                        if !response.is_empty() {
                            out.push_str(&self.push_text(response));
                        }
                    }
                }
                out
            }
            _ => String::new(),
        }
    }

    /// Close the SSE message after a failed run.
    ///
    /// The headers are long committed by the time a mid-stream failure is
    /// known, so the failure has to be reported inside the stream. Anthropic's
    /// SSE protocol carries an `error` event for exactly this; emitting a plain
    /// `message_stop` instead would present a crash as a normal end of turn.
    pub fn finish_with_error(&mut self, detail: Option<&str>) -> String {
        let mut out = String::new();
        if self.block_open {
            self.block_open = false;
            out.push_str(&sse(
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": 0 }),
            ));
        }
        let message = detail
            .map(str::to_owned)
            .unwrap_or_else(|| self.error_message());
        out.push_str(&sse(
            "error",
            &json!({
                "type": "error",
                "error": { "type": "api_error", "message": message },
            }),
        ));
        out.push_str(&sse("message_stop", &json!({ "type": "message_stop" })));
        out
    }

    fn error_message(&self) -> String {
        match &self.end {
            Some(AgyEnd::Failed(message)) => message.clone(),
            _ => "the Antigravity CLI stopped without reporting a result".to_string(),
        }
    }

    /// Close the SSE message. Safe to call regardless of how the run ended.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.message_start());
        }
        if self.block_open {
            self.block_open = false;
            out.push_str(&sse(
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": 0 }),
            ));
        }
        out.push_str(&sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": self.usage.to_json(),
            }),
        ));
        out.push_str(&sse("message_stop", &json!({ "type": "message_stop" })));
        out
    }

    /// Non-streaming counterpart: the finished Anthropic Messages response.
    pub fn to_message(&self) -> Value {
        json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": self.text }],
            "model": self.model,
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": self.usage.to_json(),
        })
    }
}
