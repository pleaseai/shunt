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
fn a_tuple_array_schema_gets_the_items_gemini_requires() {
    // The shape that failed in the field: a `where` clause declared as
    // `[field, operator, value]` with `prefixItems` and no `items`. The
    // backend answers `…properties[where].items.items: missing field.` with a
    // 400 for the whole request, so the positions have to become the one
    // `items` schema Gemini reads — and that schema must itself carry a
    // `type`: `Schema.type` is REQUIRED on every node, so an `anyOf` with no
    // sibling type is the same missing-field failure one level down. The
    // positions agree on `string`, so `string` is what the element becomes;
    // the typeless "anything" slot contributes nothing.
    let input = json!({
        "messages": [{ "role": "user", "content": "Query" }],
        "tools": [{
            "name": "query",
            "input_schema": {
                "type": "object",
                "properties": {
                    "where": {
                        "type": "array",
                        "items": {
                            "type": "array",
                            "prefixItems": [
                                { "type": "string" },
                                { "type": "string", "enum": ["eq", "ne"] },
                                {}
                            ]
                        }
                    }
                }
            }
        }]
    });

    let result = translate_request(&input).unwrap();
    let clause = &result["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]
        ["where"]["items"];
    assert!(clause.get("prefixItems").is_none());
    assert_eq!(clause["items"], json!({ "type": "string" }));
}

#[test]
fn a_derived_items_schema_always_declares_a_type() {
    let sanitized = element_schema_sanitizer();

    // Positions that agree on nothing but their type keep that type.
    assert_eq!(
        sanitized(json!({
            "type": "array",
            "prefixItems": [{ "type": "string", "minLength": 1 }, { "type": "string" }]
        })),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // Positions that disagree on type keep the first one — a typeless union
    // would be rejected, and no single Gemini type admits both.
    assert_eq!(
        sanitized(
            json!({ "type": "array", "prefixItems": [{ "type": "number" }, { "type": "string" }] })
        ),
        json!({ "type": "array", "items": { "type": "number" } })
    );
    // An `anyOf`/`oneOf` position contributes its arms, not itself: forwarding
    // it would put a typeless node where Gemini requires a type.
    assert_eq!(
        sanitized(json!({
            "type": "array",
            "prefixItems": [{ "anyOf": [{ "type": "boolean" }, { "type": "boolean" }] }]
        })),
        json!({ "type": "array", "items": { "type": "boolean" } })
    );
    // An arm with no type of its own contributes nothing, so an `anyOf` of
    // typeless arms falls to the last resort rather than being forwarded.
    assert_eq!(
        sanitized(json!({
            "type": "array",
            "prefixItems": [{ "anyOf": [{ "description": "anything" }] }]
        })),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // A position constrained by an enum alone is not the "anything" slot: its
    // type is read off the enum instead of being dropped, so the other
    // positions cannot silently retype it. (The enum leads and the other
    // position is a number, so dropping the inference would change the answer.)
    assert_eq!(
        sanitized(json!({
            "type": "array",
            "prefixItems": [{ "enum": ["gt", "lt"] }, { "type": "number" }]
        })),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // A non-string enum is where the inference differs from the last resort.
    assert_eq!(
        sanitized(json!({ "type": "array", "prefixItems": [{ "enum": [1, 2] }] })),
        json!({ "type": "array", "items": { "type": "number" } })
    );
    // Two array positions of the same element type keep a complete branch:
    // collapsing them to a bare `array` would recreate the missing `items`.
    assert_eq!(
        sanitized(json!({
            "type": "array",
            "prefixItems": [
                { "type": "array", "items": { "type": "string" } },
                { "type": "array", "items": { "type": "number" } }
            ]
        })),
        json!({ "type": "array", "items": { "type": "array", "items": { "type": "string" } } })
    );
}

#[test]
fn a_type_list_becomes_a_scalar_type_and_nullable() {
    let sanitized = element_schema_sanitizer();

    // `Schema.type` is one enum value, with nullability in its own field.
    assert_eq!(
        sanitized(json!({ "type": ["string", "null"] })),
        json!({ "type": "string", "nullable": true })
    );
    // No `null` in the list: no `nullable` invented.
    assert_eq!(
        sanitized(json!({ "type": ["string", "number"] })),
        json!({ "type": "string" })
    );
    // A list naming nothing but `null` is that type.
    assert_eq!(
        sanitized(json!({ "type": ["null"] })),
        json!({ "type": "null" })
    );
}

#[test]
fn a_draft_07_tuple_is_folded_like_a_2020_12_one() {
    let sanitized = element_schema_sanitizer();

    // draft-04/07 spell a tuple as an array-valued `items`. That is not a
    // Gemini `Schema` either, so its presence must not read as "already answered".
    assert_eq!(
        sanitized(
            json!({ "type": "array", "items": [{ "type": "number" }, { "type": "number" }] })
        ),
        json!({ "type": "array", "items": { "type": "number" } })
    );
    // 2020-12 closes a tuple with `items: false`; a boolean is no schema either.
    assert_eq!(
        sanitized(
            json!({ "type": "array", "prefixItems": [{ "type": "boolean" }], "items": false })
        ),
        json!({ "type": "array", "items": { "type": "boolean" } })
    );
}

#[test]
fn a_tuple_position_is_sanitized_before_it_becomes_items() {
    let sanitized = element_schema_sanitizer();

    // The positions are folded in after the child walk, so an element schema
    // that is itself an array already carries its own `items` — otherwise the
    // 400 this fix exists to stop reappears one level down.
    assert_eq!(
        sanitized(json!({ "type": "array", "prefixItems": [{ "type": "array" }] })),
        json!({ "type": "array", "items": { "type": "array", "items": { "type": "string" } } })
    );
    // And two positions that differ only in a key the sanitizer strips are the
    // duplicates they look like once it has run — the shared constraint
    // survives, which folding before the strip would have collapsed away.
    assert_eq!(
        sanitized(json!({
            "type": "array",
            "prefixItems": [
                { "type": "string", "minLength": 1, "const": "asc" },
                { "type": "string", "minLength": 1, "const": "desc" }
            ]
        })),
        json!({ "type": "array", "items": { "type": "string", "minLength": 1 } })
    );
}

#[test]
fn instance_values_are_not_mistaken_for_schemas() {
    let sanitized = element_schema_sanitizer();

    // `default` holds an instance, not a schema. A tool that documents a
    // JSON-Schema-shaped default must get it back verbatim, not with an
    // `items` invented inside it.
    assert_eq!(
        sanitized(json!({ "type": "object", "default": { "type": "array" } })),
        json!({ "type": "object", "default": { "type": "array" } })
    );
    // Nor may a property named after a keyword be deleted or retyped.
    assert_eq!(
        sanitized(json!({
            "type": "object",
            "properties": { "prefixItems": { "type": "string" } }
        })),
        json!({
            "type": "object",
            "properties": { "prefixItems": { "type": "string" } }
        })
    );
    // A property named after an instance keyword is still a schema: it is
    // sanitized like any other, not skipped on account of its name.
    assert_eq!(
        sanitized(json!({
            "type": "object",
            "properties": {
                "enum": { "type": ["string", "null"] },
                "default": { "type": "array", "prefixItems": [{ "type": "number" }] },
                "const": { "type": "string", "const": "x" }
            }
        })),
        json!({
            "type": "object",
            "properties": {
                "enum": { "type": "string", "nullable": true },
                "default": { "type": "array", "items": { "type": "number" } },
                "const": { "type": "string" }
            }
        })
    );
}

/// Translate a request carrying one tool parameter and hand back the schema
/// that parameter reached the Gemini side as.
fn element_schema_sanitizer() -> impl Fn(Value) -> Value {
    |schema: Value| {
        let input = json!({
            "messages": [{ "role": "user", "content": "x" }],
            "tools": [{ "name": "t", "input_schema": {
                "type": "object",
                "properties": { "a": schema }
            } }]
        });
        translate_request(&input).unwrap()["tools"][0]["functionDeclarations"][0]["parameters"]
            ["properties"]["a"]
            .clone()
    }
}

#[test]
fn array_items_are_derived_only_when_something_is_missing() {
    let sanitized = element_schema_sanitizer();
    // A homogeneous tuple collapses to its one schema rather than a one-arm anyOf.
    assert_eq!(
        sanitized(
            json!({ "type": "array", "prefixItems": [{ "type": "number" }, { "type": "number" }] })
        ),
        json!({ "type": "array", "items": { "type": "number" } })
    );
    // An array that says nothing about its elements still gets an `items`.
    assert_eq!(
        sanitized(json!({ "type": "array" })),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // A tuple of only typeless slots falls to the same last resort.
    assert_eq!(
        sanitized(json!({ "type": "array", "prefixItems": [{}] })),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // `items` already present: `prefixItems` is dropped, `items` is kept as written.
    assert_eq!(
        sanitized(
            json!({ "type": "array", "prefixItems": [{ "type": "string" }], "items": { "type": "object" } })
        ),
        json!({ "type": "array", "items": { "type": "object" } })
    );
    // A tuple declared with `prefixItems` alone is an array in all but name:
    // it gains the type and the derived `items` instead of losing the tuple.
    assert_eq!(
        sanitized(json!({ "prefixItems": [{ "type": "string" }, { "type": "integer" }] })),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // A nullable array spelled as a type list is still an array — and the list
    // becomes the scalar type plus `nullable`, which is the only spelling
    // Gemini's `Schema.type` accepts.
    assert_eq!(
        sanitized(json!({ "type": ["array", "null"], "prefixItems": [{ "type": "boolean" }] })),
        json!({ "type": "array", "nullable": true, "items": { "type": "boolean" } })
    );
    // A non-array is untouched — no `items` is invented for an object.
    assert_eq!(
        sanitized(json!({ "type": "object", "properties": { "k": { "type": "string" } } })),
        json!({ "type": "object", "properties": { "k": { "type": "string" } } })
    );
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
