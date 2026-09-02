use super::*;

#[test]
fn translate_plain_text_user_message() {
    let input = json!({
        "messages": [
            { "role": "user", "content": "Hello Gemini!" }
        ]
    });

    let result = translate_request(&input).unwrap();
    assert!(result.get("thinkingConfig").is_none());
    assert_eq!(result["contents"][0]["role"], "user");
    assert_eq!(result["contents"][0]["parts"][0]["text"], "Hello Gemini!");
}

#[test]
fn translate_system_prompt_and_generation_config() {
    let input = json!({
        "system": "You are a Rust expert.",
        "temperature": 0.5,
        "max_tokens": 2048,
        "messages": [
            { "role": "user", "content": "Explain async/await." }
        ]
    });

    let result = translate_request(&input).unwrap();
    assert_eq!(
        result["systemInstruction"]["parts"][0]["text"],
        "You are a Rust expert."
    );
    assert_eq!(result["generationConfig"]["temperature"], 0.5);
    assert_eq!(result["generationConfig"]["maxOutputTokens"], 2048);
}

#[test]
fn translate_tools_and_tool_choice() {
    let input = json!({
        "messages": [
            { "role": "user", "content": "Check weather in Tokyo" }
        ],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get current weather",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    }
                }
            }
        ],
        "tool_choice": {
            "type": "tool",
            "name": "get_weather"
        }
    });

    let result = translate_request(&input).unwrap();
    let decls = &result["tools"][0]["functionDeclarations"];
    assert_eq!(decls[0]["name"], "get_weather");
    assert_eq!(result["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    assert_eq!(
        result["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
        "get_weather"
    );
}

#[test]
fn translate_extended_thinking() {
    let input = json!({
        "messages": [
            { "role": "user", "content": "Solve math puzzle" }
        ],
        "thinking": {
            "type": "enabled",
            "budget_tokens": 4096
        }
    });

    let result = translate_request(&input).unwrap();
    assert_eq!(
        result["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        4096
    );
    assert!(result.get("thinkingConfig").is_none());
}

#[test]
fn sanitize_tool_input_schema_removes_unsupported_keys() {
    let input = json!({
        "messages": [
            { "role": "user", "content": "Run tool" }
        ],
        "tools": [
            {
                "name": "sample_tool",
                "description": "Tool description",
                "input_schema": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {
                        "arg1": {
                            "type": "string",
                            "propertyNames": { "pattern": "^[a-z]+$" }
                        }
                    }
                }
            }
        ]
    });

    let result = translate_request(&input).unwrap();
    let params = &result["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(params.get("$schema").is_none());
    assert!(params["properties"]["arg1"].get("propertyNames").is_none());
}

#[test]
fn rejects_url_images_instead_of_dropping_them() {
    let input = json!({
        "messages": [{"role": "user", "content": [{
            "type": "image",
            "source": {"type": "url", "url": "https://example.com/image.png"}
        }]}]
    });

    let error = translate_request(&input).unwrap_err();
    assert!(error.message.contains("URL image sources"));
}

#[test]
fn preserves_tool_failure_signal() {
    let input = json!({
        "messages": [
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}
            }]},
            {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_1",
                "is_error": true, "content": "not found"
            }]}
        ]
    });

    let result = translate_request(&input).unwrap();
    let response = &result["contents"][1]["parts"][0]["functionResponse"]["response"];
    assert_eq!(response["error"], true);
    assert_eq!(response["output"], "not found");
}

#[test]
fn merges_system_message_between_tool_use_and_tool_result_into_user_turn() {
    // Claude Code's mid-conversation system message must not become a
    // standalone user turn between a functionCall and its functionResponse.
    let input = json!({
        "messages": [
            {"role": "user", "content": "read a.txt"},
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.txt"}
            }]},
            {"role": "system", "content": "<system-reminder>a.txt changed</system-reminder>"},
            {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_1", "content": "hello"
            }]}
        ]
    });

    let contents = translate_request(&input).unwrap()["contents"].clone();
    let roles: Vec<&str> = contents
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "model", "user"]);

    let parts = contents[2]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0]["text"],
        "<system-reminder>a.txt changed</system-reminder>"
    );
    assert_eq!(parts[1]["functionResponse"]["name"], "read_file");
    assert_eq!(parts[1]["functionResponse"]["response"]["output"], "hello");
}

#[test]
fn merges_consecutive_user_and_system_turns() {
    let input = json!({
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": [{"type": "text", "text": "date rolled over"}]},
            {"role": "user", "content": "continue"}
        ]
    });

    let contents = translate_request(&input).unwrap()["contents"].clone();
    assert_eq!(contents.as_array().unwrap().len(), 1);
    assert_eq!(contents[0]["role"], "user");
    let texts: Vec<&str> = contents[0]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["hi", "date rolled over", "continue"]);
}

#[test]
fn never_merges_consecutive_model_turns() {
    // Merging model turns would shift part indices and break the
    // thought-signature placement on the first functionCall of a turn.
    let input = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_1", "name": "a", "input": {}
            }]},
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_2", "name": "b", "input": {}
            }]}
        ]
    });

    let contents = translate_request(&input).unwrap()["contents"].clone();
    let roles: Vec<&str> = contents
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "model", "model"]);
    assert_eq!(
        contents[1]["parts"][0]["thoughtSignature"],
        GEMINI_THOUGHT_SIGNATURE_PLACEHOLDER
    );
    assert_eq!(
        contents[2]["parts"][0]["thoughtSignature"],
        GEMINI_THOUGHT_SIGNATURE_PLACEHOLDER
    );
}

#[test]
fn rejects_rich_media_tool_results() {
    let input = json!({
        "messages": [
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_1", "name": "inspect", "input": {}
            }]},
            {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_1", "content": [{
                    "type": "image", "source": {"type": "base64", "data": "AA=="}
                }]
            }]}
        ]
    });

    let error = translate_request(&input).unwrap_err();
    assert!(error.message.contains("rich media tool results"));
}

#[test]
fn rejects_excessively_nested_tool_schema() {
    let mut nested = json!({"type": "string"});
    for _ in 0..=MAX_SCHEMA_DEPTH {
        nested = json!({"items": nested});
    }
    let input = json!({
        "messages": [{"role": "user", "content": "run"}],
        "tools": [{"name": "deep", "input_schema": nested}]
    });

    let error = translate_request(&input).unwrap_err();
    assert!(error.message.contains("maximum nesting depth"));
}

#[test]
fn wrap_envelope_creates_code_assist_shape() {
    let inner = json!({ "contents": [] });
    let wrapped = wrap_code_assist_envelope("gemini-3-flash-preview", "test-proj-789", inner);

    assert_eq!(wrapped["model"], "gemini-3-flash-preview");
    assert_eq!(wrapped["project"], "test-proj-789");
    assert!(wrapped.get("request").is_some());
}

#[test]
fn the_code_assist_envelope_carries_none_of_the_agent_fields() {
    // The `gemini` provider talks to production Code Assist as the Gemini
    // CLI. Sending Antigravity's client identity there would misidentify
    // it, so these fields must never leak onto that path.
    let wrapped = wrap_code_assist_envelope(
        "gemini-3-flash-preview",
        "test-proj-789",
        json!({ "contents": [] }),
    );

    for field in ["userAgent", "requestType", "requestId"] {
        assert!(wrapped.get(field).is_none(), "{field} must not be sent");
    }
    assert!(wrapped["request"].get("sessionId").is_none());
}
