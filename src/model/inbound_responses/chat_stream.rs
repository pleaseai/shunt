//! OpenAI Chat Completions → OpenAI Responses.
//!
//! [`ChatSseMachine`] is the Chat Completions counterpart of
//! [`super::messages_stream::MessagesSseMachine`]: it takes the
//! `chat.completion.chunk` payloads an OpenAI-compatible upstream streams and
//! re-emits the turn as the Responses events the inbound Codex endpoint's
//! client expects. One upstream chunk in, the Responses frames it justifies
//! out — nothing waits for the turn to end.
//!
//! All Responses-side shape lives in [`super::events`]; this module only maps
//! Chat Completions' delta grammar onto it. That grammar carries far less
//! structure than Anthropic's: there are no block boundaries, so an item opens
//! on the first delta of its kind and closes when a delta of another kind
//! arrives or the stream terminates. Tool calls are keyed by their own
//! `index`, which the machine maps to the `output_index` the emitter
//! allocated.

use std::collections::{BTreeSet, HashMap};

use serde_json::{json, Value};

use super::events::{
    function_call_item, item_id, message_item, reasoning_item, ResponsesEmitter, Usage,
};

/// The end-of-stream sentinel Chat Completions sends as its last `data:` line.
const DONE: &str = "[DONE]";

/// The `code` on the `response.failed` a stream that stopped before `[DONE]`
/// produces.
const TRUNCATED_CODE: &str = "upstream_stream_truncated";

/// The `code` on the `response.failed` a stream that carried a chunk the
/// machine could not parse produces, however it then terminated.
const MALFORMED_CODE: &str = "upstream_stream_malformed";

/// The `message` that failure reports. Fixed, because the payload that failed
/// to parse is client-visible content and never leaves this module.
const MALFORMED_MESSAGE: &str = "the upstream stream contained an unreadable chunk";

/// The `code` on the `response.failed` an in-stream error that named neither a
/// `code` nor a `type` produces.
const UPSTREAM_ERROR_CODE: &str = "upstream_error";

/// The `message` a failure reports when the upstream named none. A client
/// reads `message` as a string, so both the in-stream and the error-envelope
/// path fall back to it.
const UPSTREAM_ERROR_MESSAGE: &str = "the upstream reported an error";

#[derive(Debug, Clone)]
pub struct ChatSseMachine {
    emitter: ResponsesEmitter,
    created: bool,
    /// The `output_index` of the assistant message currently open, if any. A
    /// tool call closes it, so text arriving afterwards opens a new one.
    message: Option<usize>,
    reasoning: Option<usize>,
    /// Tool-call `index` → the `output_index` its function call was opened at,
    /// so a later chunk carrying only `function.arguments` for that index still
    /// reaches the right item.
    calls: HashMap<usize, usize>,
    /// The `output_index`es of the function calls still open. Parallel calls
    /// stay open alongside each other: only a text or reasoning delta, or the
    /// end of the turn, closes them.
    open_calls: BTreeSet<usize>,
    usage: Usage,
    finish_reason: Option<String>,
    /// Whether a `data:` payload failed to parse. A turn that lost a chunk
    /// cannot be reported as complete: the client would take a truncated
    /// answer for the whole one.
    damaged: bool,
}

impl ChatSseMachine {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            emitter: ResponsesEmitter::new(model),
            created: false,
            message: None,
            reasoning: None,
            calls: HashMap::new(),
            open_calls: BTreeSet::new(),
            usage: Usage::default(),
            finish_reason: None,
            damaged: false,
        }
    }

    /// Whether a terminal Responses event has already been emitted. A caller
    /// that sees this can stop relaying without calling [`Self::finish`].
    pub fn is_terminal(&self) -> bool {
        self.emitter.is_terminal()
    }

    /// One SSE `data:` payload, already stripped of its `data: ` prefix.
    /// `[DONE]` is the end-of-stream sentinel and ends the turn.
    pub fn apply(&mut self, data: &str) -> Vec<String> {
        if self.is_terminal() {
            return Vec::new();
        }
        let data = data.trim();
        if data == DONE {
            return self.complete();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            // Upstream chunks reach logs and Sentry, so the payload that failed
            // to parse is described, never quoted.
            tracing::debug!("an unparsable chat completions chunk damaged the stream");
            self.damaged = true;
            return Vec::new();
        };
        let mut frames = self.created();

        // Some OpenAI-compatible backends report a mid-stream failure as an
        // error object on a 200 OK body. Surface it and stop: the turn is over
        // either way.
        if let Some(error) = chunk.get("error").filter(|error| error.is_object()) {
            frames.extend(self.fail(error));
            return frames;
        }

        // `stream_options.include_usage` puts usage on a final chunk that
        // carries no choices, but a backend is free to attach it anywhere.
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = read_usage(usage);
        }

        let Some(choice) = chunk.pointer("/choices/0") else {
            return frames;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(text) = reasoning_text(delta) {
            frames.extend(self.reasoning_delta(text));
        }
        if let Some(text) = text_field(delta, "content") {
            frames.extend(self.text_delta(text));
        }
        for entry in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            frames.extend(self.tool_call(entry));
        }
        // The terminal event waits for `[DONE]`: with usage requested, the
        // chunk carrying `finish_reason` is not the last one.
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        frames
    }

    /// The upstream stream ended without `[DONE]`. Close whatever is open and
    /// fail the response, so the client sees a terminal event rather than a
    /// silently truncated turn.
    pub fn finish(&mut self) -> Vec<String> {
        if self.is_terminal() {
            return Vec::new();
        }
        let mut frames = self.created();
        frames.extend(self.close_open_items());
        frames.push(if self.damaged {
            self.emitter.failed(MALFORMED_CODE, MALFORMED_MESSAGE)
        } else {
            self.emitter.failed(
                TRUNCATED_CODE,
                "the upstream stream ended before the completion finished",
            )
        });
        frames
    }

    /// `response.created` + `response.in_progress`, emitted once, on the first
    /// payload the upstream sent.
    fn created(&mut self) -> Vec<String> {
        if std::mem::replace(&mut self.created, true) {
            return Vec::new();
        }
        self.emitter.created()
    }

    fn complete(&mut self) -> Vec<String> {
        let mut frames = self.created();
        frames.extend(self.close_open_items());
        // `[DONE]` says the upstream finished sending, not that what arrived
        // was whole: a chunk shunt could not read makes this turn a failure.
        if self.damaged {
            frames.push(self.emitter.failed(MALFORMED_CODE, MALFORMED_MESSAGE));
            return frames;
        }
        let usage = self.usage.clone();
        frames.push(match incomplete_reason(self.finish_reason.as_deref()) {
            Some(reason) => self.emitter.incomplete(reason, usage),
            None => self.emitter.completed(usage),
        });
        frames
    }

    fn fail(&mut self, error: &Value) -> Vec<String> {
        let mut frames = self.close_open_items();
        let code = code_field(error.get("code"))
            .or_else(|| {
                error
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| UPSTREAM_ERROR_CODE.to_string());
        let message = error_message(error);
        frames.push(self.emitter.failed(&code, message));
        frames
    }

    fn reasoning_delta(&mut self, text: &str) -> Vec<String> {
        // Text of any kind ends the tool calls that preceded it: Chat
        // Completions has no block boundaries, so a delta of another kind is
        // the only signal their arguments are complete.
        let mut frames = self.close_calls();
        let output_index = match self.reasoning {
            Some(output_index) => output_index,
            None => {
                let (output_index, opened) = self.emitter.open_reasoning();
                frames.extend(opened);
                self.reasoning = Some(output_index);
                output_index
            }
        };
        frames.push(self.emitter.reasoning_delta(output_index, text));
        frames
    }

    fn text_delta(&mut self, text: &str) -> Vec<String> {
        // Reasoning runs ahead of the answer, so the first visible token ends
        // it. Chat Completions never signs its reasoning, so nothing
        // round-trips as `encrypted_content`.
        let mut frames = self.close_calls();
        frames.extend(self.close_reasoning());
        let output_index = match self.message {
            Some(output_index) => output_index,
            None => {
                let (output_index, opened) = self.emitter.open_message();
                frames.extend(opened);
                self.message = Some(output_index);
                output_index
            }
        };
        frames.push(self.emitter.text_delta(output_index, text));
        frames
    }

    fn tool_call(&mut self, entry: &Value) -> Vec<String> {
        let index = entry
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .try_into()
            .unwrap_or(usize::MAX);
        let call_id = entry.get("id").and_then(Value::as_str);
        let name = entry.pointer("/function/name").and_then(Value::as_str);
        let mut frames = Vec::new();
        // An entry naming a tool opens a call, unless this index already has
        // one: some backends repeat the name on every fragment.
        if (call_id.is_some() || name.is_some()) && !self.calls.contains_key(&index) {
            frames.extend(self.close_reasoning());
            frames.extend(self.close_message());
            let (output_index, opened) = self
                .emitter
                .open_function_call(call_id.unwrap_or_default(), name.unwrap_or_default());
            frames.extend(opened);
            self.calls.insert(index, output_index);
            self.open_calls.insert(output_index);
        }
        if let Some(arguments) = text_field(entry.get("function").unwrap_or(entry), "arguments") {
            if let Some(&output_index) = self.calls.get(&index) {
                // Inert when that call has already closed, which is what the
                // empty frame the emitter returns means.
                let frame = self.emitter.arguments_delta(output_index, arguments);
                if !frame.is_empty() {
                    frames.push(frame);
                }
            }
        }
        frames
    }

    fn close_reasoning(&mut self) -> Vec<String> {
        self.reasoning
            .take()
            .map(|output_index| self.emitter.close_reasoning(output_index, None))
            .unwrap_or_default()
    }

    fn close_message(&mut self) -> Vec<String> {
        self.message
            .take()
            .map(|output_index| self.emitter.close_message(output_index))
            .unwrap_or_default()
    }

    /// Close every function call still open, lowest `output_index` first.
    fn close_calls(&mut self) -> Vec<String> {
        std::mem::take(&mut self.open_calls)
            .into_iter()
            .flat_map(|output_index| self.emitter.close_function_call(output_index))
            .collect()
    }

    /// Close every item still open, lowest `output_index` first, so a
    /// truncated or failed turn still reports well-formed items.
    fn close_open_items(&mut self) -> Vec<String> {
        self.emitter
            .open_indexes()
            .into_iter()
            .flat_map(|output_index| {
                if self.reasoning == Some(output_index) {
                    self.close_reasoning()
                } else if self.message == Some(output_index) {
                    self.close_message()
                } else if self.open_calls.remove(&output_index) {
                    self.emitter.close_function_call(output_index)
                } else {
                    Vec::new()
                }
            })
            .collect()
    }
}

/// A non-streaming `chat.completion` → the Responses response body. Built from
/// the same item constructors the streaming path emits, so the two paths cannot
/// drift.
pub fn translate_response(completion: &Value, model: &str) -> Value {
    let mut emitter = ResponsesEmitter::new(model);
    let choice = completion.pointer("/choices/0");
    if let Some(message) = choice.and_then(|choice| choice.get("message")) {
        if let Some(text) = reasoning_text(message) {
            emitter.push_item(reasoning_item(&item_id("rs_"), Some(text), None));
        }
        if let Some(text) = text_field(message, "content") {
            emitter.push_item(message_item(&item_id("msg_"), text, "completed"));
        }
        for entry in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = |pointer: &str| entry.pointer(pointer).and_then(Value::as_str);
            // The Codex CLI parses `arguments` as JSON, so a tool called with
            // no input has to become an empty object.
            let arguments = field("/function/arguments").filter(|arguments| !arguments.is_empty());
            emitter.push_item(function_call_item(
                &item_id("fc_"),
                field("/id").unwrap_or_default(),
                field("/function/name").unwrap_or_default(),
                arguments.unwrap_or("{}"),
                "completed",
            ));
        }
    }

    let usage = completion.get("usage").map(read_usage).unwrap_or_default();
    let reason = incomplete_reason(
        choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str),
    );
    let status = if reason.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    let mut response = emitter.response_object(status, Some(&usage));
    if let Some(reason) = reason {
        response["incomplete_details"] = json!({"reason": reason});
    }
    response
}

/// A Chat Completions error envelope → the OpenAI error envelope the inbound
/// Codex endpoint's clients parse. Chat Completions already reports errors in
/// that envelope, so a body of that shape passes through with the four keys a
/// client reads filled in; anything else is reported as a generic `api_error`
/// rather than echoed back — an unrecognised body may be an intermediary's
/// HTML or a provider payload the client should not be handed.
pub fn translate_error(chat_error: &Value) -> Value {
    let source = chat_error.get("error").unwrap_or(&Value::Null);
    let Some(error) = source.as_object() else {
        return generic_error();
    };
    let mut error = error.clone();
    // A client reads `message` as a string, so a body that omitted it — or
    // wrote something else there — gets the same fallback `fail()` uses.
    error.insert("message".to_string(), json!(error_message(source)));
    // The same holds for `type`: a missing, non-string, or empty one becomes
    // the generic `api_error` rather than reaching the client as-is.
    if !error
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| !kind.is_empty())
    {
        error.insert("type".to_string(), json!("api_error"));
    }
    for key in ["code", "param"] {
        error.entry(key).or_insert(Value::Null);
    }
    json!({"error": error})
}

/// The Responses `incomplete_details.reason` a Chat Completions finish reason
/// justifies. `length` and `content_filter` are the two that are not a clean
/// end of turn; every other reason completes the response.
fn incomplete_reason(finish_reason: Option<&str>) -> Option<&'static str> {
    match finish_reason? {
        "length" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => None,
    }
}

/// The `message` of an error object, or the fallback when it named none as a
/// string.
fn error_message(error: &Value) -> &str {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(UPSTREAM_ERROR_MESSAGE)
}

fn generic_error() -> Value {
    json!({"error": {
        "type": "api_error",
        "message": "upstream chat completions error",
        "code": null,
        "param": null,
    }})
}

/// Chat Completions reports usage as flat prompt/completion totals with the
/// cached and reasoning portions in details objects — the same split Responses
/// uses, under different names.
fn read_usage(usage: &Value) -> Usage {
    let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: count("/prompt_tokens"),
        cached_input_tokens: count("/prompt_tokens_details/cached_tokens"),
        output_tokens: count("/completion_tokens"),
        reasoning_tokens: count("/completion_tokens_details/reasoning_tokens"),
    }
}

/// The reasoning text of a delta or a message. `reasoning_content` is
/// DeepSeek's and vLLM's spelling, `reasoning` is OpenRouter's; a backend that
/// reports neither as a string has no reasoning to show.
fn reasoning_text(source: &Value) -> Option<&str> {
    text_field(source, "reasoning_content").or_else(|| text_field(source, "reasoning"))
}

/// A non-empty string field. An empty one carries nothing to translate and
/// would open an item with no content.
fn text_field<'a>(source: &'a Value, key: &str) -> Option<&'a str> {
    source
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// An error `code`, which backends report as either a string or a number.
fn code_field(code: Option<&Value>) -> Option<String> {
    match code? {
        Value::String(code) if !code.is_empty() => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
