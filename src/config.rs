use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

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
    /// Optional opt-in Sentry error reporting. Absent (the default) means no
    /// Sentry client is created and nothing ever leaves the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentry: Option<SentryConfig>,
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
    /// Optional inbound client authentication for shared gateways (M4).
    /// Absent ⇒ no inbound auth (loopback-only personal use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<InboundAuthConfig>,
    /// Idle seconds before shunt injects an SSE `ping` event into a streaming
    /// response so middlebox timers (Cloudflare's 100s → 524) never expire.
    /// `0` disables injection (M5).
    #[serde(default = "default_sse_keepalive_seconds")]
    pub sse_keepalive_seconds: u64,
}

fn default_sse_keepalive_seconds() -> u64 {
    30
}

/// `[server.auth]` — inbound client-token check on injected-credential routes.
/// Tokens live in the environment (never in the TOML), as `name:token` pairs:
/// `SHUNT_CLIENT_TOKENS="alice:3f9c…,bob:a41b…"`. See `docs/m4-inbound-auth.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InboundAuthConfig {
    /// Header carrying the client token.
    #[serde(default = "default_auth_header")]
    pub header: String,
    /// Env var holding the `name:token` pairs.
    #[serde(default = "default_tokens_env")]
    pub tokens_env: String,
}

fn default_auth_header() -> String {
    "x-shunt-token".to_string()
}

fn default_tokens_env() -> String {
    "SHUNT_CLIENT_TOKENS".to_string()
}

impl InboundAuthConfig {
    /// Resolve the configured tokens from the environment. Fails closed: a
    /// present `[server.auth]` with an unset/empty/malformed env var is a
    /// startup error, never a silently-open gateway.
    pub fn resolve(&self) -> Result<crate::auth::inbound::InboundAuth, ConfigError> {
        let header = axum::http::HeaderName::from_bytes(self.header.as_bytes()).map_err(|_| {
            ConfigError::InvalidAuthHeader {
                header: self.header.clone(),
            }
        })?;
        let raw = std::env::var(&self.tokens_env).unwrap_or_default();
        if raw.trim().is_empty() {
            return Err(ConfigError::MissingClientTokens {
                env: self.tokens_env.clone(),
            });
        }
        let tokens = crate::auth::inbound::parse_tokens(&raw).map_err(|message| {
            ConfigError::InvalidClientTokens {
                env: self.tokens_env.clone(),
                message,
            }
        })?;
        Ok(crate::auth::inbound::InboundAuth::new(header, tokens))
    }
}

/// `[sentry]` — opt-in error reporting to the operator's own Sentry project.
/// Only gateway-owned diagnostics are reported (panics plus `error!` log
/// events, with `warn!`/`info!` as breadcrumbs); request/response bodies,
/// headers, and credentials never are.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SentryConfig {
    /// DSN of the operator's Sentry project. An empty string disables
    /// reporting, so `SHUNT_SENTRY__DSN=""` can turn a TOML-configured section
    /// off without editing the file.
    pub dsn: String,
    /// Optional environment tag on reported events (e.g. "prod", "home-lab").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Also send usage/performance metrics (request counts and latency per
    /// provider/model). Off by default — a separate opt-in from error
    /// reporting, since metrics describe traffic rather than gateway faults.
    #[serde(default)]
    pub metrics: bool,
}

impl SentryConfig {
    /// Whether this section actually enables reporting (non-empty DSN).
    pub fn enabled(&self) -> bool {
        !self.dsn.trim().is_empty()
    }
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
    /// Return 404 so the client falls back on its own (no server endpoint
    /// exists on the Responses API; the gateway protocol allows this). Claude
    /// Code's /context reacts by re-counting every category against Haiku over
    /// the network — slow, and silently zero without an Anthropic credential —
    /// so this is opt-in rather than the default.
    Estimate,
    /// Compute the count locally with tiktoken (o200k_base) and return
    /// `{"input_tokens": N}`. o200k_base is the GPT-family encoder, so for
    /// responses-routed models this is near-exact for text, though it can't see
    /// the backend's image/tool-schema encoding or cache accounting.
    #[default]
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
    /// xAI subscription OAuth (SuperGrok / X Premium+), acquired via the
    /// device-code flow (`shunt login xai`) and stored in ~/.shunt/xai-auth.json.
    XaiOauth,
}

/// The dialect of the OpenAI Responses API an upstream speaks. Some backends
/// reject parameters others require, so translation is gated per flavor rather
/// than by hardcoded provider names (AGENTS.md table-driven rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesFlavor {
    /// Stock OpenAI Responses API (api.openai.com and compatible gateways).
    OpenAi,
    /// ChatGPT/Codex backend under /codex/responses — rejects parameters codex
    /// never sends (e.g. `max_output_tokens`).
    Chatgpt,
    /// xAI Grok Responses API — rejects `service_tier`/`text`, and 400s on
    /// `reasoning.effort` for several grok models, so reasoning stays opt-in.
    Xai,
}

/// Whether `host` belongs to xAI (`x.ai` or any subdomain). Used both to gate
/// xai-flavored translation and to reject an `xai_oauth` provider pointed at a
/// non-xAI host, so shunt never leaks a subscription bearer to another origin.
pub fn host_is_xai(host: &str) -> bool {
    host == "x.ai" || host.ends_with(".x.ai")
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
    #[error("config file not found: {}", .0.display())]
    MissingConfigFile(PathBuf),
    #[error("failed to read config file {}: {message}", .path.display())]
    ReadConfigFile { path: PathBuf, message: String },
    #[error("server.bind must be a socket address: {0}")]
    BindAddress(#[from] std::net::AddrParseError),
    #[error("providers.{provider}.base_url must be a valid absolute URL: {message}")]
    ProviderBaseUrl { provider: String, message: String },
    #[error("providers.{provider}.base_url must include a scheme and host")]
    ProviderBaseUrlMissingHost { provider: String },
    #[error("providers.{provider} uses auth = \"api_key\" but api_key_env is not set")]
    MissingApiKeyEnv { provider: String },
    #[error("providers.{provider} uses auth = \"xai_oauth\" but base_url host {host} is not x.ai; refusing to send a subscription token off-origin")]
    XaiOauthNonXaiHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"xai_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    XaiOauthNotHttps { provider: String },
    #[error("providers.{provider} uses auth = \"xai_oauth\" but kind is not \"responses\"; the anthropic adapter would forward the client's own credential instead of the xAI token")]
    XaiOauthWrongKind { provider: String },
    #[error("server.default_provider references unknown provider: {0}")]
    UnknownDefaultProvider(String),
    #[error("route for model {model} references unknown provider: {provider}")]
    UnknownRouteProvider { model: String, provider: String },
    #[error("route prefix {prefix} references unknown provider: {provider}")]
    UnknownPrefixProvider { prefix: String, provider: String },
    #[error("server.auth.header is not a valid header name: {header}")]
    InvalidAuthHeader { header: String },
    #[error("[server.auth] is set but {env} is unset or empty; refusing to run open")]
    MissingClientTokens { env: String },
    #[error("invalid client tokens in {env}: {message}")]
    InvalidClientTokens { env: String, message: String },
    #[error("sentry.dsn is not a valid DSN: {message}")]
    InvalidSentryDsn { message: String },
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
            count_tokens: CountTokens::default(),
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
                    count_tokens: CountTokens::default(),
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
                    count_tokens: CountTokens::default(),
                },
            ),
            (
                // xAI Grok. Defaults to the API-key path (XAI_API_KEY); a
                // subscription user flips `auth = "xai_oauth"` in shunt.toml to
                // reuse a SuperGrok / X Premium+ login (`shunt login xai`).
                "xai".to_string(),
                ProviderConfig {
                    kind: ProviderKind::Responses,
                    base_url: "https://api.x.ai/v1".to_string(),
                    auth: AuthMode::ApiKey,
                    api_key_env: Some("XAI_API_KEY".to_string()),
                    api_key_header: ApiKeyHeader::Bearer,
                    effort: None,
                    count_tokens: CountTokens::default(),
                },
            ),
        ]);
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:3001".to_string(),
                default_provider: "anthropic".to_string(),
                auth: None,
                sse_keepalive_seconds: default_sse_keepalive_seconds(),
            },
            providers,
            models: Vec::new(),
            routes: Vec::new(),
            route_prefixes: Vec::new(),
            sentry: None,
        }
    }
}

/// Standard config search order: `./shunt.toml`, then
/// `$XDG_CONFIG_HOME/shunt/shunt.toml` (defaulting to `~/.config`), then
/// `<homebrew prefix>/etc/shunt.toml` (`$HOMEBREW_PREFIX`, or the stock
/// `/opt/homebrew` and `/usr/local` prefixes when unset).
fn config_file_candidates(
    xdg_config_home: Option<PathBuf>,
    homebrew_prefix: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("./shunt.toml")];
    if let Some(dir) = xdg_config_home {
        candidates.push(dir.join("shunt").join("shunt.toml"));
    }
    let brew_prefixes = match homebrew_prefix {
        Some(prefix) => vec![prefix],
        None => vec![PathBuf::from("/opt/homebrew"), PathBuf::from("/usr/local")],
    };
    for prefix in brew_prefixes {
        candidates.push(prefix.join("etc").join("shunt.toml"));
    }
    candidates
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(path) => Some(path.to_path_buf()),
            None => Self::find_config_file(),
        };
        let mut figment = Figment::from(Serialized::defaults(Self::default()));
        if let Some(path) = &path {
            // Read the file ourselves instead of `Toml::file`, which silently
            // yields an empty provider for a missing file — a typo'd --config
            // or a file deleted after discovery must error, not fall back to
            // defaults while the boot log claims the file was loaded.
            let raw = std::fs::read_to_string(path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ConfigError::MissingConfigFile(path.clone())
                } else {
                    ConfigError::ReadConfigFile {
                        path: path.clone(),
                        message: error.to_string(),
                    }
                }
            })?;
            figment = figment.merge(Toml::string(&raw));
        }
        let config: Self = figment
            .merge(Env::prefixed("SHUNT_").split("__"))
            .extract()
            .map_err(Box::new)?;
        let config = config.validate()?;
        // Logged only after validation so a rejected config never boots with a
        // misleading "loaded config" line.
        match &path {
            Some(path) => tracing::info!(path = %path.display(), "loaded config"),
            None => tracing::info!("no config file found, using defaults"),
        }
        Ok(config)
    }

    /// First existing file from the standard search order used when no
    /// `--config` is given.
    fn find_config_file() -> Option<PathBuf> {
        let xdg_config_home = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
            _ => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")),
        };
        let homebrew_prefix = std::env::var_os("HOMEBREW_PREFIX")
            .filter(|prefix| !prefix.is_empty())
            .map(PathBuf::from);
        config_file_candidates(xdg_config_home, homebrew_prefix)
            .into_iter()
            .find(|path| path.is_file())
    }

    pub fn validate(self) -> Result<Self, ConfigError> {
        self.server.bind_addr()?;
        // Fail closed at boot: [server.auth] without resolvable tokens is an
        // error, not an open gateway.
        if let Some(auth) = &self.server.auth {
            auth.resolve()?;
        }
        // A [sentry] section with a non-empty DSN must parse at boot; a typo'd
        // DSN silently dropping every report would defeat the point of opting in.
        if let Some(sentry) = &self.sentry {
            if sentry.enabled() {
                sentry.dsn.parse::<sentry::types::Dsn>().map_err(|error| {
                    ConfigError::InvalidSentryDsn {
                        message: error.to_string(),
                    }
                })?;
            }
        }
        for (name, provider) in &self.providers {
            let url = self.provider_base_url(name, &provider.base_url)?;
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
            // An xai_oauth provider injects the operator's subscription bearer,
            // so its base_url must stay on an xAI host over https (mirrors
            // Hermes' endpoint re-validation) — never a gateway that would
            // receive it, and never plaintext. It must also be a Responses
            // provider: the anthropic adapter has no XaiOauth injection and
            // would forward the client's own credential to the upstream.
            if provider.auth == AuthMode::XaiOauth {
                if provider.kind != ProviderKind::Responses {
                    return Err(ConfigError::XaiOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                if url.scheme() != "https" {
                    return Err(ConfigError::XaiOauthNotHttps {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                if !host_is_xai(host) {
                    return Err(ConfigError::XaiOauthNonXaiHost {
                        provider: name.clone(),
                        host: host.to_string(),
                    });
                }
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

    /// Which Responses dialect a provider speaks, so translation can gate the
    /// per-backend quirks (see [`ResponsesFlavor`]). Detected from `auth` and
    /// the base_url host rather than provider names: the ChatGPT/Codex backend
    /// by its OAuth mode, xAI by its host (covers both the API-key `xai`
    /// provider and an `xai_oauth` one), everything else stock OpenAI.
    pub fn responses_flavor(&self, provider: &str) -> ResponsesFlavor {
        let Some(provider) = self.provider(provider) else {
            return ResponsesFlavor::OpenAi;
        };
        if provider.auth == AuthMode::ChatgptOauth {
            return ResponsesFlavor::Chatgpt;
        }
        let host = reqwest::Url::parse(&provider.base_url)
            .ok()
            .and_then(|url| url.host_str().map(ToOwned::to_owned))
            .unwrap_or_default();
        if host_is_xai(&host) {
            ResponsesFlavor::Xai
        } else {
            ResponsesFlavor::OpenAi
        }
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

    use super::{
        config_file_candidates, AuthMode, Config, ConfigError, ModelConfig, ProviderKind,
        ResponsesFlavor,
    };

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
    fn default_seeds_builtin_xai_provider() {
        let config = Config::default();
        let xai = config.provider("xai").unwrap();
        assert_eq!(xai.kind, ProviderKind::Responses);
        assert_eq!(xai.base_url, "https://api.x.ai/v1");
        assert_eq!(xai.auth, AuthMode::ApiKey);
        assert_eq!(xai.api_key_env.as_deref(), Some("XAI_API_KEY"));
        // The API-key xai provider still speaks the xai Responses dialect.
        assert_eq!(config.responses_flavor("xai"), ResponsesFlavor::Xai);
        assert_eq!(config.responses_flavor("openai"), ResponsesFlavor::OpenAi);
        assert_eq!(config.responses_flavor("codex"), ResponsesFlavor::Chatgpt);
    }

    #[test]
    fn xai_oauth_provider_validates_and_rejects_non_xai_host() {
        // Flipping the built-in xai provider to oauth is accepted (x.ai host).
        let mut config = Config::default();
        config.providers.get_mut("xai").unwrap().auth = AuthMode::XaiOauth;
        config.providers.get_mut("xai").unwrap().api_key_env = None;
        let config = config.validate().unwrap();
        assert_eq!(config.responses_flavor("xai"), ResponsesFlavor::Xai);

        // Pointing an xai_oauth provider off-origin is refused (bearer-leak guard).
        let mut config = Config::default();
        let provider = config.providers.get_mut("xai").unwrap();
        provider.auth = AuthMode::XaiOauth;
        provider.api_key_env = None;
        provider.base_url = "https://evil.example.com/v1".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::XaiOauthNonXaiHost { .. }));
        assert!(error.to_string().contains("evil.example.com"));
    }

    #[test]
    fn xai_oauth_requires_https_base_url() {
        let mut config = Config::default();
        let provider = config.providers.get_mut("xai").unwrap();
        provider.auth = AuthMode::XaiOauth;
        provider.api_key_env = None;
        provider.base_url = "http://api.x.ai/v1".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::XaiOauthNotHttps { .. }));
        assert!(error.to_string().contains("plaintext"));
    }

    #[test]
    fn xai_oauth_requires_responses_kind() {
        // An anthropic-kind provider never injects the XaiOauth credential —
        // the anthropic adapter would forward the client's own headers — so
        // the combination is rejected at boot.
        let mut config = Config::default();
        let provider = config.providers.get_mut("xai").unwrap();
        provider.auth = AuthMode::XaiOauth;
        provider.api_key_env = None;
        provider.kind = ProviderKind::Anthropic;
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::XaiOauthWrongKind { .. }));
    }

    #[test]
    fn xai_oauth_accepts_x_ai_subdomain() {
        let mut config = Config::default();
        let provider = config.providers.get_mut("xai").unwrap();
        provider.auth = AuthMode::XaiOauth;
        provider.api_key_env = None;
        provider.base_url = "https://api.x.ai/v1".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sentry_is_disabled_by_default() {
        let config = Config::default();
        assert!(config.sentry.is_none());
    }

    #[test]
    fn sentry_section_with_valid_dsn_validates() {
        let config = Config {
            sentry: Some(super::SentryConfig {
                dsn: "https://public@o0.ingest.sentry.io/1234".to_string(),
                environment: Some("home-lab".to_string()),
                metrics: false,
            }),
            ..Config::default()
        };
        let config = config.validate().unwrap();
        assert!(config.sentry.as_ref().unwrap().enabled());
    }

    #[test]
    fn sentry_metrics_default_off_and_parse_from_toml() {
        // `metrics` is a separate opt-in on top of error reporting.
        use figment::providers::{Format, Toml};
        let dsn = "dsn = \"https://public@o0.ingest.sentry.io/1234\"";
        let sentry: super::SentryConfig =
            figment::Figment::from(Toml::string(dsn)).extract().unwrap();
        assert!(!sentry.metrics);
        let sentry: super::SentryConfig =
            figment::Figment::from(Toml::string(&format!("{dsn}\nmetrics = true")))
                .extract()
                .unwrap();
        assert!(sentry.metrics);
    }

    #[test]
    fn sentry_invalid_dsn_is_rejected_at_boot() {
        let config = Config {
            sentry: Some(super::SentryConfig {
                dsn: "not-a-dsn".to_string(),
                environment: None,
                metrics: false,
            }),
            ..Config::default()
        };
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidSentryDsn { .. }));
    }

    #[test]
    fn sentry_empty_dsn_disables_reporting_and_validates() {
        // SHUNT_SENTRY__DSN="" must be able to switch a TOML section off.
        let config = Config {
            sentry: Some(super::SentryConfig {
                dsn: "".to_string(),
                environment: None,
                metrics: false,
            }),
            ..Config::default()
        };
        let config = config.validate().unwrap();
        assert!(!config.sentry.as_ref().unwrap().enabled());
    }

    #[test]
    fn load_errors_when_explicit_config_path_is_missing() {
        let path = std::path::Path::new("./no-such-shunt-config.toml");
        let error = Config::load(Some(path)).unwrap_err();
        assert!(matches!(error, ConfigError::MissingConfigFile(_)));
        assert!(error.to_string().contains("no-such-shunt-config.toml"));
    }

    #[test]
    fn config_file_candidates_follow_search_order() {
        let candidates = config_file_candidates(
            Some(std::path::PathBuf::from("/home/u/.config")),
            Some(std::path::PathBuf::from("/opt/homebrew")),
        );
        let candidates: Vec<_> = candidates
            .iter()
            .map(|path| path.to_str().unwrap())
            .collect();
        assert_eq!(
            candidates,
            [
                "./shunt.toml",
                "/home/u/.config/shunt/shunt.toml",
                "/opt/homebrew/etc/shunt.toml",
            ]
        );
    }

    #[test]
    fn config_file_candidates_try_stock_brew_prefixes_when_env_is_unset() {
        let candidates = config_file_candidates(None, None);
        let candidates: Vec<_> = candidates
            .iter()
            .map(|path| path.to_str().unwrap())
            .collect();
        assert_eq!(
            candidates,
            [
                "./shunt.toml",
                "/opt/homebrew/etc/shunt.toml",
                "/usr/local/etc/shunt.toml",
            ]
        );
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
