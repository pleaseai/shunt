use axum::http::StatusCode;
use serde::Deserialize;

use crate::{
    config::{Config, ProviderKind},
    error::ShuntError,
};

use stage::StageContext;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterKind {
    Anthropic,
    Responses,
    Cursor,
    Gemini,
    /// Local `agy` subprocess execution. Deprecated alongside
    /// [`ProviderKind::AntigravityCli`].
    AntigravityCli,
}

impl From<ProviderKind> for AdapterKind {
    fn from(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Anthropic => AdapterKind::Anthropic,
            ProviderKind::Responses => AdapterKind::Responses,
            ProviderKind::Cursor => AdapterKind::Cursor,
            ProviderKind::Gemini => AdapterKind::Gemini,
            // Stage 1 of the native Antigravity upstream is wire-identical to
            // the Code Assist path, so it rides the Gemini adapter; only the
            // credential and the discovery metadata differ. The
            // Antigravity-specific request rewrites (signature replay, schema
            // sanitization, the claude/non-claude split) fork this in stage 2.
            ProviderKind::Antigravity => AdapterKind::Gemini,
            ProviderKind::AntigravityCli => AdapterKind::AntigravityCli,
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
    pub service_tier: Option<String>,
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
    // Deserialize the narrow view straight from bytes for callers without a
    // parsed tree: serde can skip every non-model field without materializing it.
    let view: RoutingView = serde_json::from_slice(body).map_err(invalid_routing_request)?;
    Ok(resolve_view(config, view, None))
}

pub(crate) fn resolve_request_chain_value(
    config: &Config,
    request: &serde_json::Value,
    stage: Option<&StageContext<'_>>,
) -> Result<(Vec<Route>, String), ShuntError> {
    let view = RoutingView::deserialize(request).map_err(invalid_routing_request)?;
    Ok(resolve_view(config, view, stage))
}

fn resolve_view(
    config: &Config,
    view: RoutingView,
    stage: Option<&StageContext<'_>>,
) -> (Vec<Route>, String) {
    let routes = resolve_chain(config, &view.model, stage);
    (routes, view.model)
}

pub(crate) fn invalid_routing_request(error: serde_json::Error) -> ShuntError {
    ShuntError::new(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        format!("request body must include a JSON model field: {error}"),
    )
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

/// Resolve a model id to its failover chain, without a request to score.
///
/// A `[models.stage_router]` entry reached this way reports its picker default,
/// which is what a body-less surface should show: the tier a fresh session
/// starts on. Live requests go through [`resolve_request_chain_value`].
pub fn resolve_model_chain(config: &Config, model: &str) -> Vec<Route> {
    resolve_chain(config, model, None)
}

fn resolve_chain(config: &Config, model: &str, stage: Option<&StageContext<'_>>) -> Vec<Route> {
    let model = strip_context_window_hint(model);
    for configured_model in &config.models {
        if configured_model.id == model {
            if let Some(router) = configured_model.stage_router.as_ref() {
                let target = stage::select(router, model, stage).tier.target(router);
                // One hop only, and structurally so: config validation rejects a
                // router whose target is itself a router, so the recursive call
                // cannot re-enter this arm.
                let mut routes = resolve_chain(config, target, None);
                for route in &mut routes {
                    // `Route.model` is the id reported back to the client, and
                    // Claude Code records it to restore the model on `--resume`.
                    // It must stay the id the caller asked for; the tier the
                    // router picked travels upstream in `upstream_model` and
                    // nowhere else (issue #172).
                    route.model = model.to_string();
                }
                return routes;
            }
            if let Some(upstream_models) = configured_model.upstream_model.as_ref() {
                // Preserve the legacy single-map path even for a Config assembled
                // directly in code without validation refreshing derived order.
                if let Some((provider, upstream_model)) = (upstream_models.len() == 1)
                    .then(|| upstream_models.iter().next())
                    .flatten()
                {
                    return vec![route_for(
                        config,
                        provider,
                        model,
                        upstream_model,
                        None,
                        None,
                    )];
                }
                let routes = config
                    .upstream_order
                    .iter()
                    .filter_map(|provider| {
                        upstream_models.get(provider).map(|upstream_model| {
                            route_for(config, provider, model, upstream_model, None, None)
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
                route.service_tier.clone(),
            )];
        }
    }
    for route in &config.route_prefixes {
        if model.starts_with(&route.prefix) {
            return vec![route_for(config, &route.provider, model, model, None, None)];
        }
    }
    vec![route_for(
        config,
        &config.server.default_provider,
        model,
        model,
        None,
        None,
    )]
}

fn route_for(
    config: &Config,
    provider: &str,
    model: &str,
    upstream_model: &str,
    effort: Option<String>,
    service_tier: Option<String>,
) -> Route {
    // The provider's declared kind picks the adapter; unknown names (only
    // reachable via a validated default) fall back to the Anthropic passthrough.
    let provider_config = config.provider(provider);
    let adapter = provider_config
        .map(|p| AdapterKind::from(p.kind))
        .unwrap_or(AdapterKind::Anthropic);
    let effort = effort.or_else(|| provider_config.and_then(|p| p.effort.clone()));
    let service_tier =
        service_tier.or_else(|| provider_config.and_then(|p| p.service_tier.clone()));
    Route {
        provider: provider.to_string(),
        adapter,
        model: model.to_string(),
        upstream_model: upstream_model.to_string(),
        effort,
        service_tier,
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
            stage_router: None,
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
                service_tier: None,
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
    fn model_upstream_map_uses_provider_service_tier() {
        let mut config = Config {
            models: vec![mapped_model("claude-opus-4-8", "codex", "gpt-5.2")],
            ..Config::default()
        };
        config.providers.get_mut("codex").unwrap().service_tier = Some("priority".to_string());

        let route = resolve_model(&config, "claude-opus-4-8");

        assert_eq!(route.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn model_without_upstream_map_keeps_existing_routing_precedence() {
        let config = Config {
            models: vec![ModelConfig {
                id: "claude-route".to_string(),
                display_name: None,
                upstream_model: None,
                stage_router: None,
            }],
            routes: vec![RouteConfig {
                model: "claude-route".to_string(),
                provider: "codex".to_string(),
                upstream_model: Some("gpt-route".to_string()),
                effort: None,
                service_tier: None,
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
                service_tier: None,
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
                service_tier: None,
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
    fn route_level_service_tier_wins_over_provider_level() {
        let mut config = Config {
            routes: vec![RouteConfig {
                model: "gpt-special".to_string(),
                provider: "openai".to_string(),
                upstream_model: Some("gpt-upstream".to_string()),
                effort: None,
                service_tier: Some("priority".to_string()),
            }],
            ..Config::default()
        };
        config.providers.get_mut("openai").unwrap().service_tier = Some("flex".to_string());

        let route = resolve_model(&config, "gpt-special");

        assert_eq!(route.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn service_tier_falls_back_to_provider_when_route_unset() {
        let mut config = Config {
            routes: vec![RouteConfig {
                model: "gpt-special".to_string(),
                provider: "openai".to_string(),
                upstream_model: Some("gpt-upstream".to_string()),
                effort: None,
                service_tier: None,
            }],
            ..Config::default()
        };
        config.providers.get_mut("openai").unwrap().service_tier = Some("flex".to_string());

        let route = resolve_model(&config, "gpt-special");

        assert_eq!(route.service_tier.as_deref(), Some("flex"));
    }

    #[test]
    fn service_tier_is_absent_by_default() {
        let config = Config {
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "gpt-".to_string(),
                provider: "openai".to_string(),
            }],
            ..Config::default()
        };

        let route = resolve_model(&config, "gpt-plain");

        assert_eq!(route.service_tier, None);
    }

    #[test]
    fn route_level_default_sentinel_is_not_overridden_by_provider_tier() {
        // The documented route-over-provider override lets a route disable an
        // inherited provider-level Fast/Flex tier by setting service_tier =
        // "default". Option::or_else only falls back on None, so the
        // normalized sentinel Some("default") must survive here untouched --
        // regression test for issue #301, where collapsing "default" to None
        // during config validation made it indistinguishable from unset and
        // the provider tier leaked through. The wire-emission filter
        // (model/responses_request.rs) is what turns the sentinel into
        // "send nothing".
        let mut config = Config {
            routes: vec![RouteConfig {
                model: "gpt-special".to_string(),
                provider: "openai".to_string(),
                upstream_model: Some("gpt-upstream".to_string()),
                effort: None,
                service_tier: Some("default".to_string()),
            }],
            ..Config::default()
        };
        config.providers.get_mut("openai").unwrap().service_tier = Some("priority".to_string());

        let route = resolve_model(&config, "gpt-special");

        assert_eq!(route.service_tier.as_deref(), Some("default"));
    }

    #[test]
    fn provider_level_default_sentinel_is_inherited_by_unset_route() {
        // A provider-only "default" still flows through the same
        // Option::or_else fallback as any other provider tier -- it resolves
        // to Some("default") rather than None, so downstream wire-emission
        // (not this resolution step) is what turns it into "send nothing".
        let mut config = Config {
            route_prefixes: vec![RoutePrefixConfig {
                prefix: "gpt-".to_string(),
                provider: "openai".to_string(),
            }],
            ..Config::default()
        };
        config.providers.get_mut("openai").unwrap().service_tier = Some("default".to_string());

        let route = resolve_model(&config, "gpt-plain");

        assert_eq!(route.service_tier.as_deref(), Some("default"));
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
                stage_router: None,
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
                service_tier: None,
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
    fn request_chain_rejects_duplicate_model_fields() {
        assert!(resolve_request_chain(
            &Config::default(),
            br#"{"model":"first","model":"second"}"#,
        )
        .is_err());
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
                stage_router: None,
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

pub(crate) mod stage;

/// Stage-router resolution tests.
///
/// These pin the two properties the router must not lose when it is wired into
/// the ladder: what the client is told it got, and that a router target is
/// resolved exactly once. Non-vacuity: delete the `route.model` re-stamp and
/// `a_stage_router_reports_the_requested_id_to_the_client` goes red; make the
/// router arm fall through instead of returning and
/// `a_stage_router_resolves_its_target_through_the_ordinary_ladder` goes red.
#[cfg(test)]
mod stage_router_tests {
    use std::{collections::BTreeMap, time::Instant};

    use serde_json::json;

    use crate::{
        config::{Config, ModelConfig, StageRouterConfig, StageRouterPicker},
        routing::stage::{StageContext, StageRouterStore},
    };

    use super::{resolve_model, resolve_request_chain_value, AdapterKind};

    const ROUTER_ID: &str = "claude-auto";

    fn router() -> StageRouterConfig {
        StageRouterConfig {
            capable_target: "capable-alias".to_string(),
            efficient_target: "efficient-alias".to_string(),
            picker: StageRouterPicker::EfficientFirst,
            confidence_threshold: crate::config::DEFAULT_CONFIDENCE_THRESHOLD,
            recent_turn_window: 3,
            min_dwell_turns: 3,
            deescalate_threshold: None,
            session_ttl_seconds: 3600,
        }
    }

    fn mapped(id: &str, upstream_model: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            display_name: None,
            upstream_model: Some(BTreeMap::from([(
                "codex".to_string(),
                upstream_model.to_string(),
            )])),
            stage_router: None,
        }
    }

    fn config() -> Config {
        Config {
            models: vec![
                ModelConfig {
                    id: ROUTER_ID.to_string(),
                    display_name: None,
                    upstream_model: None,
                    stage_router: Some(router()),
                },
                mapped("capable-alias", "upstream-capable"),
                mapped("efficient-alias", "upstream-efficient"),
            ],
            ..Config::default()
        }
    }

    /// Two failed investigative turns — enough for the scorer to escalate.
    fn erroring_request() -> serde_json::Value {
        json!({
            "model": ROUTER_ID,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "Grep"}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "b", "is_error": true}]},
            ]
        })
    }

    /// The router's target travels upstream; the id the caller asked for is what
    /// comes back. Claude Code records the reported id and restores the model
    /// from it on `--resume`, so leaking the target here is issue #172 again.
    #[test]
    fn a_stage_router_reports_the_requested_id_to_the_client() {
        let config = config();
        let store = StageRouterStore::new();
        let request = erroring_request();
        let context = StageContext {
            store: &store,
            request: &request,
            session_id: Some("session-a"),
            read_only: false,
            now: Instant::now(),
        };

        let (routes, requested) = resolve_request_chain_value(&config, &request, Some(&context))
            .expect("a router-backed id resolves");

        assert_eq!(requested, ROUTER_ID);
        let route = routes.first().expect("route chains are non-empty");
        assert_eq!(
            route.upstream_model, "upstream-capable",
            "an erroring session must reach the capable tier"
        );
        assert_eq!(
            route.model, ROUTER_ID,
            "the client must be told the id it asked for, not the tier"
        );
    }

    /// The target is a public model id, so it resolves through the same ladder
    /// as any other — picking up its provider, adapter, and upstream mapping.
    #[test]
    fn a_stage_router_resolves_its_target_through_the_ordinary_ladder() {
        let config = config();
        let store = StageRouterStore::new();
        let request = erroring_request();
        let context = StageContext {
            store: &store,
            request: &request,
            session_id: None,
            read_only: false,
            now: Instant::now(),
        };

        let (routes, _) = resolve_request_chain_value(&config, &request, Some(&context))
            .expect("a router-backed id resolves");
        let route = routes.first().expect("route chains are non-empty");

        assert_eq!(route.provider, "codex");
        assert_eq!(route.adapter, AdapterKind::Responses);
    }

    /// `/routes`, discovery, and the public `resolve_model` have no conversation
    /// to score. They must report the tier a fresh session starts on rather than
    /// panicking or inventing a signal.
    #[test]
    fn a_body_less_resolution_reports_the_picker_default() {
        let mut config = config();

        let route = resolve_model(&config, ROUTER_ID);
        assert_eq!(route.upstream_model, "upstream-efficient");
        assert_eq!(route.model, ROUTER_ID);

        config.models[0].stage_router = Some(StageRouterConfig {
            picker: StageRouterPicker::CapableFirst,
            ..router()
        });
        let route = resolve_model(&config, ROUTER_ID);
        assert_eq!(route.upstream_model, "upstream-capable");
    }

    /// A `[1m]` request still names the bare id back to the client, exactly as a
    /// non-router model does.
    #[test]
    fn the_context_window_hint_is_stripped_before_the_router_sees_the_id() {
        let config = config();

        let route = resolve_model(&config, "claude-auto[1m]");

        assert_eq!(route.model, ROUTER_ID);
        assert_eq!(route.upstream_model, "upstream-efficient");
    }
}
