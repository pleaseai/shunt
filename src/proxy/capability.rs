//! Minimal capability gate for ordered heterogeneous fallback chains.
//!
//! The configured primary is always preserved. Only later candidates are
//! excluded, and only for request features whose current adapter behavior is
//! already known. This is deliberately not a general capability registry.

use serde_json::Value;

use crate::routing::{AdapterKind, Route};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Requirements {
    tools: bool,
    base64_images: bool,
    url_images: bool,
    structured_output: bool,
    explicit_effort: bool,
    million_context: bool,
}

impl Requirements {
    fn extract(request: &Value, requested_model: &str) -> Self {
        let mut requirements = Self {
            tools: request
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| !tools.is_empty()),
            structured_output: request
                .pointer("/output_config/format")
                .is_some_and(|format| !format.is_null()),
            explicit_effort: request
                .pointer("/output_config/effort")
                .is_some_and(Value::is_string),
            million_context: requested_model.ends_with("[1m]") || requested_model.ends_with("[1M]"),
            ..Self::default()
        };
        let Some(messages) = request.get("messages").and_then(Value::as_array) else {
            return requirements;
        };
        let mut content = messages
            .iter()
            .filter_map(|message| message.get("content"))
            .collect::<Vec<_>>();
        while let Some(value) = content.pop() {
            let Some(blocks) = value.as_array() else {
                continue;
            };
            for block in blocks {
                let Some(block) = block.as_object() else {
                    continue;
                };
                match block.get("type").and_then(Value::as_str) {
                    Some("image") => match block
                        .get("source")
                        .and_then(Value::as_object)
                        .and_then(|source| source.get("type"))
                        .and_then(Value::as_str)
                    {
                        Some("base64") => requirements.base64_images = true,
                        Some("url") => requirements.url_images = true,
                        _ => {}
                    },
                    Some("tool_result") => {
                        if let Some(nested) = block.get("content") {
                            content.push(nested);
                        }
                    }
                    _ => {}
                }
            }
        }
        requirements
    }

    fn incompatibilities(self, adapter: &AdapterKind) -> Vec<&'static str> {
        if self.million_context {
            return vec!["1m-context-unknown"];
        }
        let mut reasons = Vec::new();
        match adapter {
            AdapterKind::Anthropic => {}
            AdapterKind::Responses => {
                if self.structured_output {
                    reasons.push("structured-output");
                }
            }
            AdapterKind::Gemini | AdapterKind::Cursor => {
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

pub(super) fn filter_fallbacks(routes: &mut Vec<Route>, request: &Value, requested_model: &str) {
    if routes.len() < 2 {
        return;
    }
    let requirements = Requirements::extract(request, requested_model);
    let mut index = 1;
    while index < routes.len() {
        let reasons = requirements.incompatibilities(&routes[index].adapter);
        if reasons.is_empty() {
            index += 1;
            continue;
        }
        let route = routes.remove(index);
        tracing::warn!(
            provider = %route.provider,
            model = %route.upstream_model,
            reasons = %reasons.join(","),
            "fallback excluded by request capabilities"
        );
        crate::metrics::record_failover(&route.provider, "capability_excluded");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn route(provider: &str, adapter: AdapterKind) -> Route {
        Route {
            provider: provider.into(),
            adapter,
            model: "alias".into(),
            upstream_model: "upstream".into(),
            effort: None,
            service_tier: None,
        }
    }

    #[test]
    fn extract_detects_known_requirements_without_recursive_schema_scanning() {
        let request = json!({
            "tools":[{"name":"lookup","input_schema":{"type":"object","properties":{"fake":{"type":"image"}}}}],
            "output_config":{"format":{"type":"json_schema"},"effort":"high"},
            "messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"url","url":"https://example.com/a.png"}},
                {"type":"tool_result","tool_use_id":"x","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}
                ]}
            ]}]
        });
        assert_eq!(
            Requirements::extract(&request, "alias[1M]"),
            Requirements {
                tools: true,
                base64_images: true,
                url_images: true,
                structured_output: true,
                explicit_effort: true,
                million_context: true,
            }
        );
        assert_eq!(
            Requirements::extract(&json!({"messages":"bad"}), "alias"),
            Requirements::default()
        );
    }

    #[test]
    fn eligibility_matrix_matches_existing_adapter_fidelity() {
        let all = Requirements {
            tools: true,
            base64_images: true,
            url_images: true,
            structured_output: true,
            explicit_effort: true,
            million_context: false,
        };
        assert!(all.incompatibilities(&AdapterKind::Anthropic).is_empty());
        assert_eq!(
            all.incompatibilities(&AdapterKind::Responses),
            vec!["structured-output"]
        );
        assert_eq!(
            all.incompatibilities(&AdapterKind::Gemini),
            vec!["url-images", "structured-output", "reasoning-effort"]
        );
        assert_eq!(
            all.incompatibilities(&AdapterKind::Cursor),
            vec!["url-images", "structured-output", "reasoning-effort"]
        );
        assert_eq!(
            all.incompatibilities(&AdapterKind::AntigravityCli),
            vec!["tools", "images", "structured-output", "reasoning-effort"]
        );
    }

    #[test]
    fn primary_is_preserved_and_only_incompatible_fallbacks_are_removed() {
        let mut routes = vec![
            route("primary", AdapterKind::AntigravityCli),
            route("responses", AdapterKind::Responses),
            route("anthropic", AdapterKind::Anthropic),
        ];
        filter_fallbacks(
            &mut routes,
            &json!({"output_config":{"format":{"type":"json_schema"}}}),
            "alias",
        );
        assert_eq!(
            routes
                .iter()
                .map(|route| route.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["primary", "anthropic"]
        );

        filter_fallbacks(&mut routes, &json!({}), "alias[1m]");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].provider, "primary");
    }

    #[test]
    fn ordinary_requests_preserve_the_entire_chain() {
        let mut routes = vec![
            route("primary", AdapterKind::Anthropic),
            route("responses", AdapterKind::Responses),
            route("gemini", AdapterKind::Gemini),
            route("cursor", AdapterKind::Cursor),
            route("antigravity-cli", AdapterKind::AntigravityCli),
        ];

        filter_fallbacks(
            &mut routes,
            &json!({"messages":[{"role":"user","content":"hello"}]}),
            "alias",
        );

        assert_eq!(routes.len(), 5);
    }
}
