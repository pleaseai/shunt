use std::{collections::BTreeMap, net::SocketAddr, path::Path};

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
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub route_prefixes: Vec<RoutePrefixConfig>,
}

/// Providers are a name → config map, so a new upstream is just another
/// `[providers.<name>]` table — no code change. figment deep-merges the map, so
/// a partial `[providers.codex]` in shunt.toml overrides only the fields it sets
/// while the built-in defaults (anthropic/openai/codex) fill the rest.
pub type ProvidersConfig = BTreeMap<String, ProviderConfig>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub bind: String,
    pub default_provider: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// Which protocol the upstream speaks, i.e. which adapter handles it.
    pub kind: ProviderKind,
    pub base_url: String,
    /// How shunt authenticates to this upstream.
    #[serde(default)]
    pub auth: AuthMode,
    /// Env var holding the API key, when `auth = "api_key"`.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Header the API key is sent in, when `auth = "api_key"`.
    #[serde(default)]
    pub api_key_header: ApiKeyHeader,
    /// Optional default reasoning effort for `kind = "responses"` providers.
    #[serde(default)]
    pub effort: Option<String>,
    /// How `POST /v1/messages/count_tokens` is answered for this provider.
    #[serde(default)]
    pub count_tokens: CountTokens,
}

/// How a provider answers `count_tokens`. Only meaningful for `responses`
/// providers; Anthropic providers always pass the request through upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CountTokens {
    /// Return 404 so Claude Code estimates tokens locally (no server endpoint
    /// exists on the Responses API; the gateway protocol allows this).
    #[default]
    Estimate,
    /// Compute an approximate count locally with tiktoken (o200k_base) and
    /// return `{"input_tokens": N}` — closer than the client's char/4 estimate,
    /// but not an exact match for the backend's billed count.
    Tiktoken,
}

/// The upstream protocol / adapter a provider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Anthropic Messages API — passed through, optionally re-keyed. Covers
    /// api.anthropic.com and every Anthropic-compatible gateway (Kimi, DeepSeek,
    /// Z.ai, MiniMax, Mimo, OpenRouter, Vercel AI Gateway, …).
    Anthropic,
    /// OpenAI Responses API — Anthropic Messages are translated to it (OpenAI,
    /// ChatGPT/Codex).
    Responses,
}

/// How shunt authenticates to an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Forward the client's own credential unchanged (api.anthropic.com).
    #[default]
    Passthrough,
    /// Inject an API key read from `api_key_env`.
    ApiKey,
    /// Reuse the ChatGPT/Codex OAuth login in ~/.codex/auth.json.
    ChatgptOauth,
}

/// Which header an injected API key is sent in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyHeader {
    /// `Authorization: Bearer <key>` (most gateways; also `ANTHROPIC_AUTH_TOKEN`).
    #[default]
    Bearer,
    /// `x-api-key: <key>` (Anthropic-native style; Vercel AI Gateway).
    XApiKey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteConfig {
    pub model: String,
    pub provider: String,
    pub upstream_model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub id: String,
    pub display_name: Option<String>,
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
    #[error("providers.{provider}.base_url must be a valid absolute URL: {message}")]
    ProviderBaseUrl { provider: String, message: String },
    #[error("providers.{provider}.base_url must include a scheme and host")]
    ProviderBaseUrlMissingHost { provider: String },
    #[error("providers.{provider} uses auth = \"api_key\" but api_key_env is not set")]
    MissingApiKeyEnv { provider: String },
    #[error("server.default_provider references unknown provider: {0}")]
    UnknownDefaultProvider(String),
    #[error("route for model {model} references unknown provider: {provider}")]
    UnknownRouteProvider { model: String, provider: String },
    #[error("route prefix {prefix} references unknown provider: {provider}")]
    UnknownPrefixProvider { prefix: String, provider: String },
}

impl ProviderConfig {
    fn anthropic(base_url: &str) -> Self {
        Self {
            kind: ProviderKind::Anthropic,
            base_url: base_url.to_string(),
            auth: AuthMode::Passthrough,
            api_key_env: None,
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            count_tokens: CountTokens::Estimate,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let providers = ProvidersConfig::from([
            (
                "anthropic".to_string(),
                ProviderConfig::anthropic("https://api.anthropic.com"),
            ),
            (
                "openai".to_string(),
                ProviderConfig {
                    kind: ProviderKind::Responses,
                    base_url: "https://api.openai.com/v1".to_string(),
                    auth: AuthMode::ApiKey,
                    api_key_env: Some("OPENAI_API_KEY".to_string()),
                    api_key_header: ApiKeyHeader::Bearer,
                    effort: None,
                    count_tokens: CountTokens::Estimate,
                },
            ),
            (
                "codex".to_string(),
                ProviderConfig {
                    kind: ProviderKind::Responses,
                    base_url: "https://chatgpt.com/backend-api".to_string(),
                    auth: AuthMode::ChatgptOauth,
                    api_key_env: None,
                    api_key_header: ApiKeyHeader::Bearer,
                    effort: None,
                    count_tokens: CountTokens::Estimate,
                },
            ),
        ]);
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:3001".to_string(),
                default_provider: "anthropic".to_string(),
            },
            providers,
            models: Vec::new(),
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
        for (name, provider) in &self.providers {
            self.provider_base_url(name, &provider.base_url)?;
            if provider.auth == AuthMode::ApiKey
                && provider
                    .api_key_env
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
            {
                return Err(ConfigError::MissingApiKeyEnv {
                    provider: name.clone(),
                });
            }
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
        for model in &self.models {
            if !self.routes.iter().any(|route| route.model == model.id) {
                tracing::warn!(
                    model_id = %model.id,
                    "configured discovery model has no matching route"
                );
            }
        }
        Ok(self)
    }

    /// Look up a provider by name.
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    /// Whether `provider` is the ChatGPT/Codex backend (ChatGPT OAuth auth).
    /// That backend serves the Responses API under `/codex/responses` and is
    /// stricter than the stock OpenAI Responses API — it rejects parameters
    /// codex never sends (e.g. `max_output_tokens`), so translation drops them.
    pub fn is_chatgpt_backend(&self, provider: &str) -> bool {
        self.provider(provider)
            .map(|provider| provider.auth == AuthMode::ChatgptOauth)
            .unwrap_or(false)
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
        self.providers.contains_key(provider)
    }
}

impl ServerConfig {
    pub fn bind_addr(&self) -> Result<SocketAddr, ConfigError> {
        Ok(self.bind.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use super::{AuthMode, Config, ModelConfig, ProviderKind};

    struct BufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for BufferWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn validate_warns_when_discovery_model_has_no_matching_route() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let config = Config {
            models: vec![ModelConfig {
                id: "claude-opus-via-codex".to_string(),
                display_name: None,
            }],
            ..Config::default()
        };

        tracing::subscriber::with_default(subscriber, || {
            config.validate().unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(logs.contains("configured discovery model has no matching route"));
        assert!(logs.contains("claude-opus-via-codex"));
    }

    #[test]
    fn default_seeds_builtin_providers() {
        let config = Config::default();
        assert_eq!(
            config.provider("anthropic").unwrap().kind,
            ProviderKind::Anthropic
        );
        assert_eq!(
            config.provider("openai").unwrap().kind,
            ProviderKind::Responses
        );
        assert_eq!(
            config.provider("codex").unwrap().auth,
            AuthMode::ChatgptOauth
        );
        assert!(config.provider("kimi").is_none());
    }

    #[test]
    fn toml_adds_a_provider_and_merges_builtin_overrides() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        std::fs::write(
            &path,
            r#"
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "KIMI_API_KEY"

[providers.codex]
effort = "high"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();

        // New provider added from TOML.
        let kimi = config.provider("kimi").unwrap();
        assert_eq!(kimi.kind, ProviderKind::Anthropic);
        assert_eq!(kimi.auth, AuthMode::ApiKey);
        assert_eq!(kimi.api_key_env.as_deref(), Some("KIMI_API_KEY"));
        // Built-in codex kept its default base_url/auth while gaining effort.
        let codex = config.provider("codex").unwrap();
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(codex.auth, AuthMode::ChatgptOauth);
        assert_eq!(codex.effort.as_deref(), Some("high"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
