//! Strip Anthropic deferred-tool protocol on hosts that do not implement it.
//!
//! Claude Code, talking to shunt's Messages surface, marks MCP and catalog tools
//! with `defer_loading: true` (and may include a `tool_search_tool_*` search
//! tool). OpenRouter's Anthropic skin accepts that only for Anthropic models:
//! a stealth slug such as `stealth/ox-alpha` returns
//! `400 Deferred custom tools are only supported on Anthropic models…`.
//! First-party Anthropic and OpenRouter `anthropic/*` / `claude*` ids keep the
//! protocol byte-for-byte.

use serde_json::Value;

use crate::request::RequestBody;

/// Drop deferred-tool fields when `upstream_model` is not an Anthropic id.
pub(super) fn strip_unsupported_deferral(body: &mut RequestBody, upstream_model: &str) {
    if is_anthropic_model(upstream_model) {
        return;
    }
    if !request_needs_strip(body.json()) {
        return;
    }
    body.mutate(strip_deferral_fields);
}

fn is_anthropic_model(upstream_model: &str) -> bool {
    let model = upstream_model.trim_start_matches('~');
    let model = model.strip_prefix("anthropic/").unwrap_or(model);
    model.starts_with("claude")
}

fn request_needs_strip(request: &Value) -> bool {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.iter().any(tool_carries_deferral)
}

fn tool_carries_deferral(tool: &Value) -> bool {
    if tool.get("defer_loading").is_some() {
        return true;
    }
    if tool
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("tool_search_tool"))
    {
        return true;
    }
    if tool.pointer("/default_config/defer_loading").is_some() {
        return true;
    }
    tool.get("configs")
        .and_then(Value::as_object)
        .is_some_and(|configs| {
            configs
                .values()
                .any(|entry| entry.get("defer_loading").is_some())
        })
}

fn strip_deferral_fields(request: &mut Value) -> bool {
    let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) else {
        return false;
    };
    let before = tools.len();
    tools.retain(|tool| {
        !tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("tool_search_tool"))
    });
    let mut changed = tools.len() != before;
    for tool in tools.iter_mut() {
        let Some(object) = tool.as_object_mut() else {
            continue;
        };
        if object.remove("defer_loading").is_some() {
            changed = true;
        }
        if let Some(config) = object
            .get_mut("default_config")
            .and_then(Value::as_object_mut)
        {
            if config.remove("defer_loading").is_some() {
                changed = true;
            }
        }
        if let Some(configs) = object.get_mut("configs").and_then(Value::as_object_mut) {
            for entry in configs.values_mut() {
                if let Some(config) = entry.as_object_mut() {
                    if config.remove("defer_loading").is_some() {
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

#[cfg(test)]
fn apply(body: &[u8], upstream_model: &str) -> Vec<u8> {
    let mut request = RequestBody::parse(body.to_vec()).expect("fixture is JSON");
    strip_unsupported_deferral(&mut request, upstream_model);
    request.into_raw()
}

#[cfg(test)]
mod tests {
    use super::{apply, is_anthropic_model};
    use serde_json::{json, Value};

    fn parsed(raw: &[u8]) -> Value {
        serde_json::from_slice(raw).unwrap()
    }

    #[test]
    fn ox_alpha_loses_defer_loading() {
        let body = serde_json::to_vec(&json!({
            "model": "ox-alpha",
            "max_tokens": 16,
            "tools": [
                {
                    "name": "Bash",
                    "description": "run a command",
                    "input_schema": {"type": "object", "properties": {}},
                    "defer_loading": true
                },
                {
                    "name": "echo",
                    "description": "echo",
                    "input_schema": {"type": "object", "properties": {}}
                }
            ]
        }))
        .unwrap();

        let out = parsed(&apply(&body, "stealth/ox-alpha"));
        assert!(out["tools"][0].get("defer_loading").is_none());
        assert_eq!(out["tools"][0]["name"], "Bash");
        assert_eq!(out["tools"][1]["name"], "echo");
    }

    #[test]
    fn ox_alpha_drops_tool_search_tools() {
        let body = serde_json::to_vec(&json!({
            "model": "ox-alpha",
            "tools": [
                {
                    "type": "tool_search_tool_regex_20251119",
                    "name": "tool_search_tool_regex"
                },
                {
                    "name": "Bash",
                    "description": "run",
                    "input_schema": {"type": "object"},
                    "defer_loading": true
                }
            ]
        }))
        .unwrap();

        let out = parsed(&apply(&body, "stealth/ox-alpha"));
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["tools"][0]["name"], "Bash");
        assert!(out["tools"][0].get("defer_loading").is_none());
    }

    #[test]
    fn ox_alpha_strips_mcp_toolset_defer_loading() {
        let body = serde_json::to_vec(&json!({
            "model": "ox-alpha",
            "tools": [{
                "type": "mcp_toolset",
                "mcp_server_name": "docs",
                "default_config": {"defer_loading": true},
                "configs": {
                    "search": {"defer_loading": true}
                }
            }]
        }))
        .unwrap();

        let out = parsed(&apply(&body, "stealth/ox-alpha"));
        assert!(out["tools"][0]["default_config"]
            .get("defer_loading")
            .is_none());
        assert!(out["tools"][0]["configs"]["search"]
            .get("defer_loading")
            .is_none());
    }

    #[test]
    fn anthropic_openrouter_slug_keeps_defer_loading_bytes() {
        let body = serde_json::to_vec(&json!({
            "model": "anthropic/claude-opus-4.8",
            "tools": [{
                "name": "Bash",
                "input_schema": {"type": "object"},
                "defer_loading": true
            }]
        }))
        .unwrap();
        assert_eq!(apply(&body, "anthropic/claude-opus-4.8"), body);
    }

    #[test]
    fn first_party_claude_id_keeps_defer_loading_bytes() {
        let body = serde_json::to_vec(&json!({
            "model": "claude-sonnet-4-6",
            "tools": [{
                "name": "Bash",
                "input_schema": {"type": "object"},
                "defer_loading": true
            }]
        }))
        .unwrap();
        assert_eq!(apply(&body, "claude-sonnet-4-6"), body);
    }

    #[test]
    fn tilde_anthropic_alias_is_treated_as_anthropic() {
        assert!(is_anthropic_model("~anthropic/claude-sonnet-latest"));
        let body = serde_json::to_vec(&json!({
            "tools": [{"name": "Bash", "defer_loading": true}]
        }))
        .unwrap();
        assert_eq!(apply(&body, "~anthropic/claude-sonnet-latest"), body);
    }

    #[test]
    fn non_anthropic_kimi_slug_is_stripped() {
        let body = serde_json::to_vec(&json!({
            "tools": [{"name": "Bash", "defer_loading": true}]
        }))
        .unwrap();
        let out = parsed(&apply(&body, "k3"));
        assert!(out["tools"][0].get("defer_loading").is_none());
    }

    #[test]
    fn body_without_tools_is_untouched() {
        let body = br#"{"model":"stealth/ox-alpha","max_tokens":1}"#;
        assert_eq!(apply(body, "stealth/ox-alpha"), body);
    }

    #[test]
    fn already_eager_tools_are_untouched() {
        let body = serde_json::to_vec(&json!({
            "tools": [{
                "name": "echo",
                "input_schema": {"type": "object"}
            }]
        }))
        .unwrap();
        assert_eq!(apply(&body, "stealth/ox-alpha"), body);
    }
}
