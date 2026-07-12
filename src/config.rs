use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use figment::{
    providers::{Env, Format, Serialized, Toml, Yaml},
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
    /// Optional opt-in OpenTelemetry (OTLP) export. Absent (the default) means
    /// no exporter is created and nothing ever leaves the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel: Option<OtelConfig>,
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

/// `[otel]` — opt-in OpenTelemetry export to the operator's own OTLP endpoint
/// (an OpenTelemetry Collector or a compatible backend). Absent (the default)
/// means no exporter is created and nothing leaves the machine. Independent of
/// `[sentry]`: both are separate opt-ins and can run together. Metrics
/// (provider/model/status) and traces (HTTP method/path; the client session id
/// only when `include_session_id` is set) stay low-cardinality and carry no
/// request/response bodies. The `logs` signal, when on, exports shunt's
/// diagnostic log events as written — so it can include request-derived fields
/// (an upstream error body, a client id); set `logs = false` for body-free
/// export. All signals go only to the configured endpoint.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct OtelConfig {
    /// OTLP/HTTP endpoint base URL, e.g. `http://localhost:4318`. shunt appends
    /// the standard signal paths (`/v1/traces`, `/v1/metrics`, `/v1/logs`). An
    /// empty string disables export, so `SHUNT_OTEL__ENDPOINT=""` turns a
    /// file-configured section off without editing it.
    pub endpoint: String,
    /// `service.name` resource attribute on all exported telemetry.
    #[serde(default = "default_otel_service_name")]
    pub service_name: String,
    /// Optional `deployment.environment` resource attribute (e.g. "prod").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Head-based trace sampling ratio in `[0.0, 1.0]`. `1.0` (default) samples
    /// every request span; lower values reduce export volume.
    #[serde(default = "default_otel_sample_ratio")]
    pub sample_ratio: f64,
    /// Extra headers on every OTLP request — e.g. an auth token for a hosted
    /// collector: `authorization = "Bearer …"`. Values can be secrets; keep
    /// them out of shared configs (prefer `SHUNT_OTEL__HEADERS__…` in the env).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Export trace spans (the per-request `proxy_request` span). On by default.
    #[serde(default = "default_true")]
    pub traces: bool,
    /// Export usage metrics (request counts + latency). On by default. Mirrors
    /// the Sentry `shunt.requests`/`shunt.latency` series to OTLP.
    #[serde(default = "default_true")]
    pub metrics: bool,
    /// Export `tracing` log events as OTLP logs. On by default; independent of
    /// the stderr `fmt` logs, which are unaffected.
    #[serde(default = "default_true")]
    pub logs: bool,
    /// Attach the client session id to request spans. Off by default: session
    /// ids are request-derived and — like the Sentry span filter — are withheld
    /// unless the operator opts in for their own backend.
    #[serde(default)]
    pub include_session_id: bool,
}

fn default_otel_service_name() -> String {
    "shunt".to_string()
}

fn default_otel_sample_ratio() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

impl OtelConfig {
    /// Whether this section actually enables export (non-empty endpoint).
    pub fn enabled(&self) -> bool {
        !self.endpoint.trim().is_empty()
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
    /// Opt in to the Codex Responses WebSocket v2 transport for this provider
    /// (issue #32). Only honored for the ChatGPT/Codex backend; ignored for
    /// stock OpenAI/xAI upstreams, which have no v2 websocket endpoint. When on,
    /// shunt reaches the backend over `wss://…/codex/responses` with the
    /// `responses_websockets` beta protocol, transparently falling back to HTTP
    /// if the websocket cannot be established (a mid-stream failure surfaces as an
    /// error event). Off by default — HTTP stays the default transport.
    #[serde(default)]
    pub websocket: bool,
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
    #[error("otel.endpoint is not a valid URL: {message}")]
    InvalidOtelEndpoint { message: String },
    #[error("otel.sample_ratio must be between 0.0 and 1.0, got {ratio}")]
    InvalidOtelSampleRatio { ratio: f64 },
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
            websocket: false,
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
                    websocket: false,
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
                    websocket: false,
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
                    websocket: false,
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
            otel: None,
        }
    }
}

/// Config file basenames tried in each search directory, in priority order.
/// TOML stays first so an existing `shunt.toml` always wins over a `.yaml`
/// dropped alongside it; `.yaml` is preferred over the terser `.yml`.
const CONFIG_FILENAMES: [&str; 3] = ["shunt.toml", "shunt.yaml", "shunt.yml"];

/// Standard config search directories, in order: the current directory, then
/// `$XDG_CONFIG_HOME/shunt` (defaulting to `~/.config`), then
/// `<homebrew prefix>/etc` (`$HOMEBREW_PREFIX`, or the stock `/opt/homebrew`
/// and `/usr/local` prefixes when unset). Each directory is probed for every
/// name in [`CONFIG_FILENAMES`] before moving on, so a local `shunt.yaml`
/// still wins over a config in a later directory.
fn config_file_candidates(
    xdg_config_home: Option<PathBuf>,
    homebrew_prefix: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(".")];
    if let Some(dir) = xdg_config_home {
        dirs.push(dir.join("shunt"));
    }
    let brew_prefixes = match homebrew_prefix {
        Some(prefix) => vec![prefix],
        None => vec![PathBuf::from("/opt/homebrew"), PathBuf::from("/usr/local")],
    };
    for prefix in brew_prefixes {
        dirs.push(prefix.join("etc"));
    }
    dirs.into_iter()
        .flat_map(|dir| CONFIG_FILENAMES.iter().map(move |name| dir.join(name)))
        .collect()
}

/// A config file's serialization format, selected by its extension so both
/// `--config foo.yaml` and a discovered `shunt.yaml` are parsed as YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFormat {
    Toml,
    Yaml,
}

impl ConfigFormat {
    /// Detect the format from a path's extension. `.yaml`/`.yml` (any case)
    /// are YAML; everything else — including no extension — is TOML, which
    /// preserves the historical `shunt.toml` default.
    fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") => {
                ConfigFormat::Yaml
            }
            _ => ConfigFormat::Toml,
        }
    }
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
            // The parser is chosen by extension so TOML and YAML configs are
            // both accepted; an unknown extension is treated as TOML.
            figment = match ConfigFormat::from_path(path) {
                ConfigFormat::Toml => figment.merge(Toml::string(&raw)),
                ConfigFormat::Yaml => figment.merge(Yaml::string(&raw)),
            };
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
    /// `--config` is given. Public so the binary can resolve the effective path
    /// once at startup and reuse it for hot-reload/file-watch.
    pub fn find_config_file() -> Option<PathBuf> {
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
        // An [otel] section with a non-empty endpoint must parse as a URL at
        // boot; a typo'd endpoint silently dropping every export would defeat
        // the point of opting in. The sample ratio must be a valid probability.
        if let Some(otel) = &self.otel {
            if otel.enabled() {
                let endpoint = reqwest::Url::parse(&otel.endpoint).map_err(|error| {
                    ConfigError::InvalidOtelEndpoint {
                        message: error.to_string(),
                    }
                })?;
                if !(0.0..=1.0).contains(&otel.sample_ratio) {
                    return Err(ConfigError::InvalidOtelSampleRatio {
                        ratio: otel.sample_ratio,
                    });
                }
                // `[otel.headers]` can carry a secret (e.g. a collector bearer);
                // over plaintext http to a non-loopback host it would travel in
                // the clear. Warn (don't reject — http to a local collector is a
                // legitimate default) so a misconfigured remote endpoint is not
                // silent, mirroring the codebase's care about leaking bearers.
                let host_is_loopback = endpoint
                    .host_str()
                    .map(|host| host == "localhost" || host == "::1" || host.starts_with("127."))
                    .unwrap_or(false);
                if !otel.headers.is_empty() && endpoint.scheme() == "http" && !host_is_loopback {
                    tracing::warn!(
                        host = endpoint.host_str().unwrap_or_default(),
                        "[otel.headers] are sent over plaintext http to a non-loopback endpoint; use https so a collector token is not transmitted in the clear"
                    );
                }
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

    /// Resolve `[server.auth]` into the runtime inbound-auth state, reading the
    /// configured tokens env. `None` when inbound auth is not configured. Fails
    /// closed (see [`InboundAuthConfig::resolve`]). Shared by `build_router` and
    /// the hot-reload path so both re-resolve tokens identically.
    pub fn resolve_inbound_auth(
        &self,
    ) -> Result<Option<std::sync::Arc<crate::auth::inbound::InboundAuth>>, ConfigError> {
        self.server
            .auth
            .as_ref()
            .map(|auth| auth.resolve())
            .transpose()
            .map(|auth| auth.map(std::sync::Arc::new))
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

    /// Whether the Codex Responses WebSocket v2 transport should be used for
    /// `provider`. Requires both the opt-in `websocket` flag and the ChatGPT/Codex
    /// backend: only that backend serves the `responses_websockets` v2 endpoint,
    /// so the flag is inert on stock OpenAI/xAI providers.
    pub fn codex_websocket_enabled(&self, provider: &str) -> bool {
        self.provider(provider)
            .map(|config| config.websocket && config.auth == AuthMode::ChatgptOauth)
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
        config_file_candidates, AuthMode, Config, ConfigError, ConfigFormat, ModelConfig,
        ProviderKind, ResponsesFlavor,
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
    fn codex_websocket_gated_on_flag_and_chatgpt_backend() {
        // Off by default, even for the ChatGPT/Codex backend.
        let config = Config::default();
        assert!(!config.codex_websocket_enabled("codex"));

        // Flag on + ChatGPT backend ⇒ enabled.
        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().websocket = true;
        assert!(config.codex_websocket_enabled("codex"));

        // Flag on but not the ChatGPT backend (stock OpenAI) ⇒ inert: no v2
        // websocket endpoint exists there.
        let mut config = Config::default();
        config.providers.get_mut("openai").unwrap().websocket = true;
        assert!(!config.codex_websocket_enabled("openai"));

        // Unknown provider ⇒ false.
        assert!(!config.codex_websocket_enabled("nope"));
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

    fn otel_config(endpoint: &str) -> super::OtelConfig {
        super::OtelConfig {
            endpoint: endpoint.to_string(),
            service_name: super::default_otel_service_name(),
            environment: None,
            sample_ratio: super::default_otel_sample_ratio(),
            headers: std::collections::BTreeMap::new(),
            traces: true,
            metrics: true,
            logs: true,
            include_session_id: false,
        }
    }

    #[test]
    fn otel_is_disabled_by_default() {
        let config = Config::default();
        assert!(config.otel.is_none());
    }

    #[test]
    fn otel_section_with_valid_endpoint_validates() {
        let config = Config {
            otel: Some(otel_config("http://localhost:4318")),
            ..Config::default()
        };
        let config = config.validate().unwrap();
        assert!(config.otel.as_ref().unwrap().enabled());
    }

    #[test]
    fn otel_invalid_endpoint_is_rejected_at_boot() {
        let config = Config {
            otel: Some(otel_config("not a url")),
            ..Config::default()
        };
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidOtelEndpoint { .. }));
    }

    #[test]
    fn otel_sample_ratio_out_of_range_is_rejected() {
        let mut otel = otel_config("http://localhost:4318");
        otel.sample_ratio = 1.5;
        let config = Config {
            otel: Some(otel),
            ..Config::default()
        };
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidOtelSampleRatio { .. }));
    }

    #[test]
    fn otel_empty_endpoint_disables_export_and_validates() {
        // SHUNT_OTEL__ENDPOINT="" must be able to switch a file section off,
        // and a disabled section skips endpoint/ratio validation entirely.
        let mut otel = otel_config("");
        otel.sample_ratio = 99.0; // ignored while disabled
        let config = Config {
            otel: Some(otel),
            ..Config::default()
        };
        let config = config.validate().unwrap();
        assert!(!config.otel.as_ref().unwrap().enabled());
    }

    #[test]
    fn otel_defaults_parse_from_toml() {
        use figment::providers::{Format, Toml};
        let otel: super::OtelConfig =
            figment::Figment::from(Toml::string("endpoint = \"http://localhost:4318\""))
                .extract()
                .unwrap();
        assert_eq!(otel.service_name, "shunt");
        assert_eq!(otel.sample_ratio, 1.0);
        assert!(otel.traces && otel.metrics && otel.logs);
        assert!(!otel.include_session_id);
        assert!(otel.headers.is_empty());
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
                "./shunt.yaml",
                "./shunt.yml",
                "/home/u/.config/shunt/shunt.toml",
                "/home/u/.config/shunt/shunt.yaml",
                "/home/u/.config/shunt/shunt.yml",
                "/opt/homebrew/etc/shunt.toml",
                "/opt/homebrew/etc/shunt.yaml",
                "/opt/homebrew/etc/shunt.yml",
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
                "./shunt.yaml",
                "./shunt.yml",
                "/opt/homebrew/etc/shunt.toml",
                "/opt/homebrew/etc/shunt.yaml",
                "/opt/homebrew/etc/shunt.yml",
                "/usr/local/etc/shunt.toml",
                "/usr/local/etc/shunt.yaml",
                "/usr/local/etc/shunt.yml",
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

    #[test]
    fn config_format_is_selected_by_extension() {
        use std::path::Path;
        assert_eq!(
            ConfigFormat::from_path(Path::new("shunt.toml")),
            ConfigFormat::Toml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("shunt.yaml")),
            ConfigFormat::Yaml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("shunt.yml")),
            ConfigFormat::Yaml
        );
        // Case-insensitive, and an unknown/absent extension falls back to TOML.
        assert_eq!(
            ConfigFormat::from_path(Path::new("/etc/shunt.YAML")),
            ConfigFormat::Yaml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("shunt.conf")),
            ConfigFormat::Toml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("shunt")),
            ConfigFormat::Toml
        );
    }

    #[test]
    fn yaml_adds_a_provider_and_merges_builtin_overrides() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-yaml-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // RAII guard so the temp dir is removed even if an assertion below
        // panics (mirrors the pattern in main.rs's run test).
        struct TempDirGuard(std::path::PathBuf);
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = TempDirGuard(dir.clone());

        let path = dir.join("shunt.yaml");
        std::fs::write(
            &path,
            r#"
providers:
  kimi:
    kind: anthropic
    base_url: https://api.moonshot.ai/anthropic
    auth: api_key
    api_key_env: KIMI_API_KEY
  codex:
    effort: high
routes:
  - model: kimi-k2.7-code
    provider: kimi
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();

        // New provider added from YAML.
        let kimi = config.provider("kimi").unwrap();
        assert_eq!(kimi.kind, ProviderKind::Anthropic);
        assert_eq!(kimi.auth, AuthMode::ApiKey);
        assert_eq!(kimi.api_key_env.as_deref(), Some("KIMI_API_KEY"));
        // Built-in codex kept its default base_url/auth while gaining effort,
        // so YAML deep-merges over the seeded defaults just like TOML does.
        let codex = config.provider("codex").unwrap();
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(codex.auth, AuthMode::ChatgptOauth);
        assert_eq!(codex.effort.as_deref(), Some("high"));
        // The YAML route is applied.
        assert!(config
            .routes
            .iter()
            .any(|route| route.model == "kimi-k2.7-code" && route.provider == "kimi"));
    }
}
