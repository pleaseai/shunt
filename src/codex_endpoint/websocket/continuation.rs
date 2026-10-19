//! Per-connection `previous_response_id` continuation for inbound Responses
//! WebSocket sessions.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::{
    adapters::responses::codex_continuation::{
        build_transcript, extends_transcript, StoredContinuation,
    },
    codex_endpoint::frame::{terminal_status_from_type, TerminalStatus},
    config::Config,
};

/// Per-connection continuation captured from the last completed turn, so an
/// incremental `response.create` frame (`previous_response_id` + input delta)
/// can be expanded to the full input the HTTP upstream requires: the ChatGPT
/// HTTP backend rejects `previous_response_id` outright
/// (`Unsupported parameter: previous_response_id`), unlike the WebSocket
/// backend the client is speaking. Lives only for one socket, like the
/// client's own continuation state.
pub(super) type SharedContinuation = Arc<Mutex<Option<ConnectionContinuation>>>;

/// The last completed turn's continuation and the provider that issued its
/// response id: only that provider can resolve the id, so a turn routed
/// elsewhere must not be handed it.
#[derive(Clone, Debug)]
pub(super) struct ConnectionContinuation {
    pub(super) stored: StoredContinuation,
    pub(super) provider: String,
}

/// The provider a turn is served by, and whether it is a ChatGPT backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TurnTarget {
    pub(super) provider: String,
    pub(super) chatgpt: bool,
}

/// Resolve a turn's target the way `codex_endpoint::forward_turn` routes it: a
/// model with no `[[server.codex_endpoint.routes]]` entry (or a frame with no
/// readable model) goes to the endpoint's own ChatGPT/Codex pool; a routed one
/// goes to its route's provider.
pub(super) fn resolve_target(config: &Config, model: Option<&str>) -> TurnTarget {
    let endpoint = config.server.codex_endpoint.as_ref();
    let route = endpoint
        .zip(model)
        .and_then(|(endpoint, model)| endpoint.route_for(model));
    match route {
        Some(route) => TurnTarget {
            provider: route.provider.clone(),
            chatgpt: config.is_chatgpt_backend(&route.provider),
        },
        None => TurnTarget {
            provider: endpoint
                .map(|endpoint| endpoint.provider.clone())
                .unwrap_or_default(),
            chatgpt: true,
        },
    }
}

/// Prepare a `response.create` body for the ChatGPT HTTP upstream: drop
/// `previous_response_id` (rejected there) and, when it names this
/// connection's stored response and the input is only a delta, expand the
/// input to the full transcript. An input that already starts with the stored
/// transcript (the client sent the full history with the id as a hint)
/// forwards unchanged apart from the dropped field, whatever else changed.
/// An id that names no stored response is dropped and the input forwarded as
/// sent: without a transcript a delta cannot be told from a full history, so
/// a delta-only frame then loses earlier context (e.g. after a reconnect).
/// Returns the full logical input the prepared body carries.
pub(super) fn prepare_body(raw_json: &mut Value, continuation: &SharedContinuation) -> Vec<Value> {
    let Some(obj) = raw_json.as_object_mut() else {
        return Vec::new();
    };
    let previous = obj
        .remove("previous_response_id")
        .and_then(|value| value.as_str().map(ToOwned::to_owned));
    let input = input_items(raw_json);
    let expanded = previous
        .and_then(|previous| stored_for(continuation, &previous))
        .and_then(|stored| expand(&stored.stored, &input));
    match expanded {
        Some(full) => replace_input(raw_json, full),
        None => input,
    }
}

/// Prepare a body for a routed Responses API, which honours
/// `previous_response_id` itself. An id this provider issued (it served the
/// stored response) or one shunt does not know forwards unchanged, the
/// provider deciding; only the recorded logical input is expanded, so a later
/// turn sent to a ChatGPT backend still carries the whole history. An id
/// another backend issued is unresolvable here, so it is dropped and a delta
/// expanded into the body, as for a ChatGPT target. Returns the full logical
/// input.
pub(super) fn prepare_routed_body(
    raw_json: &mut Value,
    continuation: &SharedContinuation,
    provider: &str,
) -> Vec<Value> {
    let input = input_items(raw_json);
    let stored = raw_json
        .get("previous_response_id")
        .and_then(Value::as_str)
        .and_then(|previous| stored_for(continuation, previous));
    let Some(stored) = stored else {
        return input;
    };
    let expanded = expand(&stored.stored, &input);
    if stored.provider == provider {
        return expanded.unwrap_or(input);
    }
    if let Some(obj) = raw_json.as_object_mut() {
        obj.remove("previous_response_id");
    }
    match expanded {
        Some(full) => replace_input(raw_json, full),
        None => input,
    }
}

/// The connection's stored continuation, when `previous` names its response.
fn stored_for(continuation: &SharedContinuation, previous: &str) -> Option<ConnectionContinuation> {
    let guard = continuation.lock().ok()?;
    guard
        .as_ref()
        .filter(|stored| stored.stored.response_id == previous)
        .cloned()
}

/// The stored transcript followed by `input`, when `input` is a delta after it
/// rather than the full history.
fn expand(stored: &StoredContinuation, input: &[Value]) -> Option<Vec<Value>> {
    if extends_transcript(&stored.transcript, input) {
        return None;
    }
    Some(build_transcript(&stored.transcript, input))
}

/// Write `full` as the body's `input` and return it.
fn replace_input(raw_json: &mut Value, full: Vec<Value>) -> Vec<Value> {
    if let Some(obj) = raw_json.as_object_mut() {
        obj.insert("input".to_string(), Value::Array(full.clone()));
    }
    full
}

/// A body's `input` as items. The Responses API also accepts a plain string,
/// which is one user message; normalizing it keeps that prompt in the
/// transcript a later delta is expanded against.
fn input_items(raw_json: &Value) -> Vec<Value> {
    match raw_json.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        })],
        _ => Vec::new(),
    }
}

/// Records one turn's upstream events against the connection continuation.
/// Like `codex_ws::capture_continuation`, the response id is captured from
/// response-level events (nested `response.id`, or a top-level `id` on e.g.
/// `response.done`) and output items from `response.output_item.done`, since
/// the terminal event alone may carry neither.
pub(super) struct TurnRecorder {
    continuation: SharedContinuation,
    /// The provider serving the turn, which issues its response id.
    provider: String,
    /// The full logical input the turn's body carries after expansion, and
    /// the signature of its non-input fields.
    logical_input: Vec<Value>,
    body_signature: String,
    response_id: Option<String>,
    output_items: Vec<Value>,
}

impl TurnRecorder {
    pub(super) fn new(
        continuation: SharedContinuation,
        provider: String,
        logical_input: Vec<Value>,
        body_signature: String,
    ) -> Self {
        Self {
            continuation,
            provider,
            logical_input,
            body_signature,
            response_id: None,
            output_items: Vec::new(),
        }
    }

    /// Observe one upstream event `payload` of type `event_type`, recording
    /// the continuation when it is terminal.
    pub(super) fn observe(&mut self, event_type: &str, payload: &str) {
        let response_level = matches!(
            event_type,
            "response.created" | "response.in_progress" | "response.completed" | "response.done"
        );
        let item_done = event_type == "response.output_item.done";
        let terminal = terminal_status_from_type(event_type);
        if !response_level && !item_done && terminal.is_none() {
            return;
        }
        let value = serde_json::from_str::<Value>(payload).ok();
        if let Some(value) = &value {
            if response_level {
                if let Some(id) = value
                    .pointer("/response/id")
                    .or_else(|| value.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    self.response_id = Some(id.to_string());
                }
            }
            if item_done {
                if let Some(item) = value.get("item") {
                    self.output_items.push(item.clone());
                }
            }
        }
        if let Some(terminal) = terminal {
            self.record(terminal, value.as_ref());
        }
    }

    /// A completed response becomes the prefix the next incremental frame
    /// extends; a failed or incomplete one (or one without an id) clears it,
    /// since the backend state the client would name no longer matches
    /// anything shunt can expand from.
    fn record(&self, terminal: TerminalStatus, payload: Option<&Value>) {
        let Ok(mut guard) = self.continuation.lock() else {
            return;
        };
        let id = self
            .response_id
            .as_ref()
            .filter(|_| terminal == TerminalStatus::Completed);
        let Some(id) = id else {
            *guard = None;
            return;
        };
        let output = payload
            .and_then(|payload| payload.pointer("/response/output"))
            .and_then(Value::as_array)
            .filter(|output| !output.is_empty())
            .unwrap_or(&self.output_items);
        *guard = Some(ConnectionContinuation {
            stored: StoredContinuation {
                response_id: id.clone(),
                signature: self.body_signature.clone(),
                transcript: build_transcript(&self.logical_input, output),
                turn_state: None,
            },
            provider: self.provider.clone(),
        });
    }
}
