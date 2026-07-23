use axum::http::StatusCode;
use serde::Deserialize;

use crate::{
    config::{Config, ProviderKind},
    error::ShuntError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterKind {
    Anthropic,
    Responses,
    Cursor,
    Gemini,
    Antigravity,
}

impl From<ProviderKind> for AdapterKind {
    fn from(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Anthropic => AdapterKind::Anthropic,
            ProviderKind::Responses => AdapterKind::Responses,
            ProviderKind::Cursor => AdapterKind::Cursor,
            ProviderKind::Gemini => AdapterKind::Gemini,
            ProviderKind::Antigravity => AdapterKind::Antigravity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub provider: String,
    pub adapter: AdapterKind,
    pub model: String,
    pub upstream_model: String,
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RoutingView {
    model: String,
}

pub fn resolve(config: &Config, body: &[u8]) -> Result<Route, ShuntError> {
    resolve_request(config, body).map(|(route, _)| route)
}

pub(crate) fn resolve_request(config: &Config, body: &[u8]) -> Result<(Route, String), ShuntError> {
    resolve_request_chain(config, body).map(|(routes, model)| {
        (
            routes
                .into_iter()
                .next()
                .expect("route chains are non-empty"),
            model,
        )
    })
}

pub(crate) fn resolve_request_chain(
    config: &Config,
    body: &[u8],
) -> Result<(Vec<Route>, String), ShuntError> {
    let view: RoutingView = serde_json::from_slice(body).map_err(|error| {
        ShuntError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("request body must include a JSON model field: {error}"),
        )
    })?;
    let routes = resolve_model_chain(config, &view.model);
    Ok((routes, view.model))
}

/// Claude Code appends a `[1m]` suffix to a model id as a *client-side* hint that
/// raises its own context-window / auto-compact threshold (see `docs/running.md`
/// §5). The suffix is not part of the real model name: upstream `responses`
/// providers (Codex/OpenAI) reject a `gpt-5.6-sol[1m]` slug, and an explicit
/// `[[routes]]` entry would never match it. Strip a single trailing `[1m]`
/// (ASCII case-insensitive) before route matching and before forwarding upstream
/// so the documented `[1m]` lever works through the gateway. `strip_suffix`
/// operates on char boundaries, so this stays panic-free on non-ASCII ids.
pub(crate) fn strip_context_window_hint(model: &str) -> &str {
    model
        .strip_suffix("[1m]")
        .or_else(|| model.strip_suffix("[1M]"))
        .unwrap_or(model)
}

pub fn resolve_model(config: &Config, model: &str) -> Route {
    resolve_model_chain(config, model)
        .into_iter()
        .next()
        .expect("route chains are non-empty")
}

pub fn resolve_model_chain(config: &Config, model: &str) -> Vec<Route> {
    let model = strip_context_window_hint(model);
    for configured_model in &config.models {
        if configured_model.id == model {
            if let Some(upstream_models) = configured_model.upstream_model.as_ref() {
                // Preserve the legacy single-map path even for a Config assembled
                // directly in code without validation refreshing derived order.
                if let Some((provider, upstream_model)) = (upstream_models.len() == 1)
                    .then(|| upstream_models.iter().next())
                    .flatten()
                {
                    return vec![route_for(config, provider, model, upstream_model, None)];
                }
                let routes = config
                    .upstream_order
                    .iter()
                    .filter_map(|provider| {
                        upstream_models.get(provider).map(|upstream_model| {
                            route_for(config, provider, model, upstream_model, None)
                        })
                    })
                    .collect::<Vec<_>>();
                if !routes.is_empty() {
                    return routes;
                }
            }
        }
    }
    for route in &config.routes {
        if route.model == model {
            return vec![route_for(
                config,
                &route.provider,
                model,
                route.upstream_model.as_deref().unwrap_or(model),
                route.effort.clone(),
            )];
        }
    }
    for route in &config.route_prefixes {
        if model.starts_with(&route.prefix) {
            return vec![route_for(config, &route.provider, model, model, None)];
        }
    }
    vec![route_for(
        config,
        &config.server.default_provider,
        model,
        model,
        None,
    )]
}

fn route_for(
    config: &Config,
    provider: &str,
    model: &str,
    upstream_model: &str,
    effort: Option<String>,
) -> Route {
    // The provider's declared kind picks the adapter; unknown names (only
    // reachable via a validated default) fall back to the Anthropic passthrough.
    let provider_config = config.provider(provider);
    let adapter = provider_config
        .map(|p| AdapterKind::from(p.kind))
        .unwrap_or(AdapterKind::Anthropic);
    let effort = effort.or_else(|| provider_config.and_then(|p| p.effort.clone()));
    Route {
        provider: provider.to_string(),
        adapter,
        model: model.to_string(),
        upstream_model: upstream_model.to_string(),
        effort,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::{Config, ModelConfig, RouteConfig, RoutePrefixConfig};

    use super::{
        resolve_model, resolve_model_chain, resolve_request, resolve_request_chain,
        strip_context_window_hint, AdapterKind,
    };

    fn mapped_model(id: &str, provider: &str, upstream_model: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            display_name: None,
            upstream_model: Some(BTreeMap::from([(
                provider.to_string(),
                upstream_model.to_string(),
            )])),
        }
    }

    #[test]
    fn model_upstream_map_routes_and_translates_the_model() {
        let config = Config {
            models: vec![mapped_model("claude-opus-4-8", "codex", "gpt-5.2")],
            ..Config::default()
        };

        let route = resolve_model(&config, "claude-opus-4-8");

        assert_eq!(route.provider, "codex");
        assert_eq!(route.adapter, AdapterKind::Responses);
        assert_eq!(route.upstream_model, "gpt-5.2");
        assert_eq!(route.model, "claude-opus-4-8");
    }

    #[test]
    fn single_model_map_supports_directly_inserted_legacy_provider() {
        let mut config = Config::default();
        let custom = config.providers["codex"].clone();
        config.providers.insert("custom".into(), custom);
        config.models = vec![mapped_model("alias", "custom", "upstream-alias")];

        let route = resolve_model(&config, "alias");

        assert_eq!(route.provider, "custom");
        assert_eq!(route.adapter, AdapterKind::Responses);
        assert_eq!(route.upstream_model, "upstream-alias");
    }

    #[test]
    fn model_upstream_map_wins_over_exact_route() {
        // This config is intentionally invalid at boot (`ModelRouteConflict`
        // rejects a map-bearing id that also has a `[[routes]]` entry); the
        // resolver is exercised directly to pin the precedence order, so do
        // not add a `config.validate()` call here.
        let config = Config {
            models: vec![mapped_model("claude-opus-4-8", "codex", "gpt-5.2")],
            routes: vec![RouteConfig {
                model: "claude-opus-4-8".to_string(),
                provider: "openai".to_string(),
                upstream_model: Some("gpt-exact-route".to_string()),
                effort: None,
            }],
            ..Config::default()
        };

        let route = resolve_model(&config, "claude-opus-4-8");

        assert_eq!(route.provider, "codex");
        assert_eq!(route.upstream_model, "gpt-5.2");
    }

    #[test]
    fn model_upstream_map_wins_over_prefix_and_default_routing() {
        let mut config = Config {
            models: vec![mapped_model("claude-opus-4-8", "codex", "gpt-5.2")],
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "claude-".to_string(),
                provider: "openai".to_string(),
            }],
            ..Config::default()
        };
        config.server.default_provider = "anthropic".to_string();

        let route = resolve_model(&config, "claude-opus-4-8");

        assert_eq!(route.provider, "codex");
        assert_eq!(route.upstream_model, "gpt-5.2");
    }

    #[test]
    fn model_upstream_map_matches_after_stripping_context_window_hint() {
        let config = Config {
            models: vec![mapped_model("claude-opus-4-8", "codex", "gpt-5.2")],
            ..Config::default()
        };

        let route = resolve_model(&config, "claude-opus-4-8[1m]");

        assert_eq!(route.model, "claude-opus-4-8");
        assert_eq!(route.upstream_model, "gpt-5.2");
    }

    #[test]
    fn model_upstream_map_uses_provider_effort() {
        let mut config = Config {
            models: vec![mapped_model("claude-opus-4-8", "codex", "gpt-5.2")],
            ..Config::default()
        };
        config.providers.get_mut("codex").unwrap().effort = Some("high".to_string());

        let route = resolve_model(&config, "claude-opus-4-8");

        assert_eq!(route.effort.as_deref(), Some("high"));
    }

    #[test]
    fn model_without_upstream_map_keeps_existing_routing_precedence() {
        let config = Config {
            models: vec![ModelConfig {
                id: "claude-route".to_string(),
                display_name: None,
                upstream_model: None,
            }],
            routes: vec![RouteConfig {
                model: "claude-route".to_string(),
                provider: "codex".to_string(),
                upstream_model: Some("gpt-route".to_string()),
                effort: None,
            }],
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "claude-".to_string(),
                provider: "openai".to_string(),
            }],
            ..Config::default()
        };

        let exact = resolve_model(&config, "claude-route");
        let prefix = resolve_model(&config, "claude-prefix");
        let default = resolve_model(&config, "other-model");

        assert_eq!(exact.provider, "codex");
        assert_eq!(exact.upstream_model, "gpt-route");
        assert_eq!(prefix.provider, "openai");
        assert_eq!(default.provider, "anthropic");
    }

    #[test]
    fn strip_context_window_hint_removes_only_a_trailing_1m_suffix() {
        assert_eq!(strip_context_window_hint("gpt-5.6-sol[1m]"), "gpt-5.6-sol");
        assert_eq!(strip_context_window_hint("gpt-5.6-sol[1M]"), "gpt-5.6-sol");
        // Not a suffix / not the hint: left untouched.
        assert_eq!(strip_context_window_hint("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(
            strip_context_window_hint("[1m]gpt-5.6-sol"),
            "[1m]gpt-5.6-sol"
        );
        assert_eq!(strip_context_window_hint("gpt-[1m]-sol"), "gpt-[1m]-sol");
        assert_eq!(strip_context_window_hint("[1m]"), "");
        // Non-ASCII id must not panic on the byte-index slice.
        assert_eq!(strip_context_window_hint("모델[1m]"), "모델");
        assert_eq!(strip_context_window_hint("모델"), "모델");
    }

    #[test]
    fn one_million_suffix_is_stripped_before_matching_and_forwarding() {
        let config = Config {
            routes: vec![RouteConfig {
                model: "claude-gpt-5.6-sol-via-codex".to_string(),
                provider: "codex".to_string(),
                upstream_model: Some("gpt-5.6-sol".to_string()),
                effort: None,
            }],
            ..Config::default()
        };

        // The `[1m]` variant resolves to the same route, and the upstream slug
        // never carries the suffix (Codex would reject it otherwise).
        let route = resolve_model(&config, "claude-gpt-5.6-sol-via-codex[1m]");
        assert_eq!(route.provider, "codex");
        assert_eq!(route.adapter, AdapterKind::Responses);
        assert_eq!(route.upstream_model, "gpt-5.6-sol");
        assert_eq!(route.model, "claude-gpt-5.6-sol-via-codex");
    }

    #[test]
    fn one_million_suffix_is_stripped_on_prefix_routes() {
        let config = Config {
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "gpt-".to_string(),
                provider: "openai".to_string(),
            }],
            ..Config::default()
        };

        // Prefix routing forwards the incoming id as the upstream model, so the
        // suffix must be gone before it reaches the provider.
        let route = resolve_model(&config, "gpt-5.6-sol[1m]");
        assert_eq!(route.provider, "openai");
        assert_eq!(route.upstream_model, "gpt-5.6-sol");
        assert_eq!(route.model, "gpt-5.6-sol");
    }

    #[test]
    fn explicit_routes_win_before_prefix_and_default() {
        let config = Config {
            routes: vec![RouteConfig {
                model: "gpt-special".to_string(),
                provider: "openai".to_string(),
                upstream_model: Some("gpt-upstream".to_string()),
                effort: Some("high".to_string()),
            }],
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "gpt-".to_string(),
                provider: "openai".to_string(),
            }],
            ..Config::default()
        };

        let route = resolve_model(&config, "gpt-special");

        assert_eq!(route.adapter, AdapterKind::Responses);
        assert_eq!(route.upstream_model, "gpt-upstream");
        assert_eq!(route.effort.as_deref(), Some("high"));
    }

    #[test]
    fn ordered_model_chain_uses_declaration_order_and_per_upstream_defaults() {
        let mut config = Config {
            upstreams_ordered: true,
            upstream_order: vec!["openai".into(), "anthropic".into(), "codex".into()],
            models: vec![ModelConfig {
                id: "alias".into(),
                display_name: None,
                upstream_model: Some(BTreeMap::from([
                    ("codex".into(), "gpt-codex".into()),
                    ("openai".into(), "gpt-openai".into()),
                ])),
            }],
            ..Config::default()
        };
        config.providers.get_mut("openai").unwrap().effort = Some("medium".into());
        config.providers.get_mut("codex").unwrap().effort = Some("high".into());

        let routes = resolve_model_chain(&config, "alias[1M]");

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].provider, "openai");
        assert_eq!(routes[0].upstream_model, "gpt-openai");
        assert_eq!(routes[0].effort.as_deref(), Some("medium"));
        assert_eq!(routes[1].provider, "codex");
        assert_eq!(routes[1].upstream_model, "gpt-codex");
        assert_eq!(routes[1].effort.as_deref(), Some("high"));
        assert!(routes.iter().all(|route| route.model == "alias"));
        assert_eq!(resolve_model(&config, "alias").provider, "openai");
    }

    #[test]
    fn route_prefix_and_default_paths_return_single_element_chains() {
        let config = Config {
            routes: vec![RouteConfig {
                model: "exact".into(),
                provider: "codex".into(),
                upstream_model: Some("gpt-exact".into()),
                effort: Some("high".into()),
            }],
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "gpt-".into(),
                provider: "openai".into(),
            }],
            ..Config::default()
        };

        for (model, provider) in [
            ("exact", "codex"),
            ("gpt-prefix", "openai"),
            ("other", "anthropic"),
        ] {
            let routes = resolve_model_chain(&config, model);
            assert_eq!(routes.len(), 1);
            assert_eq!(routes[0].provider, provider);
        }
    }

    #[test]
    fn request_chain_and_legacy_wrapper_return_the_same_requested_model() {
        let config = Config {
            upstreams_ordered: true,
            upstream_order: vec!["codex".into(), "openai".into()],
            models: vec![ModelConfig {
                id: "alias".into(),
                display_name: None,
                upstream_model: Some(BTreeMap::from([
                    ("openai".into(), "gpt-openai".into()),
                    ("codex".into(), "gpt-codex".into()),
                ])),
            }],
            ..Config::default()
        };
        let body = br#"{"model":"alias[1m]"}"#;

        let (chain, requested) = resolve_request_chain(&config, body).unwrap();
        let (first, legacy_requested) = resolve_request(&config, body).unwrap();

        assert_eq!(requested, "alias[1m]");
        assert_eq!(legacy_requested, requested);
        assert_eq!(chain[0], first);
        assert_eq!(first.provider, "codex");
    }

    #[test]
    fn codex_routes_use_responses_adapter_and_codex_effort() {
        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().effort = Some("high".to_string());
        config.route_prefixes = vec![RoutePrefixConfig {
            prefix: "gpt-".to_string(),
            provider: "codex".to_string(),
        }];

        let route = resolve_model(&config, "gpt-5.2-codex");

        assert_eq!(route.provider, "codex");
        assert_eq!(route.adapter, AdapterKind::Responses);
        assert_eq!(route.effort.as_deref(), Some("high"));
    }
}
