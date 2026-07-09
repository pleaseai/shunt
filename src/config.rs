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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvidersConfig {
    pub anthropic: AnthropicConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicConfig {
    pub base_url: String,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:3001".to_string(),
            },
            providers: ProvidersConfig {
                anthropic: AnthropicConfig {
                    base_url: "https://api.anthropic.com".to_string(),
                },
            },
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
}

impl ServerConfig {
    pub fn bind_addr(&self) -> Result<SocketAddr, ConfigError> {
        Ok(self.bind.parse()?)
    }
}
