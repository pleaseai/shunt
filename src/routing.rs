use axum::http::StatusCode;
use serde::Deserialize;

use crate::{config::Config, error::ShuntError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterKind {
    Anthropic,
    Responses,
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
    let view: RoutingView = serde_json::from_slice(body).map_err(|error| {
        ShuntError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("request body must include a JSON model field: {error}"),
        )
    })?;
    Ok(resolve_model(config, &view.model))
}

pub fn resolve_model(config: &Config, model: &str) -> Route {
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
    let adapter = if provider == "openai" {
        AdapterKind::Responses
    } else {
        AdapterKind::Anthropic
    };
    let effort = effort.or_else(|| {
        if provider == "openai" {
            config.providers.openai.effort.clone()
        } else {
            None
        }
    });
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

    use super::{resolve_model, AdapterKind};

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
}
