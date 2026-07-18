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
}

impl From<ProviderKind> for AdapterKind {
    fn from(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Anthropic => AdapterKind::Anthropic,
            ProviderKind::Responses => AdapterKind::Responses,
            ProviderKind::Cursor => AdapterKind::Cursor,
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
    let view: RoutingView = serde_json::from_slice(body).map_err(|error| {
        ShuntError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("request body must include a JSON model field: {error}"),
        )
    })?;
    let route = resolve_model(config, &view.model);
    Ok((route, view.model))
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
    let model = strip_context_window_hint(model);
    for route in &config.routes {
        if route.model == model {
            return route_for(
                config,
                &route.provider,
                model,
                route.upstream_model.as_deref().unwrap_or(model),
                route.effort.clone(),
            );
        }
    }
    for route in &config.route_prefixes {
        if model.starts_with(&route.prefix) {
            return route_for(config, &route.provider, model, model, None);
        }
    }
    route_for(config, &config.server.default_provider, model, model, None)
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
    use crate::config::{Config, RouteConfig, RoutePrefixConfig};

    use super::{resolve_model, strip_context_window_hint, AdapterKind};

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
