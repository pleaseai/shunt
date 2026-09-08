use serde_json::json;

use super::*;
use crate::model::inbound_responses::reasoning;

fn translate(request: Value) -> Value {
    translate_request(&request, "claude-sonnet-4-5").expect("request translates")
}

/// A minimal turn to hang a non-input behavior off, since a request with no
/// message is rejected outright.
fn with_input(mut request: Value) -> Value {
    request["input"] = json!("hi");
    request
}

#[test]
fn string_input_becomes_one_user_message() {
    let out = translate(json!({"model": "gpt-5.5-codex", "input": "hello"}));

    assert_eq!(out["model"], "claude-sonnet-4-5");
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
    );
}

#[test]
fn instructions_and_developer_items_fold_into_system() {
    let out = translate(json!({
        "instructions": "You are codex.",
        "input": [
            {"type": "message", "role": "developer", "content": [
                {"type": "input_text", "text": "Prefer small diffs."}
            ]},
            {"type": "message", "role": "system", "content": "No emoji."},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "fix the bug"}
            ]}
        ]
    }));

    assert_eq!(
        out["system"],
        "You are codex.\nPrefer small diffs.\nNo emoji."
    );
    // System/developer items never become Anthropic messages.
    assert_eq!(out["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(out["messages"][0]["role"], "user");
}

#[test]
fn consecutive_same_role_items_merge_into_one_message() {
    let out = translate(json!({
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "one"}]},
            {"type": "message", "role": "user", "content": "two"},
            {"role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "three"}]}
        ]
    }));

    let messages = out["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0],
        json!({"role": "user", "content": [
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two"}
        ]})
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["content"][0]["text"], "three");
}

#[test]
fn function_call_becomes_a_tool_use_with_parsed_arguments() {
    let out = translate(json!({
        "input": [
            {"type": "message", "role": "user", "content": "run it"},
            {"type": "function_call", "call_id": "call_1", "name": "bash",
             "arguments": "{\"command\":\"ls\"}"},
            {"type": "function_call", "call_id": "call_2", "name": "bash",
             "arguments": "not json"}
        ]
    }));

    let blocks = out["messages"][1]["content"].as_array().expect("blocks");
    assert_eq!(out["messages"][1]["role"], "assistant");
    assert_eq!(
        blocks[0],
        json!({"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}})
    );
    // Unparsable arguments must not fail the turn.
    assert_eq!(blocks[1]["input"], json!({}));
}

#[test]
fn function_call_output_becomes_a_user_tool_result() {
    let out = translate(json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "bash", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "README.md"},
            {"type": "function_call_output", "call_id": "call_2", "output": [
                {"type": "output_text", "text": "done"},
                {"type": "input_image", "image_url": "https://example.com/shot.png"}
            ]}
        ]
    }));

    // A function_call_output following a function_call starts a new user turn.
    let messages = out["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(
        messages[1]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "call_1", "content": "README.md"})
    );
    assert_eq!(
        messages[1]["content"][1]["content"],
        json!([
            {"type": "text", "text": "done"},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/shot.png"}}
        ])
    );
}

#[test]
fn reasoning_round_trips_and_leads_its_assistant_turn() {
    let block = reasoning::Thinking::Signed {
        thinking: "check the clamp".to_string(),
        signature: "sig-1".to_string(),
    };
    let out = translate(json!({
        "input": [
            {"type": "message", "role": "user", "content": "why?"},
            {"type": "reasoning", "summary": [],
             "encrypted_content": reasoning::encode_thinking(&block)},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "because"}
            ]}
        ]
    }));

    assert_eq!(
        out["messages"][1],
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "check the clamp", "signature": "sig-1"},
            {"type": "text", "text": "because"}
        ]})
    );
}

#[test]
fn foreign_reasoning_is_dropped() {
    let out = translate(json!({
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "reasoning", "summary": [], "encrypted_content": "gAAAAABnot-shunt"},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hello"}
            ]}
        ]
    }));

    assert_eq!(
        out["messages"][1]["content"],
        json!([{"type": "text", "text": "hello"}])
    );
}

#[test]
fn trailing_reasoning_without_assistant_content_is_dropped() {
    let block = reasoning::Thinking::Signed {
        thinking: "still thinking".to_string(),
        signature: "sig-2".to_string(),
    };
    let out = translate(json!({
        "input": [
            {"type": "message", "role": "user", "content": "go"},
            {"type": "reasoning", "summary": [],
             "encrypted_content": reasoning::encode_thinking(&block)}
        ]
    }));

    // Anthropic rejects a thinking-only assistant turn at the end of history.
    assert_eq!(out["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(out["messages"][0]["role"], "user");
}

#[test]
fn image_and_file_parts_translate_by_source() {
    let out = translate(json!({
        "input": [{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,QUJD"},
            {"type": "input_image", "image_url": "https://example.com/a.png"},
            {"type": "input_file", "file_data": "data:application/pdf;base64,UERG"},
            {"type": "input_file", "file_url": "https://example.com/a.pdf"},
            {"type": "input_image", "image_url": "file:///etc/passwd"}
        ]}]
    }));

    // The unrepresentable source is skipped rather than forwarded.
    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "UERG"}},
            {"type": "document", "source": {"type": "url", "url": "https://example.com/a.pdf"}}
        ])
    );
}

#[test]
fn tools_translate_and_built_ins_without_an_equivalent_are_dropped() {
    let out = translate(with_input(json!({
        "tools": [
            {"type": "function", "name": "bash", "description": "run",
             "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}},
             "strict": true},
            {"type": "function", "name": "noop"},
            {"type": "web_search_2025_08_26", "filters": {"allowed_domains": ["rust-lang.org"]},
             "user_location": {"type": "approximate", "country": "KR"}},
            {"type": "code_interpreter"}
        ]
    })));

    assert_eq!(
        out["tools"],
        json!([
            {"name": "bash", "description": "run",
             "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
            {"name": "noop", "input_schema": {"type": "object", "properties": {}}},
            {"type": "web_search_20250305", "name": "web_search",
             "allowed_domains": ["rust-lang.org"],
             "user_location": {"type": "approximate", "country": "KR"}}
        ])
    );
}

#[test]
fn tool_choice_variants_map_to_anthropic_shapes() {
    let function_tool = json!([{"type": "function", "name": "bash"}]);
    for (choice, expected) in [
        (json!("auto"), json!({"type": "auto"})),
        (json!("none"), json!({"type": "none"})),
        (json!("required"), json!({"type": "any"})),
        (
            json!({"type": "function", "name": "bash"}),
            json!({"type": "tool", "name": "bash"}),
        ),
        (
            json!({"type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "bash"}]}),
            json!({"type": "any"}),
        ),
    ] {
        let out = translate(with_input(
            json!({"tools": function_tool, "tool_choice": choice}),
        ));
        assert_eq!(out["tool_choice"], expected);
    }

    // No tools survive translation -> no tool_choice.
    let out = translate(with_input(json!({"tool_choice": "required"})));
    assert!(out.get("tool_choice").is_none());

    // parallel_tool_calls: false rides on the choice object, inventing an `auto`
    // one when the request named none.
    let out = translate(with_input(
        json!({"tools": function_tool, "parallel_tool_calls": false}),
    ));
    assert_eq!(
        out["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
}

#[test]
fn reasoning_effort_maps_to_a_thinking_budget() {
    for (effort, expected) in [
        (json!("none"), None),
        (json!("minimal"), None),
        (json!("low"), Some(2048)),
        (json!("medium"), Some(8192)),
        (json!("high"), Some(16384)),
        (json!("xhigh"), Some(32768)),
    ] {
        let out = translate(with_input(json!({
            "max_output_tokens": 64000,
            "reasoning": {"effort": effort, "summary": "auto"}
        })));
        assert_eq!(
            out.get("thinking")
                .and_then(|t| t["budget_tokens"].as_u64()),
            expected,
            "effort={effort}"
        );
    }

    // Absent reasoning asks for no thinking.
    let out = translate(with_input(json!({"max_output_tokens": 64000})));
    assert!(out.get("thinking").is_none());

    // The budget leaves room for the answer inside max_tokens...
    let out = translate(with_input(json!({
        "max_output_tokens": 4096, "reasoning": {"effort": "high"}
    })));
    assert_eq!(
        out["thinking"],
        json!({"type": "enabled", "budget_tokens": 3072})
    );

    // ...and drops out entirely when that leaves less than Anthropic's floor.
    let out = translate(with_input(json!({
        "max_output_tokens": 2000, "reasoning": {"effort": "high"}
    })));
    assert!(out.get("thinking").is_none());
}

#[test]
fn thinking_drops_the_sampling_parameters_anthropic_rejects() {
    let request = json!({
        "input": "hi", "temperature": 0.2, "top_p": 0.9,
        "metadata": {"user_id": "u-1"}
    });
    let out = translate(request.clone());
    assert_eq!(out["temperature"], 0.2);
    assert_eq!(out["top_p"], 0.9);
    assert_eq!(out["metadata"], json!({"user_id": "u-1"}));

    let mut thinking = request;
    thinking["reasoning"] = json!({"effort": "medium"});
    let out = translate(thinking);
    assert!(out["thinking"].is_object());
    assert!(out.get("temperature").is_none());
    assert!(out.get("top_p").is_none());
}

#[test]
fn an_allowed_tools_choice_narrows_the_forwarded_tools() {
    let request = |allowed: Value| {
        with_input(json!({
            "tools": [
                {"type": "function", "name": "bash"},
                {"type": "function", "name": "edit"},
            ],
            "tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": allowed},
        }))
    };

    let out = translate(request(json!([{"type": "function", "name": "edit"}])));
    assert_eq!(
        out["tools"],
        json!([{"name": "edit", "input_schema": {"type": "object", "properties": {}}}])
    );
    assert_eq!(out["tool_choice"], json!({"type": "auto"}));

    // Nothing declared is on the list -> neither tools nor tool_choice is sent.
    let out = translate(request(json!([{"type": "function", "name": "grep"}])));
    assert!(out.get("tools").is_none());
    assert!(out.get("tool_choice").is_none());

    // An explicitly empty list is the same restriction, not its absence.
    let out = translate(request(json!([])));
    assert!(out.get("tools").is_none());
    assert!(out.get("tool_choice").is_none());

    // So is a `tools` field that is missing or not a list: the allowlist fails
    // closed rather than forwarding everything.
    for allowed in [Value::Null, json!("bash")] {
        let out = translate(request(allowed));
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
    }
    let out = translate(with_input(json!({
        "tools": [{"type": "function", "name": "bash"}],
        "tool_choice": {"type": "allowed_tools", "mode": "auto"},
    })));
    assert!(out.get("tools").is_none());
    assert!(out.get("tool_choice").is_none());
}

#[test]
fn an_allowed_tools_choice_also_narrows_web_search() {
    let request = |allowed: Value| {
        with_input(json!({
            "tools": [{"type": "function", "name": "bash"}, {"type": "web_search"}],
            "tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": allowed},
        }))
    };
    let names = |out: &Value| -> Vec<String> {
        out["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    // A list of function names alone excludes the hosted web search.
    let out = translate(request(json!([{"type": "function", "name": "bash"}])));
    assert_eq!(names(&out), vec!["bash"]);

    // Listing the built-in admits it, under any of its Responses spellings.
    let out = translate(request(json!([{"type": "web_search_preview"}])));
    assert_eq!(names(&out), vec!["web_search"]);
    assert!(out["tools"][0]["type"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("web_search_")));
}

#[test]
fn a_file_url_with_an_unfetchable_scheme_is_dropped() {
    let out = translate(json!({
        "input": [{"type": "message", "role": "user", "content": [
            {"type": "input_file", "file_url": "file:///etc/passwd"},
            {"type": "input_file", "file_url": "https://example.com/a.pdf"}
        ]}]
    }));

    assert_eq!(
        out["messages"][0]["content"],
        json!([{"type": "document", "source": {"type": "url", "url": "https://example.com/a.pdf"}}])
    );
}

#[test]
fn a_forced_tool_choice_wins_over_thinking() {
    for choice in [
        json!("required"),
        json!({"type": "function", "name": "bash"}),
    ] {
        let out = translate(with_input(json!({
            "tools": [{"type": "function", "name": "bash"}],
            "tool_choice": choice,
            "reasoning": {"effort": "high"}
        })));
        assert!(out.get("thinking").is_none(), "choice={choice}");
        assert!(out["tool_choice"].is_object());
    }

    // An unforced choice keeps thinking.
    let out = translate(with_input(json!({
        "tools": [{"type": "function", "name": "bash"}],
        "tool_choice": "auto",
        "reasoning": {"effort": "high"}
    })));
    assert!(out["thinking"].is_object());
}

#[test]
fn text_format_json_schema_becomes_output_format() {
    let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
    let out = translate(with_input(json!({
        "text": {"format": {"type": "json_schema", "name": "verdict",
                            "schema": schema, "strict": true},
                 "verbosity": "low"}
    })));
    assert_eq!(
        out["output_format"],
        json!({"type": "json_schema", "schema": schema})
    );

    for format in [json!({"type": "json_object"}), json!({"type": "text"})] {
        let out = translate(with_input(json!({"text": {"format": format}})));
        assert!(out.get("output_format").is_none(), "format={format}");
    }
}

#[test]
fn max_output_tokens_maps_to_max_tokens_with_a_default() {
    let out = translate(with_input(
        json!({"max_output_tokens": 1234, "stream": true}),
    ));
    assert_eq!(out["max_tokens"], 1234);
    assert_eq!(out["stream"], true);

    let out = translate(with_input(json!({})));
    assert_eq!(out["max_tokens"], DEFAULT_MAX_TOKENS);
    assert!(out.get("stream").is_none());
}

#[test]
fn keys_anthropic_does_not_know_are_dropped() {
    let out = translate(json!({
        "model": "gpt-5.5-codex",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"},
            {"type": "some_future_item", "payload": "x"}
        ],
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "thread-1",
        "previous_response_id": "resp_1",
        "truncation": "auto",
        "service_tier": "priority",
        "safety_identifier": "id",
        "user": "u"
    }));

    for key in [
        "store",
        "include",
        "prompt_cache_key",
        "previous_response_id",
        "truncation",
        "service_tier",
        "safety_identifier",
        "user",
    ] {
        assert!(out.get(key).is_none(), "key={key}");
    }
    assert_eq!(out["messages"].as_array().expect("messages").len(), 1);
}

#[test]
fn a_non_object_body_is_rejected() {
    assert!(matches!(
        translate_request(&json!(["input"]), "claude-sonnet-4-5"),
        Err(TranslateError::NotAnObject)
    ));
}

#[test]
fn a_request_with_no_translatable_input_is_rejected() {
    for request in [
        json!({}),
        json!({"input": []}),
        json!({"input": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "  "}
        ]}]}),
        json!({"instructions": "system only", "input": [
            {"type": "message", "role": "developer", "content": "more system"}
        ]}),
    ] {
        assert!(
            matches!(
                translate_request(&request, "claude-sonnet-4-5"),
                Err(TranslateError::MissingInput)
            ),
            "request={request}"
        );
    }
}
