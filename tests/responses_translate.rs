use axum::http::StatusCode;
use serde_json::{json, Value};
use shunt::{
    model::responses::{
        anthropic_error_type, parse_sse_events, translate_request, AnthropicSseMachine,
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
        "data: {\"response\":{\"usage\":{\"output_tokens\":9}}}\n\n",
        "data: [DONE]\n\n"
    );
    let mut machine = AnthropicSseMachine::new("gpt-5.2-codex");
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

fn event_names(sse: &str) -> Vec<String> {
    sse.split("\n\n")
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("event: ").map(ToOwned::to_owned))
        })
        .collect()
}
