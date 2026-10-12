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
                .pointer("/output_config/format")
                .is_some_and(|format| !format.is_null()),
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
                if self.structured_output {
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
fn serves_explicit_effort(config: &Config, route: &Route) -> bool {
    config.provider(&route.provider).is_some_and(|provider| {
        provider.kind == ProviderKind::Antigravity
            && crate::model::antigravity_request::antigravity_model_needs_catalog(
                &route.upstream_model,
            )
    })
}

pub(super) fn filter_fallbacks(routes: &mut Vec<Route>, config: &Config, request: &Value) {
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
mod tests {
    use serde_json::json;

    use crate::config::{Config, ProviderKind};

    use super::*;

    fn route(provider: &str, adapter: AdapterKind) -> Route {
        route_model(provider, adapter, "upstream")
    }

    fn route_model(provider: &str, adapter: AdapterKind, upstream_model: &str) -> Route {
        Route {
            provider: provider.into(),
            adapter,
            model: "alias".into(),
            upstream_model: upstream_model.into(),
            effort: None,
            service_tier: None,
        }
    }

    fn config_with_custom_antigravity() -> Config {
        let mut config = Config::default();
        let native = config.provider("antigravity").expect("builtin").clone();
        config.providers.insert("backup".into(), native);
        let mut lookalike = config.provider("gemini").expect("builtin").clone();
        lookalike.kind = ProviderKind::Gemini;
        config
            .providers
            .insert("antigravity-lookalike".into(), lookalike);
        config
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
            Requirements::extract(&request),
            Requirements {
                tools: true,
                base64_images: true,
                url_images: true,
                tool_result_images: true,
                structured_output: true,
                explicit_effort: true,
            }
        );
        assert_eq!(
            Requirements::extract(&json!({"messages":"bad"})),
            Requirements::default()
        );
    }

    #[test]
    fn tool_choice_alone_drives_the_antigravity_cli_verdict() {
        let config = Config::default();
        let cli = route("antigravity-cli", AdapterKind::AntigravityCli);
        let forces = Requirements::extract(&json!({"tool_choice":{"type":"any"}}));
        assert_eq!(forces.incompatibilities(&config, &cli), vec!["tools"]);
        let named = Requirements::extract(&json!({"tool_choice":{"type":"tool","name":"lookup"}}));
        assert_eq!(named.incompatibilities(&config, &cli), vec!["tools"]);
        let declined = Requirements::extract(&json!({
            "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"none"}
        }));
        assert!(declined.incompatibilities(&config, &cli).is_empty());
        let auto = Requirements::extract(&json!({"tool_choice":{"type":"auto"}}));
        assert!(auto.incompatibilities(&config, &cli).is_empty());
    }

    #[test]
    fn eligibility_matrix_matches_existing_adapter_fidelity() {
        let config = config_with_custom_antigravity();
        let all = Requirements {
            tools: true,
            base64_images: true,
            url_images: true,
            tool_result_images: true,
            structured_output: true,
            explicit_effort: true,
        };
        assert!(all
            .incompatibilities(&config, &route("anthropic", AdapterKind::Anthropic))
            .is_empty());
        assert_eq!(
            all.incompatibilities(&config, &route("responses", AdapterKind::Responses)),
            vec!["structured-output"]
        );
        assert_eq!(
            all.incompatibilities(&config, &route("gemini", AdapterKind::Gemini)),
            vec![
                "url-images",
                "tool-result-images",
                "structured-output",
                "reasoning-effort"
            ]
        );
        assert_eq!(
            all.incompatibilities(
                &config,
                &route_model("antigravity", AdapterKind::Gemini, "gemini-2.5-pro")
            ),
            vec!["url-images", "tool-result-images", "structured-output"]
        );
        assert_eq!(
            all.incompatibilities(
                &config,
                &route_model("backup", AdapterKind::Gemini, "gemini-2.5-pro")
            ),
            vec!["url-images", "tool-result-images", "structured-output"]
        );
        assert_eq!(
            all.incompatibilities(
                &config,
                &route_model("antigravity", AdapterKind::Gemini, "claude-sonnet-4-6")
            ),
            vec![
                "url-images",
                "tool-result-images",
                "structured-output",
                "reasoning-effort"
            ]
        );
        assert_eq!(
            all.incompatibilities(
                &config,
                &route_model(
                    "antigravity-lookalike",
                    AdapterKind::Gemini,
                    "gemini-2.5-pro"
                )
            ),
            vec![
                "url-images",
                "tool-result-images",
                "structured-output",
                "reasoning-effort"
            ]
        );
        assert_eq!(
            all.incompatibilities(&config, &route("cursor", AdapterKind::Cursor)),
            vec!["url-images", "structured-output", "reasoning-effort"]
        );
        assert_eq!(
            all.incompatibilities(
                &config,
                &route("antigravity-cli", AdapterKind::AntigravityCli)
            ),
            vec!["tools", "images", "structured-output", "reasoning-effort"]
        );
    }

    #[test]
    fn native_antigravity_fallback_is_kept_for_explicit_effort() {
        let config = config_with_custom_antigravity();
        let mut routes = vec![
            route("primary", AdapterKind::Anthropic),
            route("gemini", AdapterKind::Gemini),
            route_model("antigravity", AdapterKind::Gemini, "gemini-2.5-pro"),
            route_model("backup", AdapterKind::Gemini, "gemini-2.5-flash"),
            route_model("antigravity", AdapterKind::Gemini, "claude-sonnet-4-6"),
            route_model(
                "antigravity-lookalike",
                AdapterKind::Gemini,
                "gemini-2.5-pro",
            ),
        ];
        filter_fallbacks(
            &mut routes,
            &config,
            &json!({"output_config":{"effort":"high"}}),
        );
        assert_eq!(
            routes
                .iter()
                .map(|route| route.upstream_model.as_str())
                .collect::<Vec<_>>(),
            vec!["upstream", "gemini-2.5-pro", "gemini-2.5-flash"]
        );
    }

    #[test]
    fn primary_is_preserved_and_only_incompatible_fallbacks_are_removed() {
        let config = Config::default();
        let mut routes = vec![
            route("primary", AdapterKind::AntigravityCli),
            route("responses", AdapterKind::Responses),
            route("anthropic", AdapterKind::Anthropic),
        ];
        filter_fallbacks(
            &mut routes,
            &config,
            &json!({"output_config":{"format":{"type":"json_schema"}}}),
        );
        assert_eq!(
            routes
                .iter()
                .map(|route| route.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["primary", "anthropic"]
        );
    }

    #[test]
    fn context_window_hint_does_not_shrink_the_chain() {
        let config = Config::default();
        let mut routes = vec![
            route("primary", AdapterKind::Anthropic),
            route("fallback", AdapterKind::Anthropic),
        ];
        filter_fallbacks(
            &mut routes,
            &config,
            &json!({"model":"alias[1m]","messages":[{"role":"user","content":"hello"}]}),
        );
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn ordinary_requests_preserve_the_entire_chain() {
        let config = Config::default();
        let mut routes = vec![
            route("primary", AdapterKind::Anthropic),
            route("responses", AdapterKind::Responses),
            route("gemini", AdapterKind::Gemini),
            route("cursor", AdapterKind::Cursor),
            route("antigravity-cli", AdapterKind::AntigravityCli),
        ];

        filter_fallbacks(
            &mut routes,
            &config,
            &json!({"messages":[{"role":"user","content":"hello"}]}),
        );

        assert_eq!(routes.len(), 5);
    }
}
