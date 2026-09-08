use super::*;

#[test]
fn string_input_becomes_one_user_message() {
    let request = json!({"model": "gpt-5.6", "input": "hello"});

    let out = translate_request(&request, "qwen3-coder").unwrap();

    assert_eq!(out["model"], "qwen3-coder");
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": "hello"}])
    );
}

#[test]
fn instructions_lead_and_developer_items_stay_in_place() {
    let request = json!({
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "message", "role": "developer", "content": "be terse"},
            {"type": "message", "role": "user", "content": "again"},
        ]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(
        out["messages"],
        json!([
            {"role": "system", "content": "You are Codex."},
            {"role": "user", "content": "hi"},
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "again"},
        ])
    );
}

#[test]
fn text_only_parts_collapse_to_a_string() {
    let request = json!({
        "input": [{
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": "first"},
                {"type": "output_text", "text": "second"},
            ]
        }]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(out["messages"][0]["content"], "first\nsecond");
}

#[test]
fn mixed_parts_stay_an_array_and_carry_the_image() {
    let request = json!({
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "what is this"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAA", "detail": "low"},
            ]
        }]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type": "text", "text": "what is this"},
            {
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,AAA", "detail": "low"},
            },
        ])
    );
}

#[test]
fn input_file_becomes_an_openai_file_part() {
    let request = json!({
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "read it"},
                {
                    "type": "input_file",
                    "filename": "notes.pdf",
                    "file_data": "data:application/pdf;base64,QUJD",
                },
                {"type": "input_file", "file_id": "file_123"},
            ]
        }]
    });

    let out = translate_request(&request, "m").unwrap();

    // The id-referenced file has nothing to send and is dropped.
    assert_eq!(
        out["messages"][0]["content"][1],
        json!({
            "type": "file",
            "file": {"filename": "notes.pdf", "file_data": "data:application/pdf;base64,QUJD"},
        })
    );
    assert_eq!(out["messages"][0]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn function_calls_merge_into_the_preceding_assistant_message() {
    let request = json!({
        "input": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "on it"}]},
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"p\":1}"},
            {"type": "function_call", "call_id": "call_2", "name": "list", "arguments": "{}"},
        ]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(
        out["messages"],
        json!([{
            "role": "assistant",
            "content": "on it",
            "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"p\":1}"}},
                {"id": "call_2", "type": "function", "function": {"name": "list", "arguments": "{}"}},
            ]
        }])
    );
}

#[test]
fn a_function_call_after_a_user_turn_opens_its_own_assistant_message() {
    let request = json!({
        "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{}"},
        ]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(out["messages"][1]["role"], "assistant");
    assert_eq!(out["messages"][1]["tool_calls"][0]["id"], "call_1");
    assert!(out["messages"][1].get("content").is_none());
}

#[test]
fn function_call_output_becomes_a_tool_message() {
    let request = json!({
        "input": [
            {"type": "function_call_output", "call_id": "call_1", "output": "done"},
            {
                "type": "function_call_output",
                "call_id": "call_2",
                "output": [{"type": "output_text", "text": "a"}, {"type": "output_text", "text": "b"}],
            },
        ]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(
        out["messages"],
        json!([
            {"role": "tool", "tool_call_id": "call_1", "content": "done"},
            {"role": "tool", "tool_call_id": "call_2", "content": "a\nb"},
        ])
    );
}

#[test]
fn reasoning_and_unknown_items_are_dropped() {
    let request = json!({
        "input": [
            {"type": "reasoning", "id": "rs_1", "encrypted_content": "abc", "summary": []},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"},
            {"type": "something_new", "id": "x_1"},
            {"role": "user", "content": "hi"},
        ]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(out["messages"], json!([{"role": "user", "content": "hi"}]));
}

#[test]
fn function_tools_nest_and_built_ins_drop() {
    let request = json!({
        "input": "hi",
        "tools": [
            {
                "type": "function",
                "name": "read",
                "description": "read a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                "strict": true,
            },
            {"type": "function", "name": "ping"},
            {"type": "web_search"},
        ]
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(
        out["tools"],
        json!([
            {
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                    "strict": true,
                }
            },
            {
                "type": "function",
                "function": {"name": "ping", "parameters": {"type": "object", "properties": {}}}
            },
        ])
    );
}

#[test]
fn tool_choice_forms_map_and_need_tools() {
    let with_tools = |tool_choice: Value| {
        json!({
            "input": "hi",
            "tools": [{"type": "function", "name": "read"}],
            "tool_choice": tool_choice,
        })
    };

    let out = translate_request(&with_tools(json!("required")), "m").unwrap();
    assert_eq!(out["tool_choice"], "required");

    let out = translate_request(
        &with_tools(json!({"type": "function", "name": "read"})),
        "m",
    )
    .unwrap();
    assert_eq!(
        out["tool_choice"],
        json!({"type": "function", "function": {"name": "read"}})
    );

    let allowed = json!({"type": "allowed_tools", "mode": "auto", "tools": [{"type": "function", "name": "read"}]});
    let out = translate_request(&with_tools(allowed), "m").unwrap();
    assert_eq!(out["tool_choice"], "auto");

    // No tools survive -> neither tools nor tool_choice is sent.
    let toolless = json!({"input": "hi", "tools": [{"type": "web_search"}], "tool_choice": "auto"});
    let out = translate_request(&toolless, "m").unwrap();
    assert!(out.get("tools").is_none());
    assert!(out.get("tool_choice").is_none());
}

#[test]
fn an_allowed_tools_choice_narrows_the_forwarded_tools() {
    let request = |allowed: Value| {
        json!({
            "input": "hi",
            "tools": [
                {"type": "function", "name": "read"},
                {"type": "function", "name": "write"},
            ],
            "tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": allowed},
        })
    };

    let out = translate_request(
        &request(json!([{"type": "function", "name": "write"}])),
        "m",
    )
    .expect("request translates");
    assert_eq!(
        out["tools"],
        json!([{
            "type": "function",
            "function": {"name": "write", "parameters": {"type": "object", "properties": {}}}
        }])
    );
    assert_eq!(out["tool_choice"], "auto");

    // Nothing declared is on the list -> neither tools nor tool_choice is sent.
    let out = translate_request(&request(json!([{"type": "function", "name": "grep"}])), "m")
        .expect("request translates");
    assert!(out.get("tools").is_none());
    assert!(out.get("tool_choice").is_none());
}

#[test]
fn a_message_whose_parts_all_drop_is_not_sent() {
    let unrepresentable = json!({
        "role": "user",
        "content": [{"type": "input_file", "file_id": "f_1"}],
    });

    // Alone, it leaves the request with no conversation at all.
    let request = json!({"input": [unrepresentable.clone()]});
    assert!(matches!(
        translate_request(&request, "m"),
        Err(TranslateError::MissingInput)
    ));

    // Beside a message that does translate, only that message is emitted.
    let request = json!({
        "input": [
            unrepresentable,
            {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
        ]
    });
    let out = translate_request(&request, "m").expect("request translates");
    assert_eq!(out["messages"], json!([{"role": "user", "content": "hi"}]));
}

#[test]
fn text_format_maps_to_response_format() {
    let schema = json!({
        "input": "hi",
        "text": {"format": {
            "type": "json_schema",
            "name": "plan",
            "description": "a plan",
            "strict": true,
            "schema": {"type": "object"},
        }}
    });
    let out = translate_request(&schema, "m").unwrap();
    assert_eq!(
        out["response_format"],
        json!({"type": "json_schema", "json_schema": {
            "name": "plan",
            "schema": {"type": "object"},
            "strict": true,
            "description": "a plan",
        }})
    );

    let object = json!({"input": "hi", "text": {"format": {"type": "json_object"}}});
    let out = translate_request(&object, "m").unwrap();
    assert_eq!(out["response_format"], json!({"type": "json_object"}));

    let plain = json!({"input": "hi", "text": {"format": {"type": "text"}}});
    let out = translate_request(&plain, "m").unwrap();
    assert!(out.get("response_format").is_none());
}

#[test]
fn streaming_asks_for_usage_on_the_last_chunk() {
    let streaming = json!({"input": "hi", "stream": true});
    let out = translate_request(&streaming, "m").unwrap();
    assert_eq!(out["stream"], true);
    assert_eq!(out["stream_options"], json!({"include_usage": true}));

    let buffered = json!({"input": "hi", "stream": false});
    let out = translate_request(&buffered, "m").unwrap();
    assert!(out.get("stream").is_none());
    assert!(out.get("stream_options").is_none());
}

#[test]
fn sampling_and_reasoning_knobs_are_renamed() {
    let request = json!({
        "input": "hi",
        "max_output_tokens": 512,
        "temperature": 0.3,
        "top_p": 0.9,
        "parallel_tool_calls": false,
        "metadata": {"trace": "t1"},
        "safety_identifier": "user-7",
        "reasoning": {"effort": "none", "summary": "auto"},
        "text": {"verbosity": "low"},
    });

    let out = translate_request(&request, "m").unwrap();

    assert_eq!(out["max_completion_tokens"], 512);
    assert_eq!(out["temperature"], 0.3);
    assert_eq!(out["top_p"], 0.9);
    assert_eq!(out["parallel_tool_calls"], false);
    assert_eq!(out["metadata"], json!({"trace": "t1"}));
    assert_eq!(out["user"], "user-7");
    assert_eq!(out["reasoning_effort"], "none");
    assert_eq!(out["verbosity"], "low");
    assert!(out.get("max_output_tokens").is_none());
    assert!(out.get("reasoning").is_none());
    assert!(out.get("text").is_none());
}

#[test]
fn responses_only_keys_are_dropped() {
    let request = json!({
        "input": "hi",
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "shunt-1",
        "previous_response_id": "resp_1",
        "truncation": "auto",
        "service_tier": "priority",
        "background": false,
    });

    let out = translate_request(&request, "m").unwrap();

    for key in [
        "store",
        "include",
        "prompt_cache_key",
        "previous_response_id",
        "truncation",
        "service_tier",
        "background",
    ] {
        assert!(out.get(key).is_none(), "{key} should not be forwarded");
    }
}

#[test]
fn a_non_object_request_is_rejected() {
    let error = translate_request(&json!("hello"), "m").unwrap_err();
    assert!(matches!(error, TranslateError::NotAnObject));
}

#[test]
fn a_request_with_no_translatable_input_is_rejected() {
    let error = translate_request(&json!({"model": "gpt-5.6"}), "m").unwrap_err();
    assert!(matches!(error, TranslateError::MissingInput));

    let dropped_only = json!({"input": [{"type": "reasoning", "id": "rs_1"}]});
    let error = translate_request(&dropped_only, "m").unwrap_err();
    assert!(matches!(error, TranslateError::MissingInput));
}

#[test]
fn a_request_with_only_system_messages_is_rejected() {
    let instructions_only = json!({"instructions": "Be terse"});
    let error = translate_request(&instructions_only, "m").unwrap_err();
    assert!(matches!(error, TranslateError::MissingInput));

    let developer_only = json!({
        "instructions": "Be terse",
        "input": [{"type": "message", "role": "developer", "content": "Use JSON"}]
    });
    let error = translate_request(&developer_only, "m").unwrap_err();
    assert!(matches!(error, TranslateError::MissingInput));
}
