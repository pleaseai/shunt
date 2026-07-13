//! `previous_response_id` continuation logic for the Codex WebSocket transport
//! (issue #32, PR3).
//!
//! Real Codex trims per-turn upload by reusing a live connection's
//! `previous_response_id` and sending only the new input delta; the backend holds
//! the prior context server-side (even under `store:false`). This module is the
//! pure decision layer: given the continuation state captured from a connection's
//! previous turn and the current translated request, it decides whether the
//! current input is a safe append-only extension and, if so, returns the delta.
//!
//! ## Why normalization
//!
//! shunt translates Anthropic ⇄ Responses, so the backend's assistant
//! `output_item` is not byte-identical to what [`translate_request`] reconstructs
//! when Claude Code echoes that turn back. [`normalize_item`] rewrites both sides
//! to a canonical shape before comparing, so an echoed turn matches the backend
//! item it came from. Any residual mismatch (an unforeseen shape, a reordered
//! prefix, a changed non-input field) falls back to sending the full input: never
//! wrong context, only a missed optimization.
//!
//! Normalization is type-aware, grounded in shunt's own reconstruction code
//! ([`crate::model::responses`] output side and
//! [`crate::model::responses_request`] input side) and cross-checked against the
//! authoritative Responses item schemas in `openai/codex` (`ResponseItem`, the
//! type codex itself round-trips as input under `store:false`) and
//! `openai/openai-python` (`ResponseReasoningItem`, `ResponseFunctionToolCall`):
//!
//! - **All items** shed the additive backend-only keys `id`/`phase`/`status`. Both
//!   schemas mark `status`/`id` as "populated when returned via API"; a live probe
//!   over the WebSocket transport (issue #45, 2026-07-13) captured real
//!   `message`/`reasoning`/`function_call` items and confirmed the reconstruction
//!   is otherwise a strict subset, with no unaccounted field.
//! - **`message`** content parts shed `annotations`/`logprobs` (probe-confirmed).
//! - **`function_call`** items parse their `arguments` JSON *string* into a value
//!   and drop `namespace`. The backend sends the model's raw argument string
//!   (both schemas type `arguments` as a string); shunt's SSE machine parses it
//!   into the Anthropic `tool_use.input` and the next turn re-serializes that with
//!   `serde_json` (sorted keys, no spaces), so the two strings differ byte-for-byte
//!   even when semantically identical — comparing the parsed values makes tool
//!   turns continue. This drift is *specific to shunt*: proxies that keep
//!   `arguments` an opaque string end-to-end (LiteLLM → chat-completions, codex →
//!   pure Responses) never parse it, so they never drift and need no such
//!   normalization; shunt is the outlier because Anthropic's `tool_use.input` is a
//!   parsed object. `namespace` appears only for namespaced/MCP tools and is not
//!   reconstructed; `call_id` already identifies the call.
//! - **`reasoning`** items round-trip `encrypted_content` verbatim (via the
//!   thinking-block signature) and shed the plaintext `content` array
//!   (`Optional[List[reasoning_text]]`, which shunt does not reconstruct) along
//!   with `id`/`status` like every item; their `summary` array is normalized
//!   part-by-part like `content`. The live probe found the backend actually omits
//!   `status` entirely and sends an empty `content` array under `store:false`, so
//!   the strip rules hold whether or not either field is present.
//!
//! [`translate_request`]: crate::model::responses::translate_request

use serde_json::{Map, Value};

/// Backend-only keys on an output item that shunt's reconstruction never carries.
const ITEM_STRIP_KEYS: &[&str] = &["id", "phase", "status"];
/// Backend-only keys on a content part (e.g. an `output_text` block).
const PART_STRIP_KEYS: &[&str] = &["annotations", "logprobs"];
/// Item-type keys whose value is an array of parts to normalize individually. A
/// `message` carries `content`; a `reasoning` item carries `summary`.
const PART_ARRAY_KEYS: &[&str] = &["content", "summary"];

/// Continuation state captured from a connection's previous turn. Lives on the
/// pooled connection, so `previous_response_id` is only ever used on the exact
/// connection that produced it (where the backend still holds its context).
#[derive(Clone, Debug)]
pub struct StoredContinuation {
    /// The previous turn's response id, replayed as `previous_response_id`.
    pub response_id: String,
    /// Signature of the previous request's non-input fields (see [`signature`]).
    pub signature: String,
    /// The previous request's input items ++ the backend's output items, the
    /// prefix the next request must extend.
    pub transcript: Vec<Value>,
    /// The `x-codex-turn-state` token to echo on the next request, if captured.
    pub turn_state: Option<String>,
}

/// A decision to continue: reuse `previous_response_id` and send only `input_delta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub previous_response_id: String,
    pub input_delta: Vec<Value>,
}

/// Decide whether the current request can continue from `stored`.
///
/// Returns `Some` only when the non-input request fields are unchanged AND the
/// current input is a strict append-only extension of the stored transcript
/// (compared with backend-only fields normalized away). The delta is the new
/// suffix. Returns `None` — meaning "send the full input" — on any mismatch, a
/// shrunk input, or an empty delta.
pub fn decide(stored: &StoredContinuation, current_body: &Value) -> Option<Decision> {
    if signature(current_body) != stored.signature {
        return None;
    }
    let input = current_body.get("input").and_then(Value::as_array)?;
    if stored.transcript.len() > input.len() {
        return None;
    }
    for (previous, current) in stored.transcript.iter().zip(input.iter()) {
        if normalize_item(previous) != normalize_item(current) {
            return None;
        }
    }
    let delta = input[stored.transcript.len()..].to_vec();
    if delta.is_empty() {
        // No new input to send; fall back to a full request rather than a
        // degenerate empty-delta turn.
        return None;
    }
    Some(Decision {
        previous_response_id: stored.response_id.clone(),
        input_delta: delta,
    })
}

/// Build the transcript to store after a turn: the request's full logical input
/// (not the delta) followed by the backend's output items.
pub fn build_transcript(request_input: &[Value], output_items: &[Value]) -> Vec<Value> {
    request_input
        .iter()
        .cloned()
        .chain(output_items.iter().cloned())
        .collect()
}

/// A stable, key-sorted signature of the request's non-input fields, so a changed
/// model/instructions/tools/reasoning/etc. forces a fresh context. Mirrors
/// `responses_request_properties_match` in openai/codex (which excludes `input`).
pub fn signature(body: &Value) -> String {
    let Some(object) = body.as_object() else {
        return String::new();
    };
    let filtered: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| key.as_str() != "input")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    stable_string(&Value::Object(filtered))
}

/// Deterministic JSON string with object keys sorted recursively, so the
/// signature is independent of serde's map ordering.
fn stable_string(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| format!("{}:{}", encode_str(key), stable_string(value)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(stable_string).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

fn encode_str(text: &str) -> String {
    Value::String(text.to_string()).to_string()
}

/// Rewrite an output item to the canonical shape shunt's reconstruction produces
/// for the same turn (see the module docs): shed the additive backend-only keys
/// `id`/`phase`/`status`, drop the per-type fields shunt never reconstructs
/// (a reasoning item's plaintext `content`, a function_call's `namespace`),
/// normalize each part of a `content`/`summary` array, and parse a
/// `function_call`'s `arguments` string into a value. Non-object values pass
/// through unchanged. Idempotent on shunt-produced items — they carry none of the
/// stripped keys, their parts are already minimal, and their `arguments` is a
/// value once parsed.
fn normalize_item(item: &Value) -> Value {
    let Some(object) = item.as_object() else {
        return item.clone();
    };
    let item_type = object.get("type").and_then(Value::as_str);
    let is_reasoning = item_type == Some("reasoning");
    let is_function_call = item_type == Some("function_call");
    let mut out = Map::new();
    for (key, value) in object {
        let key = key.as_str();
        if ITEM_STRIP_KEYS.contains(&key) {
            continue;
        }
        // Per-type backend fields shunt's reconstruction never carries (validated
        // against openai/codex `ResponseItem` + openai-python Responses types): a
        // reasoning item round-trips through `encrypted_content`, not its plaintext
        // `content` array; a `function_call.namespace` is set only for
        // namespaced/MCP tools and the `call_id` already identifies the call.
        if is_reasoning && key == "content" {
            continue;
        }
        if is_function_call && key == "namespace" {
            continue;
        }
        if PART_ARRAY_KEYS.contains(&key) {
            if let Some(parts) = value.as_array() {
                let parts = parts.iter().map(normalize_part).collect();
                out.insert(key.to_string(), Value::Array(parts));
                continue;
            }
        }
        if is_function_call && key == "arguments" {
            out.insert(key.to_string(), normalize_arguments(value));
            continue;
        }
        out.insert(key.to_string(), value.clone());
    }
    Value::Object(out)
}

/// Parse a `function_call.arguments` JSON string into a value so the comparison is
/// structural rather than string-exact (the backend's raw argument string and
/// shunt's re-serialized one differ in whitespace and key order). A non-string or
/// unparseable value is returned unchanged, so an odd shape falls back safely.
fn normalize_arguments(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| value.clone())
}

fn normalize_part(part: &Value) -> Value {
    let Some(object) = part.as_object() else {
        return part.clone();
    };
    let mut out = Map::new();
    for (key, value) in object {
        if PART_STRIP_KEYS.contains(&key.as_str()) {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    /// The backend's assistant message item, as captured live over the Codex
    /// WebSocket (issue #45, 2026-07-13): keys `content`/`id`/`phase`/`role`/
    /// `status`/`type`, with each content part carrying `annotations`/`logprobs`.
    fn backend_assistant(text: &str) -> Value {
        json!({
            "type": "message",
            "role": "assistant",
            "id": "msg_abc",
            "phase": "final_answer",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": [], "logprobs": []}]
        })
    }

    /// shunt's reconstruction of the same assistant turn echoed back by Claude
    /// Code (a strict subset of the backend item).
    fn reconstructed_assistant(text: &str) -> Value {
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        })
    }

    fn body(input: Vec<Value>) -> Value {
        json!({
            "model": "gpt-5.6-sol",
            "reasoning": {"effort": "medium", "summary": "auto"},
            "store": false,
            "stream": true,
            "input": input
        })
    }

    /// The backend's `function_call` output item, as captured live over the Codex
    /// WebSocket (issue #45, 2026-07-13): keys `arguments`/`call_id`/`id`/`name`/
    /// `status`/`type`, with `arguments` a JSON string in the model's own
    /// formatting (here spaced/reordered to exercise the structural comparison).
    /// A plain function tool carries no `namespace` (that appears only for MCP).
    fn backend_function_call() -> Value {
        json!({
            "type": "function_call",
            "id": "fc_123",
            "call_id": "call_abc",
            "name": "read_file",
            "arguments": "{\"path\": \"Cargo.toml\", \"limit\": 20}",
            "status": "completed"
        })
    }

    /// shunt's reconstruction of that tool call echoed back: no `id`/`status`, and
    /// `arguments` re-serialized from the parsed `tool_use.input` (sorted keys, no
    /// spaces — the shape `serde_json` produces).
    fn reconstructed_function_call() -> Value {
        json!({
            "type": "function_call",
            "call_id": "call_abc",
            "name": "read_file",
            "arguments": "{\"limit\":20,\"path\":\"Cargo.toml\"}"
        })
    }

    /// The backend's `reasoning` output item, as captured live over the Codex
    /// WebSocket (issue #45, 2026-07-13): keys `content`/`encrypted_content`/`id`/
    /// `summary`/`type`. Under `store:false` the backend omits `status` and sends
    /// an empty `content` array (the plaintext chain of thought is not returned);
    /// `summary` and `encrypted_content` round-trip through the thinking-block
    /// signature. The `content`/`status` strip is exercised defensively in
    /// [`normalize_reasoning_matches_reconstruction`].
    fn backend_reasoning() -> Value {
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "think about it"}],
            "content": [],
            "encrypted_content": "ENC"
        })
    }

    /// shunt's reconstruction of that reasoning item (from a decoded thinking-block
    /// signature): same id/summary/encrypted_content, no `status`.
    fn reconstructed_reasoning() -> Value {
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "think about it"}],
            "encrypted_content": "ENC"
        })
    }

    /// A client tool result echoed back as a `function_call_output` input item —
    /// produced by `translate_request` both turns, so it is byte-identical and
    /// needs no normalization.
    fn function_call_output() -> Value {
        json!({
            "type": "function_call_output",
            "call_id": "call_abc",
            "output": "file contents"
        })
    }

    #[test]
    fn normalize_strips_backend_only_fields_to_reconstruction() {
        assert_eq!(
            normalize_item(&backend_assistant("hello world")),
            normalize_item(&reconstructed_assistant("hello world"))
        );
    }

    #[test]
    fn normalize_function_call_matches_despite_arguments_formatting() {
        // The backend's spaced/reordered arguments string and shunt's compact,
        // sorted re-serialization normalize to the same value.
        assert_eq!(
            normalize_item(&backend_function_call()),
            normalize_item(&reconstructed_function_call())
        );
    }

    #[test]
    fn normalize_function_call_drops_namespace() {
        // A namespaced/MCP tool call carries `namespace` the reconstruction lacks;
        // normalization drops it so the append-only match still holds.
        let mut namespaced = backend_function_call();
        namespaced["namespace"] = json!("mcp__server");
        assert_eq!(
            normalize_item(&namespaced),
            normalize_item(&reconstructed_function_call())
        );
    }

    #[test]
    fn normalize_reasoning_matches_reconstruction() {
        // The live-captured item (empty `content`, no `status`) matches the
        // reconstruction once normalized.
        assert_eq!(
            normalize_item(&backend_reasoning()),
            normalize_item(&reconstructed_reasoning())
        );
        // Defensive: even if the backend ever returns `status` and a populated
        // plaintext `content` array (the API-returned shape), both are shed so the
        // append-only match still holds.
        let mut verbose = backend_reasoning();
        verbose["status"] = json!("completed");
        verbose["content"] =
            json!([{"type": "reasoning_text", "text": "long private chain of thought"}]);
        assert_eq!(
            normalize_item(&verbose),
            normalize_item(&reconstructed_reasoning())
        );
    }

    #[test]
    fn normalize_arguments_leaves_unparseable_string_untouched() {
        // A non-JSON arguments string is compared as-is (safe fallback, no panic).
        let odd = json!({"type": "function_call", "arguments": "not json"});
        assert_eq!(normalize_item(&odd)["arguments"], json!("not json"));
    }

    #[test]
    fn continues_across_a_tool_turn() {
        // Turn 1: the assistant reasoned then called a tool; the backend items are
        // a reasoning item ++ a function_call item.
        let stored = StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body(vec![user("run tests")])),
            transcript: build_transcript(
                &[user("run tests")],
                &[backend_reasoning(), backend_function_call()],
            ),
            turn_state: None,
        };
        // Turn 2: Claude Code echoes the reasoning + tool call (reconstructed) and
        // appends the tool result. The delta is just the tool result.
        let current = body(vec![
            user("run tests"),
            reconstructed_reasoning(),
            reconstructed_function_call(),
            function_call_output(),
        ]);
        let decision = decide(&stored, &current).expect("tool turn continues");
        assert_eq!(decision.previous_response_id, "resp_1");
        assert_eq!(decision.input_delta, vec![function_call_output()]);
    }

    #[test]
    fn continues_on_append_only_extension() {
        // Stored after turn 1: [user "hi"] ++ [backend assistant "hello"].
        let stored = StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body(vec![user("hi")])),
            transcript: build_transcript(&[user("hi")], &[backend_assistant("hello")]),
            turn_state: Some("ts_1".to_string()),
        };
        // Turn 2: echoes the assistant turn (reconstructed) and adds a new user turn.
        let current = body(vec![
            user("hi"),
            reconstructed_assistant("hello"),
            user("bye"),
        ]);
        let decision = decide(&stored, &current).expect("append-only turn continues");
        assert_eq!(decision.previous_response_id, "resp_1");
        assert_eq!(decision.input_delta, vec![user("bye")]);
    }

    #[test]
    fn falls_back_when_non_input_fields_change() {
        let stored = StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body(vec![user("hi")])),
            transcript: build_transcript(&[user("hi")], &[backend_assistant("hello")]),
            turn_state: None,
        };
        // Same input prefix, but the reasoning effort changed → fresh context.
        let mut current = body(vec![
            user("hi"),
            reconstructed_assistant("hello"),
            user("bye"),
        ]);
        current["reasoning"]["effort"] = json!("high");
        assert!(decide(&stored, &current).is_none());
    }

    #[test]
    fn falls_back_when_prefix_diverges() {
        let stored = StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body(vec![user("hi")])),
            transcript: build_transcript(&[user("hi")], &[backend_assistant("hello")]),
            turn_state: None,
        };
        // The first user turn was edited (history rewrite / compaction) → the
        // prefix no longer matches, so we must resend everything.
        let current = body(vec![
            user("HELLO EDITED"),
            reconstructed_assistant("hello"),
            user("bye"),
        ]);
        assert!(decide(&stored, &current).is_none());
    }

    #[test]
    fn falls_back_on_empty_delta() {
        let stored = StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body(vec![user("hi")])),
            transcript: build_transcript(&[user("hi")], &[]),
            turn_state: None,
        };
        // Current input is exactly the stored transcript: nothing new to send.
        let current = body(vec![user("hi")]);
        assert!(decide(&stored, &current).is_none());
    }

    #[test]
    fn falls_back_when_input_shorter_than_transcript() {
        let stored = StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body(vec![user("hi")])),
            transcript: build_transcript(&[user("hi"), user("there")], &[]),
            turn_state: None,
        };
        let current = body(vec![user("hi")]);
        assert!(decide(&stored, &current).is_none());
    }

    #[test]
    fn signature_ignores_input_and_is_order_stable() {
        let a = signature(&json!({"model": "m", "stream": true, "input": [user("x")]}));
        let b = signature(&json!({"stream": true, "model": "m", "input": [user("y")]}));
        assert_eq!(
            a, b,
            "signature excludes input and is key-order independent"
        );
        let c = signature(&json!({"model": "n", "stream": true, "input": []}));
        assert_ne!(a, c, "a changed non-input field changes the signature");
    }
}
