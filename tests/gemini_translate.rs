use serde_json::json;
use shunt::model::gemini::{map_gemini_error, translate_gemini_error_val, GeminiSseMachine};
use shunt::model::gemini_request::{
    translate_request, translate_request_for_model, wrap_code_assist_envelope,
};

#[test]
fn test_translate_plain_user_message() {
    let request = json!({
        "messages": [
            { "role": "user", "content": "Hello, Gemini!" }
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert!(translated.get("thinkingConfig").is_none());
    assert_eq!(translated["contents"][0]["role"], "user");
    assert_eq!(
        translated["contents"][0]["parts"][0]["text"],
        "Hello, Gemini!"
    );
}

#[test]
fn test_translate_system_and_tools() {
    let request = json!({
        "system": "System instructions for Gemini.",
        "messages": [
            { "role": "user", "content": "Fetch data" }
        ],
        "tools": [
            {
                "name": "fetch_data",
                "description": "Fetch data by key",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" }
                    }
                }
            }
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["systemInstruction"]["parts"][0]["text"],
        "System instructions for Gemini."
    );
    let tools = &translated["tools"][0]["functionDeclarations"];
    assert_eq!(tools[0]["name"], "fetch_data");
    assert_eq!(tools[0]["description"], "Fetch data by key");
}

#[test]
fn test_wrap_code_assist_envelope() {
    let inner = json!({ "contents": [] });
    let envelope = wrap_code_assist_envelope("gemini-3.1-pro-preview", "test-project-999", inner);

    assert_eq!(envelope["model"], "gemini-3.1-pro-preview");
    assert_eq!(envelope["project"], "test-project-999");
    assert!(envelope.get("request").is_some());
}

#[test]
fn test_gemini_sse_machine_text_and_finish() {
    let mut machine = GeminiSseMachine::new("gemini-3-flash-preview");

    let chunk = json!({
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello world!"}],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 3
        }
    });

    let mut events = machine.process_chunk_checked(&chunk).unwrap();
    events.extend(machine.transport_close_checked().unwrap());
    assert_eq!(events[0].event, "message_start");
    assert_eq!(events[0].data["message"]["model"], "gemini-3-flash-preview");
    assert_eq!(events[1].event, "content_block_start");
    assert_eq!(events[2].event, "content_block_delta");
    assert_eq!(events[2].data["delta"]["text"], "Hello world!");
    assert_eq!(events[3].event, "content_block_stop");
    assert_eq!(events[4].event, "message_delta");
    assert_eq!(events[4].data["delta"]["stop_reason"], "end_turn");
    assert_eq!(events[4].data["usage"]["output_tokens"], 3);
    assert_eq!(events[5].event, "message_stop");
}

#[test]
fn test_gemini_error_translation() {
    let err_val = json!({
        "code": 429,
        "message": "Resource has been exhausted (e.g. check quota).",
        "status": "RESOURCE_EXHAUSTED"
    });

    let err_env = translate_gemini_error_val(&err_val);
    assert_eq!(err_env["type"], "error");
    assert_eq!(err_env["error"]["type"], "rate_limit_error");

    let mapped = map_gemini_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &err_val.to_string());
    assert_eq!(
        mapped.message,
        "Resource has been exhausted (e.g. check quota)."
    );
}

#[test]
fn test_tool_result_uses_original_function_name() {
    let request = json!({
        "messages": [
            { "role": "user", "content": "Check the weather" },
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "get_weather",
                    "input": { "city": "Paris" }
                }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "sunny"
                }]
            }
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["contents"][2]["parts"][0]["functionResponse"]["name"],
        "get_weather"
    );
}

#[test]
fn test_gemini_3_thought_signature_survives_tool_round_trip() {
    let signature = "opaque-step-1-signature";
    let mut machine = GeminiSseMachine::new("gemini-3.1-pro-preview");
    let chunk = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": {
                        "name": "default_api:Read",
                        "args": { "file_path": "a.tex" }
                    },
                    "thoughtSignature": signature
                }]
            },
            "finishReason": "STOP"
        }]
    });

    let events = machine.process_chunk_checked(&chunk).unwrap();
    assert_eq!(events[1].event, "content_block_start");
    assert_eq!(events[1].data["content_block"]["type"], "tool_use");
    assert_eq!(events[1].data["index"], 0);
    assert!(events[1].data["content_block"]["id"]
        .as_str()
        .unwrap()
        .starts_with("call_gemini_v1_"));
    machine.transport_close_checked().unwrap();
    let assistant_content = machine.final_json_checked().unwrap()["content"].clone();
    let tool_use_id = assistant_content
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .and_then(|block| block["id"].as_str())
        .unwrap();
    let request = json!({
        "messages": [
            { "role": "user", "content": "Read a.tex" },
            { "role": "assistant", "content": assistant_content },
            { "role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": "file contents"
            }]}
        ]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["contents"][1]["parts"][0]["thoughtSignature"],
        signature
    );
}

#[test]
fn test_gemini_3_parallel_calls_keep_signature_on_first_call() {
    let request = json!({
        "messages": [{
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": "call_gemini_v1_cGFyYWxsZWwtc2lnbmF0dXJl",
                    "name": "read_file",
                    "input": { "path": "a.tex" }
                },
                {
                    "type": "tool_use",
                    "id": "toolu_parallel_unsigned",
                    "name": "read_file",
                    "input": { "path": "b.tex" }
                }
            ]
        }, {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "call_gemini_v1_cGFyYWxsZWwtc2lnbmF0dXJl", "content": "a"},
                {"type": "tool_result", "tool_use_id": "toolu_parallel_unsigned", "content": "b"}
            ]
        }]
    });

    let translated = translate_request_for_model(&request, "gemini-3.1-pro-preview").unwrap();
    let parts = translated["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(parts[0]["thoughtSignature"], "parallel-signature");
    assert!(parts[1].get("thoughtSignature").is_none());
}

#[test]
fn gemini_3_parallel_signature_policy_is_independent_of_sse_chunking() {
    let mut machine =
        GeminiSseMachine::new_streaming_for_upstream("claude-via-gemini", "gemini-3.1-pro-preview");
    machine
        .process_chunk_checked(&json!({"candidates": [{"content": {
            "role": "model", "parts": [{
                "functionCall": {"name": "first", "args": {}},
                "thoughtSignature": "authentic-first"
            }]
        }}]}))
        .unwrap();
    machine
        .process_chunk_checked(&json!({"candidates": [{"content": {
            "role": "model", "parts": [{
                "functionCall": {"name": "second", "args": {}}
            }]
        }, "finishReason": "STOP"}]}))
        .unwrap();
    machine.transport_close_checked().unwrap();
}

#[test]
fn test_gemini_3_sequential_steps_keep_distinct_signatures() {
    let request = json!({
        "messages": [
            { "role": "user", "content": "Read both files" },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "call_gemini_v1_c3RlcC0x", "name": "read_file", "input": { "path": "a.tex" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_gemini_v1_c3RlcC0x", "content": "a" }
            ]},
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "call_gemini_v1_c3RlcC0y", "name": "read_file", "input": { "path": "b.tex" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_gemini_v1_c3RlcC0y", "content": "b" }
            ]}
        ]
    });

    let translated = translate_request_for_model(&request, "gemini-3.1-pro-preview").unwrap();
    assert_eq!(
        translated["contents"][1]["parts"][0]["thoughtSignature"],
        "step-1"
    );
    assert_eq!(
        translated["contents"][3]["parts"][0]["thoughtSignature"],
        "step-2"
    );
}

#[test]
fn test_unsigned_gemini_3_history_is_rejected() {
    let request = json!({
        "messages": [{
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_imported",
                "name": "read_file",
                "input": { "path": "a.tex" }
            }]
        }]
    });

    let error = translate_request_for_model(&request, "gemini-3.1-pro-preview").unwrap_err();
    assert!(error.message.contains("authentic thought signature"));
}

#[test]
fn test_foreign_tool_use_id_is_rejected_for_gemini_3() {
    let request = json!({
        "messages": [{
            "role": "assistant",
            "content": [
                { "type": "tool_use", "id": "toolu_foreign", "name": "read_file", "input": {} }
            ]
        }]
    });

    let error = translate_request_for_model(&request, "gemini-3.1-pro-preview").unwrap_err();
    assert!(error.message.contains("authentic thought signature"));
}

#[test]
fn test_unsigned_gemini_2_5_history_stays_unsigned() {
    let request = json!({
        "messages": [{
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_legacy",
                "name": "read_file",
                "input": { "path": "a.tex" }
            }]
        }, {"role": "user", "content": [{
            "type": "tool_result", "tool_use_id": "toolu_legacy", "content": "ok"
        }]}]
    });

    let translated = translate_request_for_model(&request, "gemini-2.5-pro").unwrap();
    assert!(translated["contents"][0]["parts"][0]
        .get("thoughtSignature")
        .is_none());
}

#[test]
fn test_disabled_thinking_sends_zero_budget() {
    let request = json!({
        "thinking": { "type": "disabled" },
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let translated = translate_request(&request).unwrap();
    assert_eq!(
        translated["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        0
    );
    assert!(translated.get("thinkingConfig").is_none());
}

#[test]
fn test_gemini_sse_machine_finishes_on_eof() {
    let mut machine = GeminiSseMachine::new("gemini-3-flash-preview");
    let chunk = json!({
        "candidates": [{
            "content": { "parts": [{ "text": "partial" }], "role": "model" }
        }]
    });
    let _ = machine.process_chunk(&chunk);

    assert!(machine.transport_close_checked().is_err());
}

#[test]
fn test_gemini_sse_machine_non_streaming_accumulation() {
    let mut machine = GeminiSseMachine::new("gemini-2.5-pro");

    // Chunk 1: text part 1
    let chunk1 = json!({
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello "}],
                "role": "model"
            }
        }]
    });
    let _ = machine.process_chunk_checked(&chunk1).unwrap();

    // Chunk 2: text part 2 + tool_use
    let chunk2 = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {"text": "world!"},
                    {
                        "functionCall": {
                            "name": "read_file",
                            "args": { "path": "main.rs" }
                        }
                    }
                ],
                "role": "model"
            },
            "finishReason": "STOP"
        }]
    });
    let _ = machine.process_chunk_checked(&chunk2).unwrap();
    machine.transport_close_checked().unwrap();

    let final_json = machine.final_json();
    assert_eq!(final_json["type"], "message");
    assert_eq!(final_json["model"], "gemini-2.5-pro");
    assert_eq!(final_json["stop_reason"], "tool_use");

    let content = final_json["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "Hello world!");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["name"], "read_file");
    assert_eq!(content[1]["input"]["path"], "main.rs");
}

fn semantic_fixture() -> serde_json::Value {
    json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"thought": true, "text": "consider "},
                    {"text": "answer "},
                    {
                        "functionCall": {
                            "name": "read_file",
                            "args": {"path": "main.rs"}
                        },
                        "thoughtSignature": "authentic-signature"
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 11,
            "candidatesTokenCount": 7
        }
    })
}

#[test]
fn semantic_parity_direct_and_wrapped_stream_and_unary() {
    let direct = semantic_fixture();
    let wrapped = json!({"response": direct.clone()});

    let mut direct_stream = GeminiSseMachine::new_streaming("gemini-3.1-pro-preview");
    let mut wrapped_stream = GeminiSseMachine::new_streaming("gemini-3.1-pro-preview");
    let mut direct_events = direct_stream.process_chunk_checked(&direct).unwrap();
    let mut wrapped_events = wrapped_stream.process_chunk_checked(&wrapped).unwrap();
    direct_events.extend(direct_stream.transport_close_checked().unwrap());
    wrapped_events.extend(wrapped_stream.transport_close_checked().unwrap());

    let direct_kinds: Vec<_> = direct_events
        .iter()
        .map(|event| event.event.as_str())
        .collect();
    let wrapped_kinds: Vec<_> = wrapped_events
        .iter()
        .map(|event| event.event.as_str())
        .collect();
    assert_eq!(direct_kinds, wrapped_kinds);
    assert_eq!(direct_kinds.last(), Some(&"message_stop"));
    assert_eq!(
        direct_kinds
            .iter()
            .filter(|kind| **kind == "message_stop")
            .count(),
        1
    );
    let ordered_deltas: Vec<_> = direct_events
        .iter()
        .filter(|event| event.event == "content_block_delta")
        .map(|event| event.data["delta"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        ordered_deltas,
        ["thinking_delta", "text_delta", "input_json_delta"]
    );

    let mut unary = GeminiSseMachine::new("gemini-3.1-pro-preview");
    unary.process_chunk_checked(&wrapped).unwrap();
    unary.transport_close_checked().unwrap();
    let final_json = unary.final_json_checked().unwrap();
    assert_eq!(final_json["usage"]["input_tokens"], 11);
    assert_eq!(final_json["usage"]["output_tokens"], 7);
    assert_eq!(final_json["stop_reason"], "tool_use");
    assert_eq!(final_json["content"][0]["type"], "thinking");
    assert_eq!(final_json["content"][1]["type"], "text");
    assert_eq!(final_json["content"][2]["type"], "tool_use");
}

#[test]
fn semantic_parity_provider_error_never_becomes_success() {
    let mut machine = GeminiSseMachine::new("gemini-3.1-pro-preview");
    let events = machine
        .process_chunk_checked(&json!({
            "response": {"error": {
                "status": "RESOURCE_EXHAUSTED",
                "message": "quota exhausted"
            }}
        }))
        .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "error");
    assert!(machine.transport_close_checked().is_err());
    assert!(machine.final_json_checked().is_err());
}

#[test]
fn gemini_semantic_strictness_rejects_ambiguous_and_incomplete() {
    let invalid = [
        json!([]),
        json!({"response": "not-an-object"}),
        json!({"response": {"response": {}}}),
        json!({"response": {}, "candidates": []}),
        json!({"candidates": [{}, {}]}),
        json!({"candidates": ["not-an-object"]}),
        json!({"candidates": [{"content": "not-an-object"}]}),
        json!({"candidates": [{"content": {"parts": "not-an-array"}}]}),
        json!({"candidates": [{"content": {"role": "user", "parts": []}}]}),
        json!({"candidates": [{"content": {"parts": ["not-an-object"]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": 1}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"thought": "yes", "text": "x"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"thinking": "x"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "x", "functionCall": {}}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"functionCall": "bad"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": " ", "args": {}}}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "x", "args": []}}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "x"}, "thoughtSignature": ""}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"functionResponse": {"name": "x"}}]}}]}),
        json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]}),
        json!({"candidates": [{"finishReason": 1}]}),
        json!({"usageMetadata": []}),
        json!({"usageMetadata": {"promptTokenCount": -1}}),
        json!({"usageMetadata": {"candidatesTokenCount": 1.5}}),
        json!({"usageMetadata": {"totalTokenCount": "seven"}}),
    ];
    for value in invalid {
        let mut machine = GeminiSseMachine::new("gemini-3.1-pro-preview");
        assert!(
            machine.process_chunk_checked(&value).is_err(),
            "accepted invalid semantic shape: {value}"
        );
        assert!(machine.process_chunk_checked(&semantic_fixture()).is_err());
    }

    let mut no_finish = GeminiSseMachine::new("gemini-3.1-pro-preview");
    no_finish
        .process_chunk_checked(&json!({"usageMetadata": {"promptTokenCount": 1}}))
        .unwrap();
    assert!(no_finish.transport_close_checked().is_err());

    let mut duplicate = GeminiSseMachine::new("gemini-3.1-pro-preview");
    duplicate
        .process_chunk_checked(&semantic_fixture())
        .unwrap();
    assert!(duplicate
        .process_chunk_checked(&semantic_fixture())
        .is_err());

    let metadata_parts = vec![json!({"citationMetadata": {}}); 4_096];
    let mut bounded = GeminiSseMachine::new("gemini-2.5-pro");
    bounded
        .process_chunk_checked(&json!({
            "candidates": [{"content": {"role": "model", "parts": metadata_parts}}]
        }))
        .unwrap();
    assert!(bounded
        .process_chunk_checked(&json!({
            "candidates": [{"content": {"role": "model", "parts": [{}]}}]
        }))
        .is_err());
}

#[test]
fn gemini_known_part_strictness() {
    let unsupported = [
        json!({"inlineData": {"mimeType": "image/png", "data": "AA=="}}),
        json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}}),
        json!({"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "1"}}),
        json!({"fileData": {"mimeType": "text/plain", "fileUri": "gs://fixture"}}),
        json!({"functionResponse": {"name": "fixture", "response": {}}}),
        json!({"text": "visible", "inlineData": {"mimeType": "image/png", "data": "AA=="}}),
        json!({"thought": true, "text": "reasoning", "executableCode": {"language": "PYTHON", "code": "pass"}}),
        json!({"functionCall": {"name": "fixture", "args": {}}, "fileData": {"mimeType": "text/plain", "fileUri": "gs://fixture"}}),
        json!({"inlineData": {}, "codeExecutionResult": {}}),
    ];

    for part in unsupported {
        let value = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [part]},
                "finishReason": "STOP"
            }]
        });
        let mut streaming = GeminiSseMachine::new_streaming("gemini-2.5-pro");
        let stream_error = streaming
            .process_chunk_checked(&value)
            .expect_err("streaming accepted a known unsupported Gemini Part")
            .to_string();
        assert!(streaming.transport_close_checked().is_err());

        let mut unary = GeminiSseMachine::new("gemini-2.5-pro");
        let unary_error = unary
            .process_chunk_checked(&value)
            .expect_err("unary accepted a known unsupported Gemini Part")
            .to_string();
        assert_eq!(stream_error, unary_error);
        assert!(unary.final_json_checked().is_err());
    }

    for metadata in [
        json!({"citationMetadata": {"citations": []}}),
        json!({"futureProviderMetadata": {"opaque": true}}),
    ] {
        let mut machine = GeminiSseMachine::new_streaming("gemini-2.5-pro");
        assert!(machine
            .process_chunk_checked(&json!({
                "candidates": [{"content": {"role": "model", "parts": [metadata]}}]
            }))
            .is_ok());
    }
}

#[test]
fn gemini_semantic_strictness_accepts_signature_cap_and_rejects_cap_plus_one() {
    for (size, accepted) in [(64 * 1024, true), (64 * 1024 + 1, false)] {
        let mut machine = GeminiSseMachine::new("gemini-3.1-pro-preview");
        let result = machine.process_chunk_checked(&json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{
                    "functionCall": {"name": "bounded", "args": {}},
                    "thoughtSignature": "s".repeat(size)
                }]},
                "finishReason": "STOP"
            }]
        }));
        assert_eq!(result.is_ok(), accepted, "signature size {size}");
    }
}

#[test]
fn gemini_tool_signature_roundtrip_preserves_exact_parallel_identities() {
    let mut machine = GeminiSseMachine::new("gemini-3.1-pro-preview");
    machine
        .process_chunk_checked(&json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {"name": "read_file", "args": {"path": "a"}}, "thoughtSignature": "sig-a"},
                    {"functionCall": {"name": "read_file", "args": {"path": "b"}}}
                ]},
                "finishReason": "STOP"
            }]
        }))
        .unwrap();
    machine.transport_close_checked().unwrap();
    let assistant = machine.final_json_checked().unwrap()["content"].clone();
    let ids: Vec<_> = assistant
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["id"].as_str().unwrap().to_string())
        .collect();

    let translated = translate_request_for_model(
        &json!({"messages": [
            {"role": "assistant", "content": assistant},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": ids[0], "content": "A"},
                {"type": "tool_result", "tool_use_id": ids[1], "content": "B"}
            ]}
        ]}),
        "gemini-3.1-pro-preview",
    )
    .unwrap();
    assert_eq!(
        translated["contents"][0]["parts"][0]["thoughtSignature"],
        "sig-a"
    );
    assert!(translated["contents"][0]["parts"][1]
        .get("thoughtSignature")
        .is_none());
    assert_eq!(
        translated["contents"][1]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
    assert_eq!(
        translated["contents"][1]["parts"][1]["functionResponse"]["name"],
        "read_file"
    );
}

#[test]
fn gemini_parallel_tool_result_identity() {
    let first_id = "call_gemini_v1_c2lnLWE";
    let second_id = "toolu_unsigned_b";
    let assistant = json!({"role": "assistant", "content": [
        {"type": "tool_use", "id": first_id, "name": "first_tool", "input": {"n": 1}},
        {"type": "tool_use", "id": second_id, "name": "second_tool", "input": {"n": 2}}
    ]});
    let valid = json!({"messages": [
        assistant.clone(),
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": second_id, "content": "second", "is_error": true},
            {"type": "tool_result", "tool_use_id": first_id, "content": [{"type": "text", "text": "first"}]}
        ]}
    ]});

    let translated = translate_request_for_model(&valid, "gemini-3.1-pro-preview").unwrap();
    let responses = translated["contents"][1]["parts"].as_array().unwrap();
    assert_eq!(responses[0]["functionResponse"]["name"], "first_tool");
    assert_eq!(
        responses[0]["functionResponse"]["response"]["output"],
        "first"
    );
    assert!(responses[0]["functionResponse"]["response"]
        .get("error")
        .is_none());
    assert_eq!(responses[1]["functionResponse"]["name"], "second_tool");
    assert_eq!(
        responses[1]["functionResponse"]["response"]["output"],
        "second"
    );
    assert_eq!(responses[1]["functionResponse"]["response"]["error"], true);

    let invalid_result_batches = [
        json!([
            {"type": "tool_result", "tool_use_id": first_id, "content": "one"},
            {"type": "tool_result", "tool_use_id": first_id, "content": "duplicate"}
        ]),
        json!([
            {"type": "tool_result", "tool_use_id": first_id, "content": "missing second"}
        ]),
        json!([
            {"type": "tool_result", "tool_use_id": first_id, "content": "one"},
            {"type": "tool_result", "tool_use_id": "foreign", "content": "foreign"}
        ]),
        json!([
            {"type": "tool_result", "content": "missing id"},
            {"type": "tool_result", "tool_use_id": second_id, "content": "two"}
        ]),
    ];
    for results in invalid_result_batches {
        let request = json!({"messages": [
            assistant.clone(),
            {"role": "user", "content": results}
        ]});
        assert!(
            translate_request_for_model(&request, "gemini-3.1-pro-preview").is_err(),
            "accepted non-bijective tool result batch: {request}"
        );
    }

    let already_consumed = json!({"messages": [
        assistant,
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": first_id, "content": "one"},
            {"type": "tool_result", "tool_use_id": second_id, "content": "two"}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": first_id, "content": "again"}
        ]}
    ]});
    assert!(translate_request_for_model(&already_consumed, "gemini-3.1-pro-preview").is_err());
}

#[test]
fn gemini_tool_signature_roundtrip_rejects_invented_or_orphan_metadata() {
    let invalid = [
        json!({"messages": [{"role": "assistant", "content": [{
            "type": "tool_use", "id": "toolu_foreign", "name": "read_file", "input": {}
        }]}]}),
        json!({"messages": [{"role": "assistant", "content": [{
            "type": "tool_use", "id": "call_gemini_v1_not*base64", "name": "read_file", "input": {}
        }]}]}),
        json!({"messages": [{"role": "user", "content": [{
            "type": "tool_result", "tool_use_id": "orphan", "content": "x"
        }]}]}),
        json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_gemini_v1_c2ln", "name": "first", "input": {}},
                {"type": "tool_use", "id": "call_gemini_v1_c2ln", "name": "second", "input": {}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_gemini_v1_c2ln", "content": "x"}]}
        ]}),
    ];
    for request in invalid {
        assert!(
            translate_request_for_model(&request, "gemini-3.1-pro-preview").is_err(),
            "accepted invented or orphan signature metadata: {request}"
        );
    }
}

#[test]
fn gemini_alias_uses_upstream_model_for_signature_policy_and_public_model_for_output() {
    let unsigned_call = json!({"candidates": [{
        "content": {"role": "model", "parts": [{
            "functionCall": {"name": "read_file", "args": {}}
        }]},
        "finishReason": "STOP"
    }]});
    let mut strict =
        GeminiSseMachine::new_for_upstream("claude-via-gemini", "gemini-3.1-pro-preview");
    assert!(strict.process_chunk_checked(&unsigned_call).is_err());

    let mut legacy = GeminiSseMachine::new_for_upstream("gemini-3-looking-alias", "gemini-2.5-pro");
    legacy.process_chunk_checked(&unsigned_call).unwrap();
    legacy.transport_close_checked().unwrap();
    assert_eq!(
        legacy.final_json_checked().unwrap()["model"],
        "gemini-3-looking-alias"
    );
}

#[test]
fn late_usage_is_authoritative_in_the_stream_terminal_delta() {
    let mut stream = GeminiSseMachine::new_streaming("gemini-2.5-pro");
    let first = stream
        .process_chunk_checked(&json!({"candidates": [{
            "content": {"role": "model", "parts": [{"text": "hello"}]}
        }]}))
        .unwrap();
    assert_eq!(first[0].data["message"]["usage"]["input_tokens"], 0);

    stream
        .process_chunk_checked(&json!({
            "candidates": [{"finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 7}
        }))
        .unwrap();
    let terminal = stream.transport_close_checked().unwrap();
    let usage = &terminal
        .iter()
        .find(|event| event.event == "message_delta")
        .unwrap()
        .data["usage"];
    assert_eq!(usage, &json!({"input_tokens": 11, "output_tokens": 7}));
}

#[test]
fn compatibility_finish_helpers_are_valid_typed_and_idempotent() {
    let mut machine = GeminiSseMachine::new_streaming("gemini-2.5-pro");
    let mut events = Vec::new();
    machine.finish_stream("STOP", &mut events).unwrap();
    assert_eq!(events.first().unwrap().event, "message_start");
    assert_eq!(events.last().unwrap().event, "message_stop");
    let count = events.len();
    machine.finish_stream("STOP", &mut events).unwrap();
    machine.finish(&mut events);
    assert_eq!(events.len(), count);

    let mut invalid = GeminiSseMachine::new_streaming("gemini-2.5-pro");
    assert!(invalid.finish_stream("OTHER", &mut Vec::new()).is_err());
    let mut once = Vec::new();
    invalid.finish(&mut once);
    invalid.finish(&mut once);
    assert!(once.is_empty());

    let mut pending = GeminiSseMachine::new_streaming("gemini-2.5-pro");
    let prefix = pending
        .process_chunk_checked(&json!({"candidates": [{"finishReason": "STOP"}]}))
        .unwrap();
    assert_eq!(prefix.first().unwrap().event, "message_start");
    let mut terminal = Vec::new();
    pending.finish_stream("STOP", &mut terminal).unwrap();
    assert_eq!(terminal.last().unwrap().event, "message_stop");
    let terminal_count = terminal.len();
    pending.finish_stream("STOP", &mut terminal).unwrap();
    assert_eq!(terminal.len(), terminal_count);
}

#[test]
fn unary_adjacent_fragment_accumulation_stays_one_linear_string() {
    let parts: Vec<_> = (0..4_096).map(|_| json!({"text": "x"})).collect();
    let mut machine = GeminiSseMachine::new("gemini-2.5-pro");
    machine
        .process_chunk_checked(&json!({"candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": "STOP"
        }]}))
        .unwrap();
    machine.transport_close_checked().unwrap();
    let final_json = machine.final_json_checked().unwrap();
    let content = final_json["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"].as_str().unwrap().len(), 4_096);
}
