use super::continuation::{ConnectionContinuation, TurnTarget};
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
/// upstream response (`resp_1`) produced `output`, served by the endpoint's
/// own ChatGPT/Codex pool.
fn stored_after(
    input: Vec<serde_json::Value>,
    output: Vec<serde_json::Value>,
) -> SharedContinuation {
    stored_after_from("codex", input, output)
}

/// [`stored_after`] for a response `provider` served.
fn stored_after_from(
    provider: &str,
    input: Vec<serde_json::Value>,
    output: Vec<serde_json::Value>,
) -> SharedContinuation {
    let body = json!({"model": "gpt-6.1-sol", "stream": true, "input": []});
    Arc::new(std::sync::Mutex::new(Some(ConnectionContinuation {
        stored: StoredContinuation {
            response_id: "resp_1".to_string(),
            signature: signature(&body),
            transcript: build_transcript(&input, &output),
            turn_state: None,
        },
        provider: provider.to_string(),
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

fn assistant_item(text: &str) -> serde_json::Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
}

#[test]
fn full_history_with_a_changed_setting_is_not_doubled() {
    // A changed non-input field fails the continuation signature, but the
    // input still carries the whole history, so it must not be prefixed with
    // the stored transcript again.
    let continuation = stored_after(vec![user_item("run pwd")], vec![assistant_item("ok")]);
    let history = vec![
        user_item("run pwd"),
        assistant_item("ok"),
        user_item("again"),
    ];
    let mut frame = json!({
        "model": "gpt-6.1-sol", "stream": true,
        "reasoning": {"effort": "high"},
        "previous_response_id": "resp_1",
        "input": history.clone(),
    });

    let logical = prepare_body(&mut frame, &continuation);

    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(logical, history);
    assert_eq!(frame["input"].as_array().unwrap(), &history);
}

#[test]
fn full_history_equal_to_the_transcript_is_not_doubled() {
    let continuation = stored_after(vec![user_item("run pwd")], vec![assistant_item("ok")]);
    let history = vec![user_item("run pwd"), assistant_item("ok")];
    let mut frame = json!({
        "model": "gpt-6.1-sol", "stream": true,
        "previous_response_id": "resp_1",
        "input": history.clone(),
    });

    let logical = prepare_body(&mut frame, &continuation);

    assert_eq!(logical, history);
    assert_eq!(frame["input"].as_array().unwrap(), &history);
}

#[test]
fn routed_frame_keeps_previous_response_id_and_its_delta() {
    // A routed Responses API honours a `previous_response_id` it issued
    // itself: the body is forwarded as sent, while the recorded logical input
    // still carries the whole history.
    let continuation = stored_after_from(
        "openai",
        vec![user_item("run pwd")],
        vec![assistant_item("ok")],
    );
    let mut frame = json!({
        "model": "glm-5.3", "stream": true,
        "previous_response_id": "resp_1",
        "input": [user_item("again")],
    });
    let sent = frame.clone();

    let logical = prepare_routed_body(&mut frame, &continuation, "openai");

    assert_eq!(frame, sent);
    assert_eq!(
        logical,
        vec![
            user_item("run pwd"),
            assistant_item("ok"),
            user_item("again")
        ]
    );
}

#[test]
fn only_routes_to_a_non_chatgpt_provider_keep_previous_response_id() {
    use crate::config::{CodexEndpointConfig, CodexRouteConfig, Config};
    let route = |model: &str, provider: &str| CodexRouteConfig {
        model: model.to_string(),
        provider: provider.to_string(),
        upstream_model: None,
    };
    let mut config = Config::default();
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![route("glm-5.3", "openai"), route("pool-model", "codex")],
    });

    let target = |provider: &str, chatgpt: bool| TurnTarget {
        provider: provider.to_string(),
        chatgpt,
    };
    assert_eq!(
        resolve_target(&config, Some("glm-5.3")),
        target("openai", false)
    );
    assert_eq!(
        resolve_target(&config, Some("pool-model")),
        target("codex", true)
    );
    assert_eq!(
        resolve_target(&config, Some("gpt-6.1-sol")),
        target("codex", true)
    );
    assert_eq!(resolve_target(&config, None), target("codex", true));
}

#[test]
fn string_input_is_recorded_and_expanded_as_a_user_message() {
    let continuation: SharedContinuation = Arc::new(std::sync::Mutex::new(None));
    let mut first = json!({"model": "gpt-6.1-sol", "stream": true, "input": "run pwd"});

    let logical = prepare_body(&mut first, &continuation);

    // The wire body keeps the client's string; only the logical input is
    // normalized.
    assert_eq!(first["input"], "run pwd");
    assert_eq!(logical, vec![user_item("run pwd")]);

    let mut recorder = TurnRecorder::new(
        continuation.clone(),
        "codex".to_string(),
        logical,
        signature(&first),
    );
    let completed = json!({
        "type": "response.completed",
        "response": {"id": "resp_1", "output": [assistant_item("/tmp")]},
    })
    .to_string();
    recorder.observe("response.completed", &completed);

    let mut delta = json!({
        "model": "gpt-6.1-sol", "stream": true,
        "previous_response_id": "resp_1",
        "input": "again",
    });
    let logical = prepare_body(&mut delta, &continuation);

    let expected = vec![
        user_item("run pwd"),
        assistant_item("/tmp"),
        user_item("again"),
    ];
    assert_eq!(logical, expected);
    assert_eq!(delta["input"].as_array().unwrap(), &expected);
}

#[test]
fn recorder_captures_a_top_level_id_and_streamed_output_items() {
    // `response.done` may carry its id at the top level and omit
    // `response.output`; the items then come from `response.output_item.done`.
    let continuation: SharedContinuation = Arc::new(std::sync::Mutex::new(None));
    let mut recorder = TurnRecorder::new(
        continuation.clone(),
        "codex".to_string(),
        vec![user_item("hi")],
        "sig".to_string(),
    );
    let item_done = json!({
        "type": "response.output_item.done",
        "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": []},
    })
    .to_string();
    recorder.observe("response.output_item.done", &item_done);
    let done = json!({"type": "response.done", "id": "resp_top"}).to_string();
    recorder.observe("response.done", &done);

    let recorded = continuation.lock().unwrap().clone().expect("recorded");
    assert_eq!(recorded.provider, "codex");
    let stored = recorded.stored;
    assert_eq!(stored.response_id, "resp_top");
    assert_eq!(
        stored.transcript,
        vec![
            user_item("hi"),
            json!({"id": "msg_1", "type": "message", "role": "assistant", "content": []}),
        ]
    );
}

#[test]
fn recorder_clears_on_a_failed_terminal() {
    let continuation = stored_after(vec![user_item("hi")], vec![]);
    let mut recorder = TurnRecorder::new(
        continuation.clone(),
        "codex".to_string(),
        Vec::new(),
        String::new(),
    );
    recorder.observe(
        "response.created",
        &json!({"type": "response.created", "response": {"id": "resp_2"}}).to_string(),
    );
    recorder.observe(
        "response.failed",
        &json!({"type": "response.failed", "response": {"id": "resp_2"}}).to_string(),
    );

    assert!(continuation.lock().unwrap().is_none());
}

#[test]
fn routed_turn_after_a_chatgpt_served_turn_gets_the_id_stripped_and_full_input() {
    // The stored response came from the ChatGPT pool, so the routed provider
    // never issued `resp_1` and cannot resolve it: shunt sends the whole
    // history instead.
    let continuation = stored_after(vec![user_item("run pwd")], vec![assistant_item("ok")]);
    let mut frame = json!({
        "model": "glm-5.3", "stream": true,
        "previous_response_id": "resp_1",
        "input": [user_item("again")],
    });

    let logical = prepare_routed_body(&mut frame, &continuation, "openai");

    let expected = vec![
        user_item("run pwd"),
        assistant_item("ok"),
        user_item("again"),
    ];
    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(logical, expected);
    assert_eq!(frame["input"].as_array().unwrap(), &expected);
}

#[test]
fn routed_turn_after_a_same_provider_turn_keeps_the_id() {
    let continuation = stored_after_from(
        "openai",
        vec![user_item("run pwd")],
        vec![assistant_item("ok")],
    );
    let mut recorder = TurnRecorder::new(
        continuation.clone(),
        "openai".to_string(),
        vec![
            user_item("run pwd"),
            assistant_item("ok"),
            user_item("again"),
        ],
        String::new(),
    );
    recorder.observe(
        "response.completed",
        &json!({"type": "response.completed", "response": {"id": "resp_2", "output": []}})
            .to_string(),
    );
    let mut frame = json!({
        "model": "glm-5.3", "stream": true,
        "previous_response_id": "resp_2",
        "input": [user_item("more")],
    });
    let sent = frame.clone();

    let logical = prepare_routed_body(&mut frame, &continuation, "openai");

    assert_eq!(frame, sent);
    assert_eq!(logical.len(), 4);
}

#[test]
fn routed_turn_with_an_unknown_id_is_forwarded_unchanged() {
    let continuation = stored_after(vec![user_item("hi")], vec![]);
    let mut frame = json!({
        "model": "glm-5.3", "stream": true,
        "previous_response_id": "resp_other",
        "input": [user_item("again")],
    });
    let sent = frame.clone();

    let logical = prepare_routed_body(&mut frame, &continuation, "openai");

    assert_eq!(frame, sent);
    assert_eq!(logical, vec![user_item("again")]);
}
