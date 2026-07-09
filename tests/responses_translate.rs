use axum::http::StatusCode;
use serde_json::{json, Value};
use shunt::{
    model::responses::{
        anthropic_error_type, map_error_value, parse_sse_events, translate_request,
        AnthropicSseMachine,
    },
    routing::{AdapterKind, Route},
};

fn route(model: &str) -> Route {
    Route {
        provider: "openai".to_string(),
        adapter: AdapterKind::Responses,
        model: model.to_string(),
        upstream_model: model.to_string(),
        effort: None,
    }
}

fn translate(input: Value) -> Value {
    let body = serde_json::to_vec(&input).unwrap();
    translate_request(&body, &route("gpt-5.2-codex")).unwrap()
}

#[test]
fn translates_plain_text_request() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "system": [{"type": "text", "text": "Be terse"}, {"type": "cache_control"}],
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 1000
    }));

    assert_eq!(
        actual,
        json!({
            "model": "gpt-5.2-codex",
            "instructions": "Be terse",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }],
            "reasoning": {"effort": "medium", "summary": "auto"},
            "text": {"verbosity": "medium"},
            "max_output_tokens": 1000,
            "store": false,
            "stream": true
        })
    );
}

#[test]
fn translates_multi_turn_text_roles() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "one"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "two"}]},
            {"role": "user", "content": [{"type": "text", "text": "three"}]}
        ]
    }));

    assert_eq!(
        actual["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "one"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "two"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "three"}]}
        ])
    );
}

#[test]
fn preserves_tool_use_and_tool_result_call_ids() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "text", "text": "calling"},
                {"type": "tool_use", "id": "toolu_123", "name": "read_file", "input": {"path": "Cargo.toml"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_123", "content": [{"type": "text", "text": "ok"}]}
            ]}
        ]
    }));

    assert_eq!(
        actual["input"],
        json!([
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "calling"}]},
            {"type": "function_call", "call_id": "toolu_123", "name": "read_file", "arguments": "{\"path\":\"Cargo.toml\"}"},
            {"type": "function_call_output", "call_id": "toolu_123", "output": "ok"}
        ])
    );
}

#[test]
fn translates_image_content_to_data_url() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "inspect"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}
        ]}]
    }));

    assert_eq!(
        actual["input"],
        json!([{
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "inspect"},
                {"type": "input_image", "image_url": "data:image/png;base64,abc"}
            ]
        }])
    );
}

#[test]
fn translates_tools_and_tool_choice_variants() {
    let base = json!({
        "model": "gpt-5.2-codex",
        "messages": [],
        "tools": [{
            "name": "run",
            "description": "Run command",
            "input_schema": {"properties": {"cmd": {"type": "string"}}, "required": "cmd"}
        }]
    });

    let default_choice = translate(base.clone());
    assert_eq!(default_choice["tool_choice"], json!("auto"));
    assert_eq!(
        default_choice["tools"],
        json!([{
            "type": "function",
            "name": "run",
            "description": "Run command",
            "parameters": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "additionalProperties": true
            }
        }])
    );

    for (anthropic, responses) in [
        (json!({"type": "auto"}), json!("auto")),
        (json!({"type": "none"}), json!("none")),
        (json!({"type": "any"}), json!("required")),
        (
            json!({"type": "tool", "name": "run"}),
            json!({"type": "function", "name": "run"}),
        ),
    ] {
        let mut input = base.clone();
        input["tool_choice"] = anthropic;
        assert_eq!(translate(input)["tool_choice"], responses);
    }
}

#[test]
fn maps_thinking_and_route_override_to_effort() {
    let thinking = translate(json!({
        "model": "gpt-5.2-codex",
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "messages": []
    }));
    assert_eq!(thinking["reasoning"]["effort"], "high");

    let mut route = route("gpt-5.2-codex-low");
    route.effort = Some("xhigh".to_string());
    let body = serde_json::to_vec(&json!({"model": "gpt-5.2-codex-low", "messages": []})).unwrap();
    let override_effort = translate_request(&body, &route).unwrap();
    assert_eq!(override_effort["reasoning"]["effort"], "xhigh");
}

#[test]
fn streaming_state_machine_emits_incremental_anthropic_events() {
    let fixture = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"output_tokens\":0}}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"message\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"Hel\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"lo\"}\n\n",
        "event: response.output_text.done\n",
        "data: {}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"delta\":\"{\\\"path\\\":\"}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"delta\":\"\\\"Cargo.toml\\\"}\"}\n\n",
        "event: response.function_call_arguments.done\n",
        "data: {\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":800},\"output_tokens\":9}}}\n\n",
        "data: [DONE]\n\n"
    );
    let mut machine = AnthropicSseMachine::new("gpt-5.2-codex", false);
    let emitted = parse_sse_events(fixture)
        .into_iter()
        .flat_map(|event| machine.apply(event))
        .collect::<String>();
    let names = event_names(&emitted);

    assert_eq!(
        names,
        vec![
            "message_start",
            "ping",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert!(emitted.contains("\"text\":\"Hel\""));
    assert!(emitted.contains("\"partial_json\":\"{\\\"path\\\":\""));
    assert!(emitted.contains("\"stop_reason\":\"tool_use\""));
    // Prompt-size usage must reach message_delta so Claude Code's context
    // indicator works for non-Anthropic (Responses) models. OpenAI input_tokens
    // (1200, incl. 800 cached) splits into input_tokens 400 + cache_read 800,
    // preserving the 1200 total the context window is charted against.
    assert!(emitted.contains("\"input_tokens\":400"));
    assert!(emitted.contains("\"cache_read_input_tokens\":800"));
    assert!(emitted.contains("\"output_tokens\":9"));
}

#[test]
fn maps_upstream_error_statuses() {
    assert_eq!(
        anthropic_error_type(StatusCode::UNAUTHORIZED),
        "authentication_error"
    );
    assert_eq!(
        anthropic_error_type(StatusCode::TOO_MANY_REQUESTS),
        "rate_limit_error"
    );
    assert_eq!(
        anthropic_error_type(StatusCode::BAD_REQUEST),
        "invalid_request_error"
    );
    assert_eq!(
        anthropic_error_type(StatusCode::INTERNAL_SERVER_ERROR),
        "api_error"
    );
}

#[test]
fn surfaces_upstream_error_detail_and_message() {
    // ChatGPT Codex backend shape: {"detail": "..."}
    let codex = map_error_value(
        &json!({"detail": "The 'gpt-x' model is not supported when using Codex with a ChatGPT account."}),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(codex["error"]["type"], "invalid_request_error");
    assert_eq!(
        codex["error"]["message"],
        "The 'gpt-x' model is not supported when using Codex with a ChatGPT account."
    );

    // OpenAI Responses shape: {"error":{"message": "..."}}
    let openai = map_error_value(
        &json!({"error": {"message": "invalid model"}}),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(openai["error"]["message"], "invalid model");

    // Unknown shape falls back to a generic message.
    let unknown = map_error_value(&json!({"weird": true}), StatusCode::BAD_GATEWAY);
    assert_eq!(unknown["error"]["message"], "upstream request failed");
}

fn event_names(sse: &str) -> Vec<String> {
    sse.split("\n\n")
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("event: ").map(ToOwned::to_owned))
        })
        .collect()
}

#[test]
fn includes_encrypted_reasoning_only_when_thinking_enabled() {
    let with_thinking = translate(json!({
        "thinking": {"type": "enabled"},
        "messages": [{"role": "user", "content": "hi"}]
    }));
    assert_eq!(
        with_thinking["include"],
        json!(["reasoning.encrypted_content"])
    );

    let without = translate(json!({
        "messages": [{"role": "user", "content": "hi"}]
    }));
    assert!(without.get("include").is_none());
}

/// End-to-end: a reasoning item streams out as a thinking block whose signature
/// carries the encrypted state, and feeding that block back yields a Responses
/// `reasoning` input item — preserving chain-of-thought under store:false.
#[test]
fn streams_reasoning_as_thinking_block_and_round_trips() {
    let fixture = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"delta\":\"Let me\"}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"delta\":\" think\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"ENC123\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"message\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"Hi\"}\n\n",
        "event: response.output_text.done\n",
        "data: {}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n",
        "data: [DONE]\n\n"
    );

    let mut machine = AnthropicSseMachine::new("gpt-5.2-codex", true);
    let emitted = parse_sse_events(fixture)
        .into_iter()
        .flat_map(|event| machine.apply(event))
        .collect::<String>();
    let mut finished = machine.finish().join("");
    finished.insert_str(0, &emitted);
    let emitted = finished;

    // A thinking block leads the message, streams summary text, then a signature.
    let names = event_names(&emitted);
    assert_eq!(names.first().map(String::as_str), Some("message_start"));
    assert!(emitted.contains("\"type\":\"thinking\""));
    assert!(emitted.contains("\"thinking_delta\""));
    assert!(emitted.contains("\"signature_delta\""));

    let expected_signature = shunt::model::responses::encode_reasoning_signature("rs_1", "ENC123");
    assert!(emitted.contains(&expected_signature));

    // Feed the thinking block back: it must become a reasoning input item.
    let out = translate(json!({
        "thinking": {"type": "enabled"},
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Let me think", "signature": expected_signature},
                {"type": "text", "text": "Hi"}
            ]}
        ]
    }));
    let input = out["input"].as_array().unwrap();
    let reasoning = input
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning input item present");
    assert_eq!(reasoning["id"], "rs_1");
    assert_eq!(reasoning["encrypted_content"], "ENC123");
    // Reasoning must precede the assistant message it reasoned about.
    let reasoning_pos = input.iter().position(|i| i["type"] == "reasoning").unwrap();
    let message_pos = input
        .iter()
        .position(|i| i["type"] == "message" && i["role"] == "assistant")
        .unwrap();
    assert!(reasoning_pos < message_pos);
}

#[test]
fn drops_foreign_thinking_signature() {
    // A signature shunt did not produce (e.g. a genuine Anthropic one) is dropped,
    // never forwarded as a bogus reasoning item the backend would reject.
    let out = translate(json!({
        "thinking": {"type": "enabled"},
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "x", "signature": "not-a-shunt-signature"},
                {"type": "text", "text": "Hi"}
            ]}
        ]
    }));
    let input = out["input"].as_array().unwrap();
    assert!(input.iter().all(|item| item["type"] != "reasoning"));
}

#[test]
fn ignores_reasoning_when_thinking_disabled() {
    let fixture = concat!(
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"delta\":\"secret\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"ENC\"}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
    );
    let mut machine = AnthropicSseMachine::new("gpt-5.2-codex", false);
    let emitted = parse_sse_events(fixture)
        .into_iter()
        .flat_map(|event| machine.apply(event))
        .collect::<String>();
    assert!(!emitted.contains("thinking"));
    assert!(!emitted.contains("signature"));
}

#[test]
fn derives_prompt_cache_key_from_session_id() {
    // Claude Code packs a JSON blob into metadata.user_id; session_id is the
    // stable per-conversation key the Responses cache should be routed by.
    let out = translate(json!({
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": "{\"device_id\":\"d1\",\"session_id\":\"sess_abc\"}"}
    }));
    assert_eq!(out["prompt_cache_key"], "shunt-sess_abc");

    // No metadata -> no key sent.
    let bare = translate(json!({"messages": [{"role": "user", "content": "hi"}]}));
    assert!(bare.get("prompt_cache_key").is_none());

    // A non-JSON user_id still yields a stable (hashed) key.
    let hashed = translate(json!({
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": "plain-user"}
    }));
    let key = hashed["prompt_cache_key"].as_str().unwrap();
    assert!(key.starts_with("shunt-"));
    // Determinism: same input -> same key.
    let again = translate(json!({
        "messages": [{"role": "user", "content": "different"}],
        "metadata": {"user_id": "plain-user"}
    }));
    assert_eq!(hashed["prompt_cache_key"], again["prompt_cache_key"]);
}

#[test]
fn tool_result_with_image_becomes_content_array() {
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "see screenshot"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "IMG"}}
            ]}
        ]}]
    }));
    let output = &out["input"][0]["output"];
    assert_eq!(
        *output,
        json!([
            {"type": "input_text", "text": "see screenshot"},
            {"type": "input_image", "image_url": "data:image/png;base64,IMG"}
        ])
    );
}

#[test]
fn text_only_tool_result_stays_a_string() {
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "ok"}
            ]}
        ]}]
    }));
    assert_eq!(out["input"][0]["output"], json!("ok"));
}

#[test]
fn document_block_becomes_input_file() {
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "read this"},
            {"type": "document", "title": "spec.pdf", "source": {"type": "base64", "media_type": "application/pdf", "data": "PDF"}}
        ]}]
    }));
    assert_eq!(
        out["input"][0]["content"],
        json!([
            {"type": "input_text", "text": "read this"},
            {"type": "input_file", "file_data": "data:application/pdf;base64,PDF", "filename": "spec.pdf"}
        ])
    );
}

#[test]
fn url_sourced_document_uses_file_url_not_empty_data() {
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "document", "source": {"type": "url", "url": "https://example.com/spec.pdf"}}
        ]}]
    }));
    assert_eq!(
        out["input"][0]["content"][0],
        json!({"type": "input_file", "file_url": "https://example.com/spec.pdf"})
    );
}

#[test]
fn url_sourced_image_passes_url_through() {
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
        ]}]
    }));
    assert_eq!(
        out["input"][0]["content"][0],
        json!({"type": "input_image", "image_url": "https://example.com/x.png"})
    );
}

#[test]
fn unrepresentable_document_source_is_dropped_not_emptied() {
    // A source shunt can't represent must not become an empty "data:...;base64," URI.
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "read"},
            {"type": "document", "source": {"type": "file", "file_id": "file_123"}}
        ]}]
    }));
    // Only the text survives; the unrepresentable document is dropped.
    assert_eq!(
        out["input"][0]["content"],
        json!([{"type": "input_text", "text": "read"}])
    );
}

#[test]
fn errored_tool_result_with_image_keeps_failure_signal() {
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "IMG"}}
            ]}
        ]}]
    }));
    assert_eq!(
        out["input"][0]["output"],
        json!([
            {"type": "input_text", "text": "Tool execution failed"},
            {"type": "input_image", "image_url": "data:image/png;base64,IMG"}
        ])
    );
}

#[test]
fn errored_tool_result_with_text_and_image_keeps_text_only() {
    // When the tool provided its own error text, don't inject a duplicate marker.
    let out = translate(json!({
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": [
                {"type": "text", "text": "boom: file missing"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "IMG"}}
            ]}
        ]}]
    }));
    assert_eq!(
        out["input"][0]["output"],
        json!([
            {"type": "input_text", "text": "boom: file missing"},
            {"type": "input_image", "image_url": "data:image/png;base64,IMG"}
        ])
    );
}

#[test]
fn reasoning_id_falls_back_to_done_event_when_added_missing() {
    // No output_item.added for the reasoning item (so the buffer id is empty);
    // the id must be recovered from the output_item.done event's item.
    let fixture = concat!(
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"delta\":\"think\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_done\",\"encrypted_content\":\"ENC\"}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
    );
    let mut machine = AnthropicSseMachine::new("gpt-5.2-codex", true);
    let emitted = parse_sse_events(fixture)
        .into_iter()
        .flat_map(|event| machine.apply(event))
        .collect::<String>();
    let expected = shunt::model::responses::encode_reasoning_signature("rs_done", "ENC");
    assert!(
        emitted.contains(&expected),
        "signature should encode the id from the done event"
    );
}
