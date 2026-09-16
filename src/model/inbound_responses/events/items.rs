//! The Responses output items both the streaming and the non-streaming
//! translators build.
//!
//! Kept in one place so the two paths cannot drift: the emitter stamps these
//! into its `output_item.done` frames and its `response.completed` envelope,
//! while a non-streaming translator builds the same items and hands them to
//! [`super::ResponsesEmitter::push_item`].

use serde_json::{json, Value};
use uuid::Uuid;

/// A fresh Responses item id: the API's per-kind prefix plus a uuid.
pub fn item_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

pub fn message_item(id: &str, text: &str, status: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": status,
        "role": "assistant",
        "content": if status == "completed" { json!([text_part(text)]) } else { json!([]) },
    })
}

/// A `reasoning` item. `summary_text` is `None` when the upstream produced no
/// visible summary (a redacted thinking block), which yields an empty
/// `summary` array rather than one empty part.
pub fn reasoning_item(
    id: &str,
    summary_text: Option<&str>,
    encrypted_content: Option<&str>,
) -> Value {
    let mut item = json!({
        "id": id,
        "type": "reasoning",
        "summary": summary_text.map_or_else(|| json!([]), |text| json!([summary_part(text)])),
    });
    if let Some(encrypted_content) = encrypted_content {
        item["encrypted_content"] = json!(encrypted_content);
    }
    item
}

pub fn function_call_item(
    id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    status: &str,
) -> Value {
    json!({
        "id": id,
        "type": "function_call",
        "status": status,
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

pub fn web_search_item(id: &str, query: Option<&str>, status: &str) -> Value {
    let mut action = json!({"type": "search"});
    if let Some(query) = query {
        action["query"] = json!(query);
    }
    json!({"id": id, "type": "web_search_call", "status": status, "action": action})
}

pub(super) fn text_part(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": []})
}

pub(super) fn summary_part(text: &str) -> Value {
    json!({"type": "summary_text", "text": text})
}
