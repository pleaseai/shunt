//! Minimal capability gate for ordered heterogeneous fallback chains.
//!
//! The configured primary is always preserved. Only later candidates are
//! excluded, and only for request features whose current adapter behavior is
//! already known. This is deliberately not a general capability registry.

use serde_json::Value;

use crate::{
    config::{Config, ProviderKind},
    routing::{AdapterKind, Route},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Requirements {
    tools: bool,
    base64_images: bool,
    url_images: bool,
    tool_result_images: bool,
    structured_output: bool,
    /// `output_config.format.schema` is an object, the shape Responses
    /// translation copies into `text.format`.
    json_schema_object: bool,
    explicit_effort: bool,
}

impl Requirements {
    fn extract(request: &Value) -> Self {
        // Mirror `reject_caller_tools`: `tool_choice: none` asks for no tool
        // call even when `tools` is present, and `any`/`tool` oblige one even
        // when `tools` is absent. `auto` with no tools is the SDK default.
        let choice_type = request.pointer("/tool_choice/type").and_then(Value::as_str);
        let has_tools = request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        let mut requirements = Self {
            tools: choice_type != Some("none")
                && (matches!(choice_type, Some("any" | "tool")) || has_tools),
            structured_output: request
                .pointer("/output_config/format/type")
                .and_then(Value::as_str)
                == Some("json_schema"),
            json_schema_object: request
                .pointer("/output_config/format/schema")
                .is_some_and(Value::is_object),
            explicit_effort: request
                .pointer("/output_config/effort")
                .is_some_and(Value::is_string),
            ..Self::default()
        };
        let Some(messages) = request.get("messages").and_then(Value::as_array) else {
            return requirements;
        };
        let mut content = messages
            .iter()
            .filter_map(|message| message.get("content").map(|value| (value, false)))
            .collect::<Vec<_>>();
        while let Some((value, in_tool_result)) = content.pop() {
            let Some(blocks) = value.as_array() else {
                continue;
            };
            for block in blocks {
                let Some(block) = block.as_object() else {
                    continue;
                };
                match block.get("type").and_then(Value::as_str) {
                    Some("image") => {
                        if in_tool_result {
                            requirements.tool_result_images = true;
                        }
                        match block
                            .get("source")
                            .and_then(Value::as_object)
                            .and_then(|source| source.get("type"))
                            .and_then(Value::as_str)
                        {
                            Some("base64") => requirements.base64_images = true,
                            Some("url") => requirements.url_images = true,
                            _ => {}
                        }
                    }
                    Some("tool_result") => {
                        if let Some(nested) = block.get("content") {
                            content.push((nested, true));
                        }
                    }
                    _ => {}
                }
            }
        }
        requirements
    }

    fn incompatibilities(self, config: &Config, route: &Route) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        match route.adapter {
            AdapterKind::Anthropic | AdapterKind::Noop => {}
            AdapterKind::Responses => {
                if self.structured_output
                    && !responses_can_serve_json_schema(config, route, self.json_schema_object)
                {
                    reasons.push("structured-output");
                }
            }
            AdapterKind::Gemini => {
                if self.url_images {
                    reasons.push("url-images");
                }
                if self.tool_result_images {
                    reasons.push("tool-result-images");
                }
                if self.structured_output {
                    reasons.push("structured-output");
                }
                if self.explicit_effort && !serves_explicit_effort(config, route) {
                    reasons.push("reasoning-effort");
                }
            }
            AdapterKind::Cursor => {
                if self.url_images {
                    reasons.push("url-images");
                }
                if self.structured_output {
                    reasons.push("structured-output");
                }
                if self.explicit_effort {
                    reasons.push("reasoning-effort");
                }
            }
            AdapterKind::AntigravityCli => {
                if self.tools {
                    reasons.push("tools");
                }
                if self.base64_images || self.url_images {
                    reasons.push("images");
                }
                if self.structured_output {
                    reasons.push("structured-output");
                }
                if self.explicit_effort {
                    reasons.push("reasoning-effort");
                }
            }
        }
        reasons
    }
}

/// Native Antigravity maps `output_config.effort` onto a Gemini tier. Other
/// ids on that provider are forwarded untouched, and ordinary Gemini routes
/// do not read the field. The provider kind comes from config: `Route` only
/// stores `AdapterKind::Gemini` for both, and the upstream name is arbitrary.
/// OpenAI and ChatGPT translations copy an object schema into `text.format`.
/// A `json_schema` format without that object is not translated, and xAI and
/// Grok reject the `text` object entirely, so those routes cannot serve it.
fn responses_can_serve_json_schema(config: &Config, route: &Route, schema_object: bool) -> bool {
    schema_object
        && matches!(
            config.responses_flavor(&route.provider),
            crate::config::ResponsesFlavor::OpenAi | crate::config::ResponsesFlavor::Chatgpt
        )
}

fn serves_explicit_effort(config: &Config, route: &Route) -> bool {
    config.provider(&route.provider).is_some_and(|provider| {
        provider.kind == ProviderKind::Antigravity
            && crate::model::antigravity_request::antigravity_model_needs_catalog(
                &route.upstream_model,
            )
    })
}

pub(crate) fn filter_fallbacks(routes: &mut Vec<Route>, config: &Config, request: &Value) {
    if routes.len() < 2 {
        return;
    }
    let requirements = Requirements::extract(request);
    let fallbacks: Vec<Route> = routes.drain(1..).collect();
    for route in fallbacks {
        let reasons = requirements.incompatibilities(config, &route);
        if reasons.is_empty() {
            routes.push(route);
        } else {
            tracing::warn!(
                provider = %route.provider,
                model = %route.upstream_model,
                reasons = %reasons.join(","),
                "fallback excluded by request capabilities"
            );
            crate::metrics::record_failover(&route.provider, "capability_excluded");
        }
    }
}

#[cfg(test)]
#[path = "capability_tests.rs"]
mod tests;
