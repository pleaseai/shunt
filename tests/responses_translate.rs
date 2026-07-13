use axum::http::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::ResponsesFlavor,
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
    // provider "openai" is the stock Responses API (not the ChatGPT backend).
    translate_request(&body, &route("gpt-5.2-codex"), ResponsesFlavor::OpenAi).unwrap()
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
fn omits_max_output_tokens_for_chatgpt_backend() {
    // The ChatGPT/Codex backend rejects `max_output_tokens` ("Unsupported
    // parameter"), so translation must drop it when chatgpt_backend is true.
    let body = serde_json::to_vec(&json!({
        "model": "gpt-5.2-codex",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 1000
    }))
    .unwrap();

    let actual =
        translate_request(&body, &route("gpt-5.2-codex"), ResponsesFlavor::Chatgpt).unwrap();

    assert!(
        actual.get("max_output_tokens").is_none(),
        "max_output_tokens must not be sent to the ChatGPT backend: {actual}"
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
fn tool_reference_result_becomes_loaded_tool_text() {
    // Claude Code's ToolSearch (ENABLE_TOOL_SEARCH) returns tool_result content
    // made only of {type:"tool_reference", tool_name} blocks. Dropping them
    // yields an empty result that reads as "no tools found" — they must render
    // as text instead. The referenced tools' schemas arrive in the next
    // request's `tools` array, so text is all the model needs here.
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_ts", "name": "ToolSearch", "input": {"query": "select:Foo"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_ts", "content": [
                    {"type": "tool_reference", "tool_name": "mcp__github__get_me"},
                    {"type": "tool_reference", "tool_name": "mcp__slack__slack_send_message"}
                ]}
            ]}
        ]
    }));

    assert_eq!(
        actual["input"][1],
        json!({
            "type": "function_call_output",
            "call_id": "toolu_ts",
            "output": "Loaded tool: mcp__github__get_me\nLoaded tool: mcp__slack__slack_send_message"
        })
    );
}

#[test]
fn defer_loading_field_never_reaches_upstream_tools() {
    // With tool search enabled, discovered deferred tools carry
    // defer_loading:true. The Responses API doesn't know the field; the tools()
    // rebuild must emit only type/name/description/parameters. Mark the deferred
    // tool loaded so progressive filtering forwards it and this test stays focused
    // on stripping the unsupported field.
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_ts", "name": "ToolSearch", "input": {"query": "select:mcp__github__get_me"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_ts", "content": [
                    {"type": "tool_reference", "tool_name": "mcp__github__get_me"}
                ]}
            ]}
        ],
        "tools": [{
            "name": "mcp__github__get_me",
            "description": "Get the authenticated user",
            "input_schema": {"type": "object", "properties": {}},
            "defer_loading": true
        }]
    }));

    assert_eq!(
        actual["tools"],
        json!([{
            "type": "function",
            "name": "mcp__github__get_me",
            "description": "Get the authenticated user",
            "parameters": {"type": "object", "properties": {}, "additionalProperties": true}
        }])
    );
}

#[test]
fn deferred_unloaded_tool_is_filtered_from_upstream_tools() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [],
        "tools": [
            {
                "name": "deferred",
                "description": "Deferred tool",
                "input_schema": {"type": "object", "properties": {}},
                "defer_loading": true
            },
            {
                "name": "eager",
                "description": "Eager tool",
                "input_schema": {"type": "object", "properties": {}}
            }
        ]
    }));

    assert_eq!(actual["tools"].as_array().unwrap().len(), 1);
    assert_eq!(actual["tools"][0]["name"], "eager");
}

#[test]
fn loaded_deferred_tool_is_forwarded_and_revealed_with_schema() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_ts", "name": "ToolSearch", "input": {"query": "select:find_issue"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_ts", "content": [
                    {"type": "tool_reference", "tool_name": "find_issue"}
                ]}
            ]}
        ],
        "tools": [{
            "name": "find_issue",
            "description": "Find an issue",
            "input_schema": {
                "type": "object",
                "properties": {"number": {"type": "integer"}},
                "required": ["number"]
            },
            "defer_loading": true
        }]
    }));

    assert_eq!(actual["tools"].as_array().unwrap().len(), 1);
    assert_eq!(actual["tools"][0]["name"], "find_issue");
    assert!(actual["tools"][0].get("defer_loading").is_none());
    // The reveal carries the FULL input_schema — `required` included — not just
    // `properties`: a tool with mandatory parameters must not read as
    // all-optional at reveal time.
    let expected_schema = serde_json::to_string_pretty(&json!({
        "type": "object",
        "properties": {"number": {"type": "integer"}},
        "required": ["number"]
    }))
    .unwrap();
    assert_eq!(
        actual["input"][1]["output"],
        json!(format!(
            "Tool 'find_issue' is now available.\n\nDescription: Find an issue\n\nParameters:\n{expected_schema}"
        ))
    );
}

#[test]
fn forced_choice_on_unloaded_deferred_tool_downgrades_to_auto() {
    // A `tool` choice naming a deferred-and-unloaded tool would force a function
    // the filtered `tools` array no longer contains — the backend rejects a
    // choice for an unregistered function. Downgrade to `auto`, mirroring the
    // dropped-web-search case.
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [],
        "tools": [
            {
                "name": "deferred",
                "description": "Deferred tool",
                "input_schema": {"type": "object", "properties": {}},
                "defer_loading": true
            },
            {
                "name": "eager",
                "description": "Eager tool",
                "input_schema": {"type": "object", "properties": {}}
            }
        ],
        "tool_choice": {"type": "tool", "name": "deferred"}
    }));

    assert_eq!(actual["tools"].as_array().unwrap().len(), 1);
    assert_eq!(actual["tools"][0]["name"], "eager");
    assert_eq!(actual["tool_choice"], json!("auto"));
}

#[test]
fn forced_choice_on_loaded_deferred_tool_stays_named() {
    // Once a deferred tool is loaded via a tool_reference it is forwarded in
    // `tools`, so a forced choice for it remains a named function choice.
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_ts", "content": [
                {"type": "tool_reference", "tool_name": "deferred"}
            ]}
        ]}],
        "tools": [{
            "name": "deferred",
            "description": "Deferred tool",
            "input_schema": {"type": "object", "properties": {}},
            "defer_loading": true
        }],
        "tool_choice": {"type": "tool", "name": "deferred"}
    }));

    assert_eq!(actual["tools"].as_array().unwrap().len(), 1);
    assert_eq!(actual["tools"][0]["name"], "deferred");
    assert_eq!(
        actual["tool_choice"],
        json!({"type": "function", "name": "deferred"})
    );
}

#[test]
fn non_deferred_tool_is_forwarded_without_a_reference() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [],
        "tools": [{
            "name": "always_available",
            "description": "Always available",
            "input_schema": {"type": "object", "properties": {}}
        }]
    }));

    assert_eq!(actual["tools"][0]["name"], "always_available");
}

#[test]
fn all_deferred_unloaded_tools_omit_the_tools_field() {
    // When every tool is deferred-and-unloaded the translated set is empty. An
    // empty `tools: []` array is rejected by OpenAI-compatible backends
    // ("expected an array with at least one element"), so translate_request omits
    // the `tools` (and `tool_choice`) field entirely rather than emit `[]`.
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [],
        "tools": [{
            "name": "deferred",
            "description": "Deferred tool",
            "input_schema": {"type": "object", "properties": {}},
            "defer_loading": true
        }]
    }));

    assert!(actual.get("tools").is_none());
    assert!(actual.get("tool_choice").is_none());
}

#[test]
fn unknown_tool_reference_falls_back_to_loaded_tool_text() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_ts", "name": "ToolSearch", "input": {"query": "select:unknown"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_ts", "content": [
                    {"type": "tool_reference", "tool_name": "unknown"}
                ]}
            ]}
        ],
        "tools": [{
            "name": "different_tool",
            "description": "A different tool",
            "input_schema": {"type": "object", "properties": {}}
        }]
    }));

    assert_eq!(actual["input"][1]["output"], "Loaded tool: unknown");
    assert!(!actual["input"][1]["output"]
        .as_str()
        .unwrap()
        .contains("Parameters:"));
}

#[test]
fn mixed_tools_forward_loaded_deferred_and_non_deferred_only() {
    let actual = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_ts", "content": [
                {"type": "tool_reference", "tool_name": "loaded"}
            ]}
        ]}],
        "tools": [
            {
                "name": "loaded",
                "description": "Loaded deferred tool",
                "input_schema": {"type": "object", "properties": {}},
                "defer_loading": true
            },
            {
                "name": "unloaded",
                "description": "Unloaded deferred tool",
                "input_schema": {"type": "object", "properties": {}},
                "defer_loading": true
            },
            {
                "name": "eager",
                "description": "Eager tool",
                "input_schema": {"type": "object", "properties": {}}
            }
        ]
    }));

    let names = actual["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["loaded", "eager"]);
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

fn translate_with_flavor(input: Value, flavor: ResponsesFlavor) -> Value {
    let body = serde_json::to_vec(&input).unwrap();
    translate_request(&body, &route("gpt-5.2-codex"), flavor).unwrap()
}

#[test]
fn translates_hosted_web_search_tool_to_responses_web_search() {
    // Claude Code sends the hosted `web_search_20250305` tool when a user
    // enables web search. It must become the Responses hosted web-search tool,
    // not a phantom `function` the client can't execute.
    let base = json!({
        "model": "gpt-5.2-codex",
        "messages": [{"role": "user", "content": "find it"}],
        "tools": [
            {"name": "Bash", "input_schema": {}},
            {"type": "web_search_20250305", "name": "web_search"}
        ]
    });

    for flavor in [ResponsesFlavor::OpenAi, ResponsesFlavor::Chatgpt] {
        let out = translate_with_flavor(base.clone(), flavor);
        assert_eq!(
            out["tools"],
            json!([
                {
                    "type": "function",
                    "name": "Bash",
                    "description": "",
                    "parameters": {"type": "object", "properties": {}, "additionalProperties": true}
                },
                {
                    "type": "web_search",
                    "external_web_access": false,
                    "search_content_types": ["text", "image"]
                }
            ]),
            "flavor {flavor:?}"
        );
    }
}

#[test]
fn hosted_web_search_forwards_domain_filters() {
    let out = translate_with_flavor(
        json!({
            "model": "gpt-5.2-codex",
            "messages": [{"role": "user", "content": "find it"}],
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "allowed_domains": ["example.com"],
                "blocked_domains": ["spam.example"]
            }]
        }),
        ResponsesFlavor::Chatgpt,
    );
    assert_eq!(
        out["tools"],
        json!([{
            "type": "web_search",
            "external_web_access": false,
            "search_content_types": ["text", "image"],
            "filters": {
                "allowed_domains": ["example.com"],
                "blocked_domains": ["spam.example"]
            }
        }])
    );
}

#[test]
fn forced_web_search_tool_choice_uses_web_search_selector() {
    // A `tool` choice naming the hosted web-search tool selects it with a bare
    // `{"type":"web_search"}`, never a named `function` choice (which the
    // backend 502s on because no such function is registered).
    let out = translate_with_flavor(
        json!({
            "model": "gpt-5.2-codex",
            "messages": [{"role": "user", "content": "find it"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }),
        ResponsesFlavor::Chatgpt,
    );
    assert_eq!(out["tool_choice"], json!({"type": "web_search"}));
}

#[test]
fn grok_forwards_web_search_tool_and_forced_choice() {
    let out = translate_with_flavor(
        json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "find it"}],
            "tools": [
                {"name": "Bash", "input_schema": {}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }),
        ResponsesFlavor::Grok,
    );
    assert_eq!(
        out["tools"],
        json!([
            {
                "type": "function",
                "name": "Bash",
                "description": "",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": true}
            },
            {
                "type": "web_search",
                "external_web_access": false,
                "search_content_types": ["text", "image"]
            }
        ])
    );
    assert_eq!(out["tool_choice"], json!({"type": "web_search"}));
}

#[test]
fn xai_drops_web_search_tool_and_downgrades_forced_choice() {
    // xAI's Responses API only accepts function tools, so the hosted
    // web-search tool is dropped and a forced choice for it falls back to
    // `auto` rather than referencing a tool that was never registered.
    let out = translate_with_flavor(
        json!({
            "model": "gpt-5.2-codex",
            "messages": [{"role": "user", "content": "find it"}],
            "tools": [
                {"name": "Bash", "input_schema": {}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }),
        ResponsesFlavor::Xai,
    );
    assert_eq!(
        out["tools"],
        json!([{
            "type": "function",
            "name": "Bash",
            "description": "",
            "parameters": {"type": "object", "properties": {}, "additionalProperties": true}
        }])
    );
    assert_eq!(out["tool_choice"], json!("auto"));
}

#[test]
fn xai_web_search_only_tool_list_omits_tools_and_tool_choice() {
    // When the hosted web-search tool is the *only* tool and the route is xAI,
    // dropping it leaves an empty tool set. An empty `tools: []` array is
    // rejected by OpenAI-compatible backends ("expected an array with at least
    // one element"), so translation must omit both `tools` and `tool_choice`
    // entirely rather than emit an empty array.
    let out = translate_with_flavor(
        json!({
            "model": "gpt-5.2-codex",
            "messages": [{"role": "user", "content": "find it"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }),
        ResponsesFlavor::Xai,
    );
    assert!(
        out.get("tools").is_none(),
        "tools should be omitted, got {:?}",
        out.get("tools")
    );
    assert!(
        out.get("tool_choice").is_none(),
        "tool_choice should be omitted, got {:?}",
        out.get("tool_choice")
    );
}

#[test]
fn empty_tools_array_is_omitted_not_forwarded() {
    // An explicit empty `tools: []` from the client must not be forwarded as
    // `tools: []` (OpenAI-compatible backends reject it); it is omitted.
    let out = translate(json!({
        "model": "gpt-5.2-codex",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": []
    }));
    assert!(out.get("tools").is_none());
    assert!(out.get("tool_choice").is_none());
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
    let override_effort = translate_request(&body, &route, ResponsesFlavor::OpenAi).unwrap();
    assert_eq!(override_effort["reasoning"]["effort"], "xhigh");
}

fn xai_route(model: &str) -> Route {
    Route {
        provider: "xai".to_string(),
        adapter: AdapterKind::Responses,
        model: model.to_string(),
        upstream_model: model.to_string(),
        effort: None,
    }
}

#[test]
fn xai_omits_reasoning_and_text_without_configured_effort() {
    // Several grok models 400 on reasoning.effort, so with no route/provider
    // effort configured the xai flavor sends no `reasoning` object at all — and
    // no `text` object (xAI rejects it). store:false and stream:true remain.
    let body = serde_json::to_vec(&json!({
        "model": "grok-4.3",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 256
    }))
    .unwrap();

    let actual = translate_request(&body, &xai_route("grok-4.3"), ResponsesFlavor::Xai).unwrap();

    assert!(
        actual.get("reasoning").is_none(),
        "reasoning must be opt-in: {actual}"
    );
    assert!(
        actual.get("text").is_none(),
        "text object must be omitted for xai: {actual}"
    );
    assert_eq!(actual["store"], json!(false));
    assert_eq!(actual["stream"], json!(true));
    // xAI is not the ChatGPT backend, so the output cap is still forwarded.
    assert_eq!(actual["max_output_tokens"], json!(256));
    assert!(actual.get("service_tier").is_none());
}

#[test]
fn xai_honors_explicit_client_effort_without_route_config() {
    // A per-request `output_config.effort` is a deliberate client choice and
    // must not be silently dropped just because the route has no static effort.
    let body = serde_json::to_vec(&json!({
        "model": "grok-4.3",
        "messages": [],
        "output_config": {"effort": "high"}
    }))
    .unwrap();

    let actual = translate_request(&body, &xai_route("grok-4.3"), ResponsesFlavor::Xai).unwrap();

    assert_eq!(actual["reasoning"], json!({"effort": "high"}));

    // Derived defaults stay off: the extended-thinking flag alone must not
    // opt xai into reasoning (several grok models 400 on it).
    let body = serde_json::to_vec(&json!({
        "model": "grok-4.3",
        "messages": [],
        "thinking": {"type": "enabled", "budget_tokens": 1024}
    }))
    .unwrap();
    let actual = translate_request(&body, &xai_route("grok-4.3"), ResponsesFlavor::Xai).unwrap();
    assert!(actual.get("reasoning").is_none());
}

#[test]
fn xai_sends_reasoning_without_summary_when_effort_configured() {
    // With an explicit route/provider effort the reasoning dial is sent, but
    // without the `summary` key (xAI rejects it).
    let mut route = xai_route("grok-4.5");
    route.effort = Some("high".to_string());
    let body = serde_json::to_vec(&json!({"model": "grok-4.5", "messages": []})).unwrap();

    let actual = translate_request(&body, &route, ResponsesFlavor::Xai).unwrap();

    assert_eq!(actual["reasoning"], json!({"effort": "high"}));
}

#[test]
fn xai_includes_encrypted_reasoning_when_thinking_enabled() {
    let body = serde_json::to_vec(&json!({
        "thinking": {"type": "enabled"},
        "messages": [{"role": "user", "content": "hi"}]
    }))
    .unwrap();

    let actual = translate_request(&body, &xai_route("grok-4.5"), ResponsesFlavor::Xai).unwrap();

    assert_eq!(actual["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn translates_grok_web_search_results_and_citations_to_anthropic_blocks() {
    let fixture = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_search\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"Rust language\"},\"results\":[{\"type\":\"web_search_result\",\"url\":\"https://www.rust-lang.org/\",\"title\":\"Rust\",\"encrypted_content\":\"enc\"}]}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"message\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"Rust is a systems language.\"}\n\n",
        "event: response.output_text.annotation.added\n",
        "data: {\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://www.rust-lang.org/\",\"title\":\"Rust\",\"cited_text\":\"Rust is a systems language.\"}}\n\n",
        "event: response.output_text.done\n",
        "data: {}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n"
    );
    let mut machine = AnthropicSseMachine::new("grok-4.5", false);
    let emitted = parse_sse_events(fixture)
        .into_iter()
        .flat_map(|event| machine.apply(event))
        .collect::<String>();

    assert!(emitted.contains("\"type\":\"server_tool_use\""));
    assert!(emitted.contains("\"id\":\"ws_1\""));
    assert!(emitted.contains("\"type\":\"web_search_tool_result\""));
    assert!(emitted.contains("\"tool_use_id\":\"ws_1\""));
    assert!(emitted.contains("\"type\":\"citations_delta\""));
    assert!(emitted.contains("\"type\":\"web_search_result_location\""));
    assert!(emitted.contains("\"encrypted_index\":\"enc\""));
    assert!(emitted.contains("https://www.rust-lang.org/"));

    let final_json = machine.final_json();
    assert_eq!(final_json["content"][0]["type"], "server_tool_use");
    assert_eq!(final_json["content"][1]["type"], "web_search_tool_result");
    assert_eq!(final_json["content"][2]["type"], "text");
    assert_eq!(
        final_json["content"][2]["citations"][0]["encrypted_index"],
        "enc"
    );
    assert_eq!(final_json["stop_reason"], "end_turn");
}

#[test]
fn citation_without_open_text_block_opens_one_and_omits_missing_encrypted_index() {
    let fixture = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_search\"}}\n\n",
        "event: response.output_text.annotation.added\n",
        "data: {\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://example.com/\",\"title\":null,\"cited_text\":null}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{}}\n\n"
    );
    let mut machine = AnthropicSseMachine::new("grok-4.5", false);
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
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert!(!emitted.contains("encrypted_index"));

    let final_json = machine.final_json();
    assert_eq!(final_json["content"][0]["type"], "text");
    assert_eq!(final_json["content"][0]["text"], "");
    assert_eq!(
        final_json["content"][0]["citations"][0]["url"],
        "https://example.com/"
    );
    assert_eq!(final_json["content"][0]["citations"][0]["title"], "");
    assert_eq!(final_json["content"][0]["citations"][0]["cited_text"], "");
    assert!(final_json["content"][0]["citations"][0]
        .get("encrypted_index")
        .is_none());
}

#[test]
fn non_array_web_search_results_become_an_empty_result_array() {
    let fixture = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_search\"}}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"results\":null}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{}}\n\n"
    );
    let mut machine = AnthropicSseMachine::new("grok-4.5", false);
    let emitted = parse_sse_events(fixture)
        .into_iter()
        .flat_map(|event| machine.apply(event))
        .collect::<String>();

    assert!(emitted.contains("\"type\":\"web_search_tool_result\""));
    assert!(emitted.contains("\"content\":[]"));
    let final_json = machine.final_json();
    assert_eq!(final_json["content"][1]["content"], json!([]));
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

    // xAI shape: the reason is a top-level STRING `error` (e.g. a 402
    // out-of-credits body) — surfaced instead of the generic fallback.
    let xai = map_error_value(
        &json!({
            "code": "personal-team-blocked:spending-limit",
            "error": "You have run out of credits or need a Grok subscription. Add credits at https://grok.com/?_s=usage or upgrade at https://grok.com/supergrok."
        }),
        StatusCode::PAYMENT_REQUIRED,
    );
    assert_eq!(
        xai["error"]["message"],
        "You have run out of credits or need a Grok subscription. Add credits at https://grok.com/?_s=usage or upgrade at https://grok.com/supergrok."
    );

    // Unknown shape falls back to a generic message.
    let unknown = map_error_value(&json!({"weird": true}), StatusCode::BAD_GATEWAY);
    assert_eq!(unknown["error"]["message"], "upstream request failed");
}

#[test]
fn rewrites_context_overflow_errors_to_anthropic_wording() {
    // Claude Code's compact-and-retry matches "prompt is too long" and parses
    // "N tokens > M maximum" to size the retry; each upstream phrasing must
    // land on that shape with actual > limit regardless of the original order.

    // Chat-Completions phrasing: limit appears before the actual count.
    let chat = map_error_value(
        &json!({"error": {
            "code": "context_length_exceeded",
            "message": "This model's maximum context length is 272000 tokens. However, your messages resulted in 372982 tokens. Please reduce the length of the messages."
        }}),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(chat["error"]["type"], "invalid_request_error");
    assert_eq!(
        chat["error"]["message"],
        "prompt is too long: 372982 tokens > 272000 maximum"
    );

    // Gateway/proxy phrasing: actual count appears before the limit.
    let proxied = map_error_value(
        &json!({"error": {"message": "prompt token count of 372982 exceeds the limit of 272000 for model gpt-5.2"}}),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        proxied["error"]["message"],
        "prompt is too long: 372982 tokens > 272000 maximum"
    );

    // Responses API phrasing carries no token counts; the phrase alone still
    // triggers the client's compaction path.
    let responses = map_error_value(
        &json!({"error": {
            "code": "context_length_exceeded",
            "message": "Your input exceeds the context window of this model. Please adjust your input and try again."
        }}),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(responses["error"]["message"], "prompt is too long");

    // Comma-formatted counts still parse (digit-group separators, not delimiters).
    let grouped = map_error_value(
        &json!({"error": {"message": "This model's maximum context length is 272,000 tokens. However, your messages resulted in 372,982 tokens."}}),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        grouped["error"]["message"],
        "prompt is too long: 372982 tokens > 272000 maximum"
    );

    // Streaming `response.failed` events nest the error under "response".
    let failed = map_error_value(
        &json!({"type": "response.failed", "response": {"error": {
            "code": "context_length_exceeded",
            "message": "Your input exceeds the context window of this model."
        }}}),
        StatusCode::BAD_GATEWAY,
    );
    assert_eq!(failed["error"]["message"], "prompt is too long");

    // Non-overflow errors pass through untouched.
    let other = map_error_value(
        &json!({"error": {"code": "invalid_api_key", "message": "Incorrect API key provided: 1234567890"}}),
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        other["error"]["message"],
        "Incorrect API key provided: 1234567890"
    );

    // Quota/rate errors mention token limits too; they must NOT be rewritten.
    let quota = map_error_value(
        &json!({"error": {"code": "rate_limit_exceeded", "message": "Your request exceeds the limit of 1000000 tokens per minute."}}),
        StatusCode::TOO_MANY_REQUESTS,
    );
    assert_eq!(
        quota["error"]["message"],
        "Your request exceeds the limit of 1000000 tokens per minute."
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
