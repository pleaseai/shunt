
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
            json_schema_object: false,
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
        json_schema_object: false,
        explicit_effort: true,
    };
    assert!(all
        .incompatibilities(&config, &route("anthropic", AdapterKind::Anthropic))
        .is_empty());
    // No object schema, so the Responses translator does not emit text.format.
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

#[test]
fn openai_responses_fallback_keeps_a_translatable_json_schema() {
    let config = Config::default();
    let mut routes = vec![
        route("primary", AdapterKind::Anthropic),
        route("responses", AdapterKind::Responses),
        route("gemini", AdapterKind::Gemini),
    ];
    filter_fallbacks(
        &mut routes,
        &config,
        &json!({
            "output_config": {
                "format": {"type": "json_schema", "schema": {"type": "object"}}
            }
        }),
    );
    assert_eq!(
        routes
            .iter()
            .map(|route| route.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["primary", "responses"]
    );
}

#[test]
fn xai_responses_fallback_cannot_carry_a_json_schema() {
    let mut config = Config::default();
    let mut xai = config.provider("anthropic").expect("builtin").clone();
    xai.base_url = "https://api.x.ai/v1".to_string();
    config.providers.insert("xai".into(), xai);
    let mut routes = vec![
        route("primary", AdapterKind::Anthropic),
        route("xai", AdapterKind::Responses),
        route("responses", AdapterKind::Responses),
    ];
    filter_fallbacks(
        &mut routes,
        &config,
        &json!({
            "output_config": {
                "format": {"type": "json_schema", "schema": {"type": "object"}}
            }
        }),
    );
    assert_eq!(
        routes
            .iter()
            .map(|route| route.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["primary", "responses"]
    );
}

#[test]
fn text_format_does_not_exclude_responses() {
    let config = Config::default();
    let mut routes = vec![
        route("primary", AdapterKind::Anthropic),
        route("responses", AdapterKind::Responses),
    ];
    filter_fallbacks(
        &mut routes,
        &config,
        &json!({"output_config":{"format":{"type":"text"}}}),
    );
    assert_eq!(routes.len(), 2);
}
