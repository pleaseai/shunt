use super::*;
use crate::adapters::responses::codex_continuation::{
    build_transcript, signature, StoredContinuation,
};
use serde_json::json;

/// Run the check against a handshake carrying the given `Origin` / `Host`.
fn allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let mut headers = HeaderMap::new();
    if let Some(origin) = origin {
        headers.insert(header::ORIGIN, origin.parse().unwrap());
    }
    if let Some(host) = host {
        headers.insert(header::HOST, host.parse().unwrap());
    }
    same_origin_or_non_browser(&headers)
}

#[test]
fn same_origin_accepts_ipv6_with_and_without_port() {
    assert!(allowed(Some("http://[::1]:3001"), Some("[::1]:3001")));
    assert!(allowed(Some("http://[::1]"), Some("[::1]")));
}

#[test]
fn same_origin_rejects_ipv6_host_mismatch() {
    assert!(!allowed(Some("http://[::2]:3001"), Some("[::1]:3001")));
}

#[test]
fn same_origin_accepts_ipv4_and_rejects_port_mismatch() {
    assert!(allowed(
        Some("http://127.0.0.1:3001"),
        Some("127.0.0.1:3001")
    ));
    assert!(!allowed(
        Some("http://127.0.0.1:3002"),
        Some("127.0.0.1:3001")
    ));
}

#[test]
fn missing_origin_is_non_browser_and_missing_host_is_rejected() {
    assert!(allowed(None, Some("127.0.0.1:3001")));
    assert!(!allowed(Some("http://127.0.0.1:3001"), None));
}

/// The `OpenAI-Beta` an HTTP turn forwards for handshake values `betas`
/// (each a separate header field), or `None` when it carries none.
fn http_beta(betas: &[&str]) -> Option<String> {
    let mut headers = HeaderMap::new();
    for beta in betas {
        headers.append("openai-beta", beta.parse().unwrap());
    }
    http_openai_beta(&mut headers);
    assert!(headers.get_all("openai-beta").iter().count() <= 1);
    headers
        .get("openai-beta")
        .map(|value| value.to_str().unwrap().to_owned())
}

#[test]
fn openai_beta_swaps_only_the_websocket_selector() {
    assert_eq!(
        http_beta(&["responses_websockets=2026-02-06"]).as_deref(),
        Some("responses=experimental")
    );
    assert_eq!(
        http_beta(&["responses_websockets=2026-02-06, assistants=v2"]).as_deref(),
        Some("assistants=v2, responses=experimental")
    );
    assert_eq!(
        http_beta(&["responses=experimental"]).as_deref(),
        Some("responses=experimental")
    );
}

#[test]
fn openai_beta_merges_repeated_fields_and_stays_absent_when_absent() {
    assert_eq!(
        http_beta(&["responses_websockets=2026-02-06", "assistants=v2"]).as_deref(),
        Some("assistants=v2, responses=experimental")
    );
    assert_eq!(http_beta(&[]), None);
}

/// A stored continuation for a first turn that sent `input` and whose
/// upstream response (`resp_1`) produced `output`.
fn stored_after(
    input: Vec<serde_json::Value>,
    output: Vec<serde_json::Value>,
) -> SharedContinuation {
    let body = json!({"model": "gpt-6.1-sol", "stream": true, "input": []});
    Arc::new(std::sync::Mutex::new(Some(StoredContinuation {
        response_id: "resp_1".to_string(),
        signature: signature(&body),
        transcript: build_transcript(&input, &output),
        turn_state: None,
    })))
}

fn user_item(text: &str) -> serde_json::Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

#[test]
fn incremental_frame_expands_to_full_input_and_drops_previous_response_id() {
    // The Codex CLI's second sampling request in a turn carries only the
    // tool output plus `previous_response_id`; the HTTP upstream rejects
    // that field, so the prepared body must carry the full transcript.
    let first_input = vec![user_item("run pwd")];
    let tool_call =
        json!({"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": "pwd"});
    let continuation = stored_after(first_input.clone(), vec![tool_call.clone()]);
    let tool_output =
        json!({"type": "custom_tool_call_output", "call_id": "call_1", "output": "/tmp"});
    let mut frame = json!({
        "model": "gpt-6.1-sol", "stream": true,
        "previous_response_id": "resp_1",
        "input": [tool_output.clone()],
    });

    let logical = prepare_body(&mut frame, &continuation);

    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(logical, vec![user_item("run pwd"), tool_call, tool_output]);
    assert_eq!(frame["input"].as_array().unwrap(), &logical);
}

#[test]
fn full_input_frame_with_previous_response_id_is_not_doubled() {
    let first_input = vec![user_item("run pwd")];
    let assistant = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]});
    let continuation = stored_after(first_input, vec![assistant.clone()]);
    let mut frame = json!({
        "model": "gpt-6.1-sol", "stream": true,
        "previous_response_id": "resp_1",
        "input": [user_item("run pwd"), assistant, user_item("again")],
    });

    let logical = prepare_body(&mut frame, &continuation);

    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(logical.len(), 3);
    assert_eq!(frame["input"].as_array().unwrap(), &logical);
}

#[test]
fn unknown_previous_response_id_is_dropped_without_expansion() {
    let continuation = stored_after(vec![user_item("hi")], vec![]);
    let mut frame = json!({
        "model": "gpt-6.1-sol", "stream": true,
        "previous_response_id": "resp_other",
        "input": [user_item("fresh")],
    });

    let logical = prepare_body(&mut frame, &continuation);

    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(logical, vec![user_item("fresh")]);
}
