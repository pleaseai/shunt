use std::{net::SocketAddr, path::Path};

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub route_prefixes: Vec<RoutePrefixConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub bind: String,
    pub default_provider: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvidersConfig {
    pub anthropic: AnthropicConfig,
    pub openai: OpenAiConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicConfig {
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiConfig {
    pub adapter: String,
    pub base_url: String,
    pub api_key_env: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteConfig {
    pub model: String,
    pub provider: String,
    pub upstream_model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutePrefixConfig {
    pub prefix: String,
    pub provider: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Figment(#[from] Box<figment::Error>),
    #[error("server.bind must be a socket address: {0}")]
    BindAddress(#[from] std::net::AddrParseError),
    #[error("providers.anthropic.base_url must be a valid absolute URL: {0}")]
    BaseUrl(String),
    #[error("providers.anthropic.base_url must include a scheme and host")]
    BaseUrlMissingHost,
    #[error("{provider}.base_url must be a valid absolute URL: {message}")]
    ProviderBaseUrl { provider: String, message: String },
    #[error("{provider}.base_url must include a scheme and host")]
    ProviderBaseUrlMissingHost { provider: String },
    #[error("server.default_provider references unknown provider: {0}")]
    UnknownDefaultProvider(String),
    #[error("route for model {model} references unknown provider: {provider}")]
    UnknownRouteProvider { model: String, provider: String },
    #[error("route prefix {prefix} references unknown provider: {provider}")]
    UnknownPrefixProvider { prefix: String, provider: String },
    #[error("providers.openai.adapter must be responses")]
    OpenAiAdapter,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:3001".to_string(),
                default_provider: "anthropic".to_string(),
            },
            providers: ProvidersConfig {
                anthropic: AnthropicConfig {
                    base_url: "https://api.anthropic.com".to_string(),
                },
                openai: OpenAiConfig {
                    adapter: "responses".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    api_key_env: "OPENAI_API_KEY".to_string(),
                    effort: None,
                },
            },
            routes: Vec::new(),
            route_prefixes: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = path.unwrap_or_else(|| Path::new("./shunt.toml"));
        let config: Self = Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("SHUNT_").split("__"))
            .extract()
            .map_err(Box::new)?;
        config.validate()
    }

    pub fn validate(self) -> Result<Self, ConfigError> {
        self.server.bind_addr()?;
        self.anthropic_base_url()?;
        self.openai_base_url()?;
        if self.providers.openai.adapter != "responses" {
            return Err(ConfigError::OpenAiAdapter);
        }
        if !self.has_provider(&self.server.default_provider) {
            return Err(ConfigError::UnknownDefaultProvider(
                self.server.default_provider.clone(),
            ));
        }
        for route in &self.routes {
            if !self.has_provider(&route.provider) {
                return Err(ConfigError::UnknownRouteProvider {
                    model: route.model.clone(),
                    provider: route.provider.clone(),
                });
            }
        }
        for route in &self.route_prefixes {
            if !self.has_provider(&route.provider) {
                return Err(ConfigError::UnknownPrefixProvider {
                    prefix: route.prefix.clone(),
                    provider: route.provider.clone(),
                });
            }
        }
        Ok(self)
    }

    pub fn anthropic_base_url(&self) -> Result<reqwest::Url, ConfigError> {
        let url = reqwest::Url::parse(&self.providers.anthropic.base_url)
            .map_err(|error| ConfigError::BaseUrl(error.to_string()))?;
        if url.scheme().is_empty() || url.host_str().is_none() {
            return Err(ConfigError::BaseUrlMissingHost);
        }
        Ok(url)
    }

    pub fn openai_base_url(&self) -> Result<reqwest::Url, ConfigError> {
        self.provider_base_url("providers.openai", &self.providers.openai.base_url)
    }

    pub fn provider_base_url(
        &self,
        provider: &str,
        base_url: &str,
    ) -> Result<reqwest::Url, ConfigError> {
        let url = reqwest::Url::parse(base_url).map_err(|error| ConfigError::ProviderBaseUrl {
            provider: provider.to_string(),
            message: error.to_string(),
        })?;
        if url.scheme().is_empty() || url.host_str().is_none() {
            return Err(ConfigError::ProviderBaseUrlMissingHost {
                provider: provider.to_string(),
            });
        }
        Ok(url)
    }

    fn has_provider(&self, provider: &str) -> bool {
        matches!(provider, "anthropic" | "openai")
    }
}

impl ServerConfig {
    pub fn bind_addr(&self) -> Result<SocketAddr, ConfigError> {
        Ok(self.bind.parse()?)
    }
}
