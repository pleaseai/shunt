use std::{
    collections::{BTreeMap, HashSet},
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
    /// Optional opt-in admin web surface (M9). Absent ⇒ no admin routes are
    /// registered at all (today's HTTP surface unchanged). See
    /// `docs/m9-admin-surface.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminConfig>,
    /// Optional OAuth device-flow login and per-user managed-policy surface for
    /// Claude apps. Absent ⇒ discovery, device approval, token, and managed
    /// settings routes are not registered. Secrets, static users, and policies
    /// are resolved into the hot-reloadable gateway snapshot.
    /// See `docs/gateway-login.md` and `docs/gateway-managed-settings.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayConfig>,
    /// Optional opt-in inbound OpenAI Responses (Codex) endpoint. Absent ⇒ no
    /// `/responses` routes are registered at all (today's HTTP surface
    /// unchanged). When set, the Codex CLI can point its `chatgpt_base_url` (or
    /// a custom `model_provider`) at shunt and be load-balanced across the named
    /// provider's ChatGPT/Codex account pool. See
    /// `docs/m11-inbound-codex-endpoint.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_endpoint: Option<CodexEndpointConfig>,
    /// Optional opt-in client-facing usage endpoint (`GET /usage`). Absent ⇒ the
    /// route is not registered (today's HTTP surface unchanged). When set, a
    /// `[server.auth]` client-token holder can read a sanitized, aggregated view
    /// of the shared account pool's quota state. Requires `[server.auth]` (a
    /// non-admin caller must be identifiable) — enforced by [`Config::validate`].
    /// See `docs/m12-client-usage-endpoint.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageEndpointConfig>,
    /// Optional account-pool tuning (issue #135) and opt-in usage-API
    /// reconciliation. Absent ⇒ legacy quota selection and no background polling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolConfig>,
    /// Idle seconds before shunt injects an SSE `ping` event into a streaming
    /// response so middlebox timers (Cloudflare's 100s → 524) never expire.
    /// `0` disables injection (M5).
    #[serde(default = "default_sse_keepalive_seconds")]
    pub sse_keepalive_seconds: u64,
}

fn default_sse_keepalive_seconds() -> u64 {
    30
}

/// `[server.pool]` — quota-aware load-balancing tuning and optional usage-API
/// reconciliation for Claude (Anthropic) account pools (issue #135). Quota
/// headers exist only on the Anthropic backend, so threshold/burn-rate knobs
/// are inert for Codex pools; per-account `priority`/`disabled` apply to both.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PoolConfig {
    /// Safety backstop common to all quota windows.
    #[serde(default = "default_hard_threshold")]
    pub hard_threshold: f64,
    /// Soft default threshold used when no more specific value is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_threshold_5h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_threshold_7d: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_threshold_fable: Option<f64>,
    /// Avoid an account projected to exhaust a soft threshold before reset.
    #[serde(default)]
    pub burn_rate_avoidance: bool,
    /// Poll `GET /api/oauth/usage` every N seconds for refreshable Claude
    /// accounts. Unset or `0` disables polling; positive values below 60 are
    /// clamped to 60 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_refresh_seconds: Option<u64>,
    /// Persist the pool's per-account quota state to this file so a restart
    /// warm-starts from the last observed utilization instead of an empty pool.
    /// Unset disables persistence (the default). The file is a best-effort
    /// cache, not a source of truth: quota is re-derived from upstream anyway,
    /// so a missing or unreadable file just means a cold start. See
    /// [`crate::state_persist`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_path: Option<PathBuf>,
    /// Storm control (issue #195): cap concurrent admissions to an account
    /// identity that just started taking traffic, so a failover switch cannot
    /// stampede the freshly selected account with every in-flight request at
    /// once. The cap starts here and doubles per successful response
    /// (slow-start), and drops back after a cooldown or an idle period. Unset
    /// or `0` disables admission gating (the default). A pool whose accounts
    /// all resolve to one upstream identity is effectively ungated: the last
    /// remaining candidate is always admitted so gating can never fail a
    /// request, and a single-identity pool only ever has a last candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramp_initial_concurrency: Option<u32>,
}

pub(crate) fn default_hard_threshold() -> f64 {
    0.98
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            hard_threshold: default_hard_threshold(),
            default_threshold: None,
            default_threshold_5h: None,
            default_threshold_7d: None,
            default_threshold_fable: None,
            burn_rate_avoidance: false,
            usage_refresh_seconds: None,
            state_path: None,
            ramp_initial_concurrency: None,
        }
    }
}

impl PoolConfig {
    /// The effective poll interval, or `None` when polling is disabled.
    pub fn usage_refresh_interval(&self) -> Option<u64> {
        match self.usage_refresh_seconds {
            None | Some(0) => None,
            Some(seconds) => Some(seconds.max(60)),
        }
    }

    /// The storm-control initial admission allowance, or `None` when admission
    /// gating is disabled (unset or `0`).
    pub fn storm_ramp_initial(&self) -> Option<u32> {
        self.ramp_initial_concurrency.filter(|&initial| initial > 0)
    }
}

/// `[server.auth]` — inbound client-token check on injected-credential routes
/// and `GET /v1/models`.
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

/// `[server.admin]` — opt-in admin web surface (M9). A **separate** credential
/// from `[server.auth]`: client tokens are handed to devices, admin tokens add
/// upstream accounts. Tokens live in the environment as `name:token` pairs
/// (`SHUNT_ADMIN_TOKENS="ops:3f9c…"`), reusing the inbound-auth format and
/// constant-time compare. Absent ⇒ no admin routes exist. See
/// `docs/m9-admin-surface.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminConfig {
    /// Header carrying the admin token for API/curl calls.
    #[serde(default = "default_admin_header")]
    pub header: String,
    /// Env var holding the `name:token` admin pairs.
    #[serde(default = "default_admin_tokens_env")]
    pub tokens_env: String,
    /// Browser session lifetime after login.
    #[serde(default = "default_admin_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// Pending-login lifetime (time to open the authorize URL and paste back).
    #[serde(default = "default_admin_pending_ttl_secs")]
    pub pending_ttl_secs: u64,
}

fn default_admin_header() -> String {
    "x-shunt-admin-token".to_string()
}

fn default_admin_tokens_env() -> String {
    "SHUNT_ADMIN_TOKENS".to_string()
}

fn default_admin_session_ttl_secs() -> u64 {
    3600
}

fn default_admin_pending_ttl_secs() -> u64 {
    600
}

impl AdminConfig {
    /// Resolve the configured admin tokens from the environment into the runtime
    /// admin-auth state. Fails closed exactly like [`InboundAuthConfig::resolve`]:
    /// a present `[server.admin]` with an unset/empty/malformed env var is a
    /// startup error, never a silently-open admin surface.
    pub fn resolve(&self) -> Result<crate::admin::AdminAuth, ConfigError> {
        let header = axum::http::HeaderName::from_bytes(self.header.as_bytes()).map_err(|_| {
            ConfigError::InvalidAdminHeader {
                header: self.header.clone(),
            }
        })?;
        let raw = std::env::var(&self.tokens_env).unwrap_or_default();
        if raw.trim().is_empty() {
            return Err(ConfigError::MissingAdminTokens {
                env: self.tokens_env.clone(),
            });
        }
        let tokens = crate::auth::inbound::parse_tokens(&raw).map_err(|message| {
            ConfigError::InvalidAdminTokens {
                env: self.tokens_env.clone(),
                message,
            }
        })?;
        Ok(crate::admin::AdminAuth::new(
            crate::auth::inbound::InboundAuth::new(header, tokens),
            std::time::Duration::from_secs(self.session_ttl_secs),
            std::time::Duration::from_secs(self.pending_ttl_secs),
        ))
    }
}

/// `[server.gateway]` — opt-in OAuth device-flow login and managed policy for
/// Claude apps. The public URL is the JWT issuer and base for every advertised
/// OAuth endpoint. Signing material and static approval users live in environment
/// variables, never in the config file. Absent ⇒ no gateway routes exist.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    /// Externally reachable URL used for issuer and OAuth endpoint metadata.
    pub public_url: String,
    /// Env var holding an HS256 signing secret of at least 32 bytes.
    #[serde(default = "default_gateway_jwt_secret_env")]
    pub jwt_secret_env: String,
    /// Env var holding comma-separated `email:secret` approval users.
    #[serde(default = "default_gateway_users_env")]
    pub users_env: String,
    /// Access-token lifetime in seconds.
    #[serde(default = "default_gateway_token_ttl_seconds")]
    pub token_ttl_seconds: u64,
    /// Honor `X-Forwarded-For`/`X-Real-IP` for `/device` rate limiting.
    /// Enable only behind a trusted proxy that replaces client-supplied values.
    #[serde(default)]
    pub trust_forwarded_for: bool,
    /// Ordered per-user managed-settings policies. `None` keeps the endpoint at
    /// its explicit "no managed policy" 404 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<GatewayPolicyConfig>>,
    /// Client telemetry configuration. M-B uses this only to push the telemetry
    /// enable flag plus five `OTEL_*` environment variables; the inbound relay
    /// routes arrive in M-C (#189).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<GatewayTelemetryConfig>,
    /// File persisting refresh sessions across restarts (issue #194). Refresh
    /// tokens are stored as SHA-256 hashes, written atomically with owner-only
    /// permissions after each grant or rotation, and restored at boot. Defaults
    /// to `~/.shunt/gateway-sessions.json` (the directory shunt's account
    /// stores already use); set `state_path = ""` for memory-only sessions,
    /// where a restart signs everyone out once their access JWT expires. When
    /// no home directory can be resolved the default is memory-only too.
    #[serde(default = "default_gateway_state_path")]
    pub state_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayPolicyConfig {
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<GatewayPolicyMatch>,
    /// Open-schema `managed-settings.json` document.
    pub cli: toml::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayPolicyMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emails: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GatewayTelemetryConfig {
    #[serde(default)]
    pub forward_to: Vec<GatewayTelemetryDestination>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayTelemetryDestination {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

/// `~/.shunt/gateway-sessions.json` (`HOME`, falling back to `USERPROFILE` on
/// Windows), or `None` — memory-only sessions — when neither is set. Unlike
/// the account stores this never falls back to a working-directory-relative
/// path: a default-on write should not land in whatever directory shunt
/// happens to start from.
fn default_gateway_state_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".shunt").join("gateway-sessions.json"))
}

fn default_gateway_jwt_secret_env() -> String {
    "SHUNT_GATEWAY_JWT_SECRET".to_string()
}

fn default_gateway_users_env() -> String {
    "SHUNT_GATEWAY_USERS".to_string()
}

fn default_gateway_token_ttl_seconds() -> u64 {
    3600
}

impl GatewayConfig {
    /// The effective session state file: the configured (or defaulted) path,
    /// with the empty string (`state_path = ""`) meaning memory-only.
    pub fn session_state_path(&self) -> Option<&std::path::Path> {
        self.state_path
            .as_deref()
            .filter(|path| !path.as_os_str().is_empty())
    }

    pub fn resolve(&self) -> Result<crate::gateway::GatewayAuth, ConfigError> {
        let public_url = reqwest::Url::parse(self.public_url.trim()).map_err(|error| {
            ConfigError::InvalidGatewayPublicUrl {
                message: error.to_string(),
            }
        })?;
        let secure_origin = public_url.scheme() == "https"
            || public_url.scheme() == "http"
                && host_is_loopback(public_url.host_str().unwrap_or_default());
        let bare_origin = public_url.host_str().is_some()
            && public_url.username().is_empty()
            && public_url.password().is_none()
            && public_url.path() == "/"
            && public_url.query().is_none()
            && public_url.fragment().is_none();
        if !secure_origin || !bare_origin {
            return Err(ConfigError::InvalidGatewayPublicUrl {
                message: "must be an https origin (http is allowed only on loopback) with no userinfo, path, query, or fragment"
                    .to_string(),
            });
        }
        if self.token_ttl_seconds == 0 {
            return Err(ConfigError::InvalidGatewayTokenTtl);
        }
        let secret = std::env::var(&self.jwt_secret_env).unwrap_or_default();
        if secret.len() < 32 {
            return Err(ConfigError::InvalidGatewayJwtSecret {
                env: self.jwt_secret_env.clone(),
            });
        }
        let raw_users = std::env::var(&self.users_env).unwrap_or_default();
        if raw_users.trim().is_empty() {
            return Err(ConfigError::MissingGatewayUsers {
                env: self.users_env.clone(),
            });
        }
        let users =
            crate::gateway::approval::StaticUsers::parse(&raw_users).map_err(|message| {
                ConfigError::InvalidGatewayUsers {
                    env: self.users_env.clone(),
                    message,
                }
            })?;
        let policies = resolve_gateway_policies(self.policies.as_deref())?;
        let telemetry_push = validate_gateway_telemetry(self.telemetry.as_ref())?;
        Ok(crate::gateway::GatewayAuth::new(
            public_url.as_str().trim_end_matches('/').to_string(),
            secret.into_bytes(),
            self.token_ttl_seconds,
            self.trust_forwarded_for,
            users,
        )
        .with_managed_policies(policies, telemetry_push))
    }
}

fn resolve_gateway_policies(
    policies: Option<&[GatewayPolicyConfig]>,
) -> Result<Option<Vec<crate::gateway::managed::ResolvedPolicy>>, ConfigError> {
    policies
        .map(|policies| {
            if policies.is_empty() {
                return Err(ConfigError::EmptyGatewayPolicies);
            }
            policies
                .iter()
                .enumerate()
                .map(resolve_gateway_policy)
                .collect()
        })
        .transpose()
}

fn resolve_gateway_policy(
    (index, policy): (usize, &GatewayPolicyConfig),
) -> Result<crate::gateway::managed::ResolvedPolicy, ConfigError> {
    let emails = policy
        .matcher
        .as_ref()
        .and_then(|matcher| matcher.emails.as_ref())
        .map(|emails| validate_gateway_policy_emails(emails, index))
        .transpose()?;
    let settings = toml_to_json(&policy.cli)
        .map_err(|key| ConfigError::InvalidGatewayPolicyValue { index, key })?;
    let settings = settings
        .as_object()
        .ok_or(ConfigError::InvalidGatewayPolicyCli { index })?;
    validate_managed_policy(settings, index)?;
    Ok(crate::gateway::managed::ResolvedPolicy {
        emails,
        settings: serde_json::Value::Object(settings.clone()),
    })
}

fn validate_gateway_policy_emails(
    emails: &[String],
    index: usize,
) -> Result<Vec<String>, ConfigError> {
    if emails.is_empty() {
        return Err(ConfigError::EmptyGatewayPolicyEmails { index });
    }
    if let Some(email_index) = emails.iter().position(|email| email.trim().is_empty()) {
        return Err(ConfigError::EmptyGatewayPolicyEmail { index, email_index });
    }
    Ok(emails
        .iter()
        .map(|email| email.trim().to_string())
        .collect())
}

fn validate_gateway_telemetry(
    telemetry: Option<&GatewayTelemetryConfig>,
) -> Result<bool, ConfigError> {
    let Some(telemetry) = telemetry else {
        return Ok(false);
    };
    for (index, destination) in telemetry.forward_to.iter().enumerate() {
        validate_gateway_telemetry_destination(destination, index)?;
    }
    Ok(!telemetry.forward_to.is_empty())
}

fn validate_gateway_telemetry_destination(
    destination: &GatewayTelemetryDestination,
    index: usize,
) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(destination.url.trim()).map_err(|error| {
        ConfigError::InvalidGatewayTelemetryUrl {
            index,
            message: error.to_string(),
        }
    })?;
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
        return Ok(());
    }
    Err(ConfigError::InvalidGatewayTelemetryUrl {
        index,
        message: format!(
            "must be an http(s) URL with a host, got `{}`",
            destination.url
        ),
    })
}

fn validate_managed_policy(
    settings: &serde_json::Map<String, serde_json::Value>,
    index: usize,
) -> Result<(), ConfigError> {
    if let Some(available_models) = settings.get("availableModels") {
        let valid = available_models
            .as_array()
            .is_some_and(|models| models.iter().all(serde_json::Value::is_string));
        if !valid {
            return Err(ConfigError::InvalidGatewayAvailableModels { index });
        }
    }
    if let Some(env) = settings.get("env") {
        let valid = env.as_object().is_some_and(|env| {
            env.values()
                .all(|value| value.is_string() || value.is_number() || value.is_boolean())
        });
        if !valid {
            return Err(ConfigError::InvalidGatewayPolicyEnv { index });
        }
    }
    Ok(())
}

fn toml_to_json(value: &toml::Value) -> Result<serde_json::Value, String> {
    match value {
        toml::Value::String(value) => Ok(serde_json::Value::String(value.clone())),
        toml::Value::Integer(value) => Ok(serde_json::Value::Number((*value).into())),
        toml::Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "non-finite float".to_string()),
        toml::Value::Boolean(value) => Ok(serde_json::Value::Bool(*value)),
        toml::Value::Datetime(value) => Ok(serde_json::Value::String(value.to_string())),
        toml::Value::Array(values) => Ok(serde_json::Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| toml_to_json(value).map_err(|key| format!("[{index}]{key}")))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        toml::Value::Table(values) => Ok(serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    toml_to_json(value)
                        .map(|value| (key.clone(), value))
                        .map_err(|child| format!(".{key}{child}"))
                })
                .collect::<Result<serde_json::Map<_, _>, _>>()?,
        )),
    }
}

/// `[server.codex_endpoint]` — opt-in inbound OpenAI Responses (Codex) endpoint.
/// When present, shunt registers `POST /backend-api/codex/responses`,
/// `POST /responses`, and `POST /v1/responses`, and proxies each request through
/// the named provider's ChatGPT/Codex account pool without translating it to or
/// from Anthropic Messages (a raw passthrough). Absent ⇒ none of those routes
/// exist. See `docs/m11-inbound-codex-endpoint.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodexEndpointConfig {
    /// Which `chatgpt_oauth` provider's account pool serves inbound Responses
    /// requests. Every inbound request is routed to this one provider (the body
    /// `model` is forwarded upstream verbatim, not used to pick a provider), so
    /// it must exist and use `auth = "chatgpt_oauth"`. Defaults to the built-in
    /// `codex` provider.
    #[serde(default = "default_codex_endpoint_provider")]
    pub provider: String,
}

fn default_codex_endpoint_provider() -> String {
    "codex".to_string()
}

/// `[server.usage]` — opt-in client-facing usage endpoint. When present, shunt
/// registers `GET /usage`, which returns a **sanitized, aggregated** view of the
/// shared account pool's quota state (per-window remaining headroom and reset)
/// for `[server.auth]` client-token holders. Unlike the admin dashboard
/// (`GET /admin/pool`), it never exposes account identities, counts, priorities,
/// disabled flags, or thresholds. Presence alone opts in; the table has no
/// fields today. Requires `[server.auth]`. Absent ⇒ the route does not exist.
/// See `docs/m12-client-usage-endpoint.md`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UsageEndpointConfig {}

/// `[sentry]` — opt-in error reporting to the operator's own Sentry project.
/// Only gateway-owned diagnostics are reported (fatal startup/serve errors,
/// panics, and `error!` log events, with `warn!`/`info!` as breadcrumbs);
/// request/response bodies, headers, and credentials never are. Metrics and
/// performance tracing are each a further, separate opt-in (`metrics` /
/// `traces_sample_rate`).
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
    /// Also send performance traces: the per-request `proxy_request` span
    /// becomes a Sentry transaction, head-sampled at this rate in `[0.0,
    /// 1.0]`. `0.0` (default) keeps tracing off entirely — spans never reach
    /// the Sentry layer. A separate opt-in from error reporting and metrics,
    /// mirroring `[otel] sample_ratio`.
    #[serde(default)]
    pub traces_sample_rate: f64,
    /// Attach the client session id to request spans sent to Sentry. Off by
    /// default: session ids are request-derived and — exactly like `[otel]
    /// include_session_id` — are withheld unless the operator opts in for
    /// their own backend. Only meaningful while `traces_sample_rate > 0`.
    #[serde(default)]
    pub include_session_id: bool,
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
    /// Explicit OAuth accounts for a `claude_oauth` (Anthropic) or
    /// `chatgpt_oauth` (Codex) provider. An empty list means the account store
    /// directory will be scanned by the account-pool layer.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    /// Opt in to the Codex Responses WebSocket v2 transport for this provider
    /// (issue #32). Only honored for the ChatGPT/Codex backend; ignored for
    /// stock OpenAI/xAI upstreams, which have no v2 websocket endpoint. When on,
    /// shunt reaches the backend over `wss://…/codex/responses` with the
    /// `responses_websockets` beta protocol, transparently falling back to HTTP
    /// if the websocket cannot be established (a mid-stream failure surfaces as an
    /// error event). Off by default — HTTP stays the default transport.
    #[serde(default)]
    pub websocket: bool,
    /// Opt in to the OpenAI Responses native client-executed `tool_search`
    /// protocol for Claude Code's tool search (issue #82). Off by default: when
    /// off, shunt keeps the #43 text-based progressive-reveal compatibility shim
    /// (ToolSearch forwarded as a plain function, `tool_reference` revealed as
    /// schema text). When on — and the upstream flavor and model support it (see
    /// [`Config::native_tool_search`]) — shunt maps Claude Code's `ToolSearch` to
    /// the native `tool_search`/`tool_search_call`/`tool_search_output` items so
    /// tool-loading semantics and cache behavior are preserved. Gated behind this
    /// flag until a live probe confirms a given backend accepts the shapes shunt
    /// emits; unsupported flavors/models fall back to the shim regardless.
    #[serde(default)]
    pub tool_search: bool,
    /// Bounded upstream retry/backoff for transient failures (issue #48).
    /// Applies to this provider's single-credential upstream calls (the
    /// `passthrough`/`api_key` Anthropic path, the single-credential Responses
    /// path — `api_key`, `xai_oauth`, or an unpooled `chatgpt_oauth` provider —
    /// and the Cursor path); the `claude_oauth`/`chatgpt_oauth` account pools
    /// have their own account-rotation failover and are unaffected. On by
    /// default with conservative settings — set `max_retries = 0` to disable.
    #[serde(default)]
    pub retry: RetryConfig,
}

/// Per-provider bounded retry/backoff for transient upstream failures (issue
/// #48). An absent `[providers.<name>.retry]` table uses every default; a
/// partial table overrides only the fields it sets (`#[serde(default)]` fills
/// the rest). See [`crate::retry`] for the runtime behavior these values drive.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct RetryConfig {
    /// Additional attempts after the first upstream try. `0` disables retry.
    pub max_retries: u32,
    /// Backoff ceiling before the first retry, milliseconds (jitter fills
    /// `[0, this]`); grown by `multiplier` per attempt up to `max_backoff_ms`.
    pub initial_backoff_ms: u64,
    /// Upper bound on any single backoff and on an honored `Retry-After`,
    /// milliseconds. A `Retry-After` longer than this surfaces the response
    /// immediately rather than sleeping past budget.
    pub max_backoff_ms: u64,
    /// Exponential growth factor applied to the backoff per attempt (>= 1.0).
    pub multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        // Conservative for a single-user local proxy: at most two extra tries,
        // sub-second first backoff, an 8s ceiling — enough to ride out a brief
        // blip without turning a hard upstream outage into a long client hang.
        Self {
            max_retries: 2,
            initial_backoff_ms: 500,
            max_backoff_ms: 8_000,
            multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Largest `max_retries` accepted at config validation — a foot-gun guard,
    /// not a runtime limit. Far above any sensible value for a local proxy.
    const MAX_RETRIES_LIMIT: u32 = 10;

    /// Build the runtime [`crate::retry::RetryPolicy`] this config describes.
    pub fn policy(&self) -> crate::retry::RetryPolicy {
        crate::retry::RetryPolicy {
            max_retries: self.max_retries,
            initial_backoff: std::time::Duration::from_millis(self.initial_backoff_ms),
            max_backoff: std::time::Duration::from_millis(self.max_backoff_ms),
            multiplier: self.multiplier,
        }
    }

    /// Validate the retry bounds for `provider`. Caps `max_retries` so a typo
    /// can't arm a retry storm, and requires a growth factor that actually grows
    /// (or holds) the backoff — a sub-1.0 or non-finite `multiplier` is rejected.
    /// The invariant lives with the type so any config path that builds a
    /// [`RetryConfig`] can enforce it, not only `Config::validate`.
    pub fn validate(&self, provider: &str) -> Result<(), ConfigError> {
        if self.max_retries > Self::MAX_RETRIES_LIMIT {
            return Err(ConfigError::InvalidRetryMaxRetries {
                provider: provider.to_string(),
                max_retries: self.max_retries,
                limit: Self::MAX_RETRIES_LIMIT,
            });
        }
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(ConfigError::InvalidRetryMultiplier {
                provider: provider.to_string(),
                multiplier: self.multiplier,
            });
        }
        // A zero backoff makes every computed delay zero (`backoff_ceiling` grows
        // from `initial_backoff` and is capped by `max_backoff`), turning retry
        // into a tight no-delay loop that defeats the "backoff" the type promises.
        // Guard it only when retry is actually enabled — `max_retries = 0` is the
        // documented way to turn retry off and legitimately leaves the backoff unused.
        if self.max_retries > 0 && (self.initial_backoff_ms == 0 || self.max_backoff_ms == 0) {
            return Err(ConfigError::InvalidRetryBackoff {
                provider: provider.to_string(),
                initial_backoff_ms: self.initial_backoff_ms,
                max_backoff_ms: self.max_backoff_ms,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountConfig {
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_credentials_path"
    )]
    pub credentials: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Provider-independent stable upstream identity used to coalesce aliases in
    /// an account pool: Claude stores `shuntAccountUuid`, while Codex stores
    /// `chatgpt_account_id`. When absent, pool selection falls back to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Soft quota threshold for every window, overriding `[server.pool]`
    /// defaults for this account. A low value reserves the account as a
    /// backup: it is rotated away from earlier, so it is used less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Per-window soft-threshold overrides; each beats `threshold` for its
    /// window (see [`PoolConfig::default_threshold`] for the resolution order).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_5h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_7d: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_fable: Option<f64>,
    /// Selection priority among available accounts: lower is preferred.
    /// Applies to Claude and Codex pools alike.
    #[serde(default = "default_account_priority")]
    pub priority: u32,
    /// Exclude this account from pool selection entirely without removing its
    /// configuration. Applies to Claude and Codex pools alike.
    #[serde(default)]
    pub disabled: bool,
}

pub(crate) fn default_account_priority() -> u32 {
    100
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            credentials: None,
            token_env: None,
            uuid: None,
            threshold: None,
            threshold_5h: None,
            threshold_7d: None,
            threshold_fable: None,
            priority: default_account_priority(),
            disabled: false,
        }
    }
}

/// Collisions in the same stable-identity key `AccountPool` uses at runtime
/// (`crate::accounts::account_identity`: explicit `uuid`, falling back to
/// `name`), so an account with an explicit `uuid` that happens to equal
/// another account's name-fallback identity is caught here too, not just
/// explicit-`uuid`-vs-explicit-`uuid` collisions.
pub(crate) fn identity_collisions(accounts: &[AccountConfig]) -> Vec<(String, Vec<String>)> {
    let mut groups = BTreeMap::<&str, Vec<String>>::new();
    for account in accounts {
        groups
            .entry(crate::accounts::account_identity(account))
            .or_default()
            .push(account.name.clone());
    }
    groups
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(identity, names)| (identity.to_string(), names))
        .collect()
}

fn deserialize_optional_credentials_path<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|path| path.map(|path| expand_tilde(&path)))
}

fn expand_tilde(path: &str) -> String {
    let Some(suffix) = path.strip_prefix("~/") else {
        return path.to_string();
    };
    // `HOME` is unset on Windows; fall back to `USERPROFILE` so `~/` expands to
    // the user's home there too (mirrors the auth credential-path helpers).
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
        .map(|home| home.join(suffix).to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// How a provider answers `count_tokens`. Only meaningful for `responses` and
/// `cursor` providers; Anthropic providers always pass the request upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CountTokens {
    /// Return 501 `not_supported` so the client falls back on its own (no server
    /// endpoint exists on the Responses API). Claude Code's /context reacts by
    /// re-counting every category against Haiku over the network — slow, and
    /// silently zero without an Anthropic credential — so this is opt-in rather
    /// than the default.
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
    /// Cursor ConnectRPC AgentService protocol.
    Cursor,
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
    /// Inject a Claude subscription OAuth bearer selected from `accounts`.
    ClaudeOauth,
    /// xAI subscription OAuth (SuperGrok / X Premium+), acquired via the
    /// device-code flow (`shunt login xai`) and stored in ~/.shunt/xai-auth.json.
    XaiOauth,
    /// Cursor OAuth acquired by `shunt login cursor`.
    CursorOauth,
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
    /// xAI developer Responses API — rejects `service_tier`/`text`, and 400s on
    /// `reasoning.effort` for several grok models, so reasoning stays opt-in.
    Xai,
    /// Grok CLI subscription proxy. It otherwise speaks the xAI dialect, but
    /// additionally accepts the hosted `web_search` tool.
    Grok,
}

/// Whether `model` advertises native Responses `tool_search` support. OpenAI
/// documents GPT-5.4 and later; codex's `models.json` flags the gpt-5.4/5.5/5.6
/// families with `supports_search_tool: true`. Kept a boundary-guarded substring
/// check (no table) like the effort ceiling in `responses_request.rs`. Earlier
/// slugs (gpt-5.2 and below) fall back to the #43 progressive-reveal shim even
/// with the provider flag on, so the native path only fires for combinations
/// documented to accept it.
fn model_supports_tool_search(model: &str) -> bool {
    // Match each documented "gpt-5.N" family as a whole minor version: the digit
    // must be followed by a non-digit (or end of string), so "gpt-5.4" matches
    // but an undocumented "gpt-5.40" does not silently borrow 5.4's flag and get
    // a native wire shape its backend may reject.
    ["gpt-5.4", "gpt-5.5", "gpt-5.6"].iter().any(|family| {
        model.match_indices(family).any(|(index, matched)| {
            model[index + matched.len()..]
                .chars()
                .next()
                .is_none_or(|next| !next.is_ascii_digit())
        })
    })
}

/// Whether `host` belongs to xAI (`x.ai` or any subdomain). Used both to gate
/// xai-flavored translation and to reject an `xai_oauth` provider pointed at a
/// non-xAI host, so shunt never leaks a subscription bearer to another origin.
pub fn host_is_xai(host: &str) -> bool {
    host == "x.ai" || host.ends_with(".x.ai")
}

/// Whether `host` belongs to Cursor (`cursor.sh`/`cursor.com` or any subdomain).
/// Used to reject a `cursor_oauth` provider pointed at a non-Cursor host, so
/// shunt never leaks the stored Cursor subscription bearer to another origin.
pub fn host_is_cursor(host: &str) -> bool {
    host == "cursor.sh"
        || host.ends_with(".cursor.sh")
        || host == "cursor.com"
        || host.ends_with(".cursor.com")
}

/// Hosts a subscription (`xai_oauth`) bearer may legitimately be sent to: xAI's
/// own API (`x.ai`) and the Grok CLI chat proxy (`grok.com`) that honors a
/// SuperGrok / X Premium+ subscription. Used to reject an `xai_oauth` provider
/// pointed at any other origin, so shunt never leaks the subscription token
/// off-origin, while still allowing the subscription surface the real Grok CLI
/// uses (`cli-chat-proxy.grok.com`).
pub fn host_is_grok_subscription(host: &str) -> bool {
    host_is_xai(host) || host == "grok.com" || host.ends_with(".grok.com")
}

/// Whether `host` belongs to Anthropic (`anthropic.com` or any subdomain).
pub fn host_is_anthropic(host: &str) -> bool {
    host == "anthropic.com" || host.ends_with(".anthropic.com")
}

/// Whether `host` belongs to the ChatGPT/Codex backend (`chatgpt.com` or any
/// subdomain). Used to reject a `chatgpt_oauth` provider pointed at a
/// non-ChatGPT host, so shunt never leaks a Codex subscription bearer to
/// another origin.
pub fn host_is_chatgpt(host: &str) -> bool {
    host == "chatgpt.com" || host.ends_with(".chatgpt.com")
}

/// Whether `host` identifies the local machine.
pub fn host_is_loopback(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
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
    #[error("providers.{provider} uses auth = \"xai_oauth\" but base_url host {host} is not an xAI/Grok host (x.ai or grok.com); refusing to send a subscription token off-origin")]
    XaiOauthNonXaiHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"xai_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    XaiOauthNotHttps { provider: String },
    #[error("providers.{provider} uses auth = \"xai_oauth\" but kind is not \"responses\"; the anthropic adapter would forward the client's own credential instead of the xAI token")]
    XaiOauthWrongKind { provider: String },
    #[error("providers.{provider} uses auth = \"cursor_oauth\" but kind is not \"cursor\"")]
    CursorOauthWrongKind { provider: String },
    #[error("providers.{provider} uses auth = \"cursor_oauth\" but base_url host {host} is not cursor.sh/cursor.com; refusing to send a subscription token off-origin")]
    CursorOauthNonCursorHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"cursor_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    CursorOauthNotHttps { provider: String },
    #[error("providers.{provider}.accounts requires auth = \"claude_oauth\" or \"chatgpt_oauth\"")]
    AccountsRequireOauthProvider { provider: String },
    #[error("providers.{provider} uses auth = \"claude_oauth\" but kind is not \"anthropic\"")]
    ClaudeOauthWrongKind { provider: String },
    #[error("providers.{provider} uses auth = \"claude_oauth\" but base_url host {host} is not anthropic.com; refusing to send a subscription token off-origin")]
    ClaudeOauthNonAnthropicHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"claude_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    ClaudeOauthNotHttps { provider: String },
    #[error("providers.{provider} uses auth = \"chatgpt_oauth\" but base_url host {host} is not chatgpt.com; refusing to send a subscription token off-origin")]
    ChatgptOauthNonChatgptHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"chatgpt_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    ChatgptOauthNotHttps { provider: String },
    #[error("providers.{provider} uses auth = \"chatgpt_oauth\" but kind is not \"responses\"; the anthropic adapter would forward the client's own credential instead of the Codex token")]
    ChatgptOauthWrongKind { provider: String },
    #[error("providers.{provider}.accounts contains duplicate account name \"{name}\"")]
    DuplicateAccountName { provider: String, name: String },
    #[error("providers.{provider}.accounts account name \"{name}\" must match [a-z0-9-]+")]
    InvalidAccountName { provider: String, name: String },
    #[error("providers.{provider}.accounts account \"{name}\" sets both credentials and token_env; set at most one credential source")]
    AccountMultipleCredentialSources { provider: String, name: String },
    #[error("server.pool.{key} must be between 0.0 and 1.0, got {value}")]
    InvalidPoolThreshold { key: &'static str, value: f64 },
    #[error("providers.{provider}.accounts account \"{name}\" {key} must be between 0.0 and 1.0, got {value}")]
    InvalidAccountThreshold {
        provider: String,
        name: String,
        key: &'static str,
        value: f64,
    },
    #[error("server.default_provider references unknown provider: {0}")]
    UnknownDefaultProvider(String),
    #[error("[server.codex_endpoint] references unknown provider: {0}")]
    UnknownCodexEndpointProvider(String),
    #[error("[server.codex_endpoint] provider {0} must use auth = \"chatgpt_oauth\"; the inbound Responses endpoint injects the operator's Codex bearer")]
    CodexEndpointWrongAuth(String),
    #[error("[server.usage] requires [server.auth]: the usage endpoint must identify a non-admin caller by client token")]
    UsageEndpointRequiresAuth,
    #[error("route for model {model} references unknown provider: {provider}")]
    UnknownRouteProvider { model: String, provider: String },
    #[error("route prefix {prefix} references unknown provider: {provider}")]
    UnknownPrefixProvider { prefix: String, provider: String },
    #[error("server.auth.header is not a valid header name: {header}")]
    InvalidAuthHeader { header: String },
    #[error("server.admin.header is not a valid header name: {header}")]
    InvalidAdminHeader { header: String },
    #[error("[server.admin] is set but {env} is unset or empty; refusing to run open")]
    MissingAdminTokens { env: String },
    #[error("[server.admin] tokens in {env} are invalid: {message}")]
    InvalidAdminTokens { env: String, message: String },
    #[error("[server.gateway] public_url is invalid: {message}")]
    InvalidGatewayPublicUrl { message: String },
    #[error("[server.gateway] token_ttl_seconds must be greater than zero")]
    InvalidGatewayTokenTtl,
    #[error(
        "[server.gateway] requires {env} to contain a JWT signing secret of at least 32 bytes"
    )]
    InvalidGatewayJwtSecret { env: String },
    #[error(
        "[server.gateway] is set but {env} is unset or empty; no approval users are configured"
    )]
    MissingGatewayUsers { env: String },
    #[error("[server.gateway] users in {env} are invalid: {message}")]
    InvalidGatewayUsers { env: String, message: String },
    #[error("[server.gateway.policies] must contain at least one policy when configured")]
    EmptyGatewayPolicies,
    #[error("[server.gateway.policies][{index}].match.emails must contain at least one email when present")]
    EmptyGatewayPolicyEmails { index: usize },
    #[error("[server.gateway.policies][{index}].match.emails[{email_index}] must not be empty")]
    EmptyGatewayPolicyEmail { index: usize, email_index: usize },
    #[error("[server.gateway.policies][{index}].cli must be a table/object")]
    InvalidGatewayPolicyCli { index: usize },
    #[error("[server.gateway.policies][{index}].cli{key} contains a non-finite float")]
    InvalidGatewayPolicyValue { index: usize, key: String },
    #[error("[server.gateway.policies][{index}].cli.availableModels must be an array of strings")]
    InvalidGatewayAvailableModels { index: usize },
    #[error("[server.gateway.policies][{index}].cli.env must be a table of scalar values")]
    InvalidGatewayPolicyEnv { index: usize },
    #[error("[server.gateway.telemetry].forward_to[{index}].url is invalid: {message}")]
    InvalidGatewayTelemetryUrl { index: usize, message: String },
    #[error("[server.auth] is set but {env} is unset or empty; refusing to run open")]
    MissingClientTokens { env: String },
    #[error("invalid client tokens in {env}: {message}")]
    InvalidClientTokens { env: String, message: String },
    #[error("sentry.dsn is not a valid DSN: {message}")]
    InvalidSentryDsn { message: String },
    #[error("sentry.traces_sample_rate must be between 0.0 and 1.0, got {rate}")]
    InvalidSentryTracesSampleRate { rate: f64 },
    #[error("otel.endpoint is not a valid URL: {message}")]
    InvalidOtelEndpoint { message: String },
    #[error("otel.sample_ratio must be between 0.0 and 1.0, got {ratio}")]
    InvalidOtelSampleRatio { ratio: f64 },
    #[error("providers.{provider}.retry.max_retries must be at most {limit}, got {max_retries}")]
    InvalidRetryMaxRetries {
        provider: String,
        max_retries: u32,
        limit: u32,
    },
    #[error(
        "providers.{provider}.retry.multiplier must be a finite value >= 1.0, got {multiplier}"
    )]
    InvalidRetryMultiplier { provider: String, multiplier: f64 },
    #[error(
        "providers.{provider}.retry: initial_backoff_ms and max_backoff_ms must both be > 0 when \
         max_retries > 0 (set max_retries = 0 to disable retry instead of zeroing the backoff), \
         got initial_backoff_ms = {initial_backoff_ms}, max_backoff_ms = {max_backoff_ms}"
    )]
    InvalidRetryBackoff {
        provider: String,
        initial_backoff_ms: u64,
        max_backoff_ms: u64,
    },
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
            accounts: Vec::new(),
            websocket: false,
            tool_search: false,
            retry: RetryConfig::default(),
        }
    }

    /// A `Responses`-kind provider on the OpenAI-compatible surface, differing
    /// only in target URL, auth mode, and API-key env var. Used for the built-in
    /// `openai`/`codex`/`xai`/`grok` providers, which are otherwise identical.
    fn responses(base_url: &str, auth: AuthMode, api_key_env: Option<&str>) -> Self {
        Self {
            kind: ProviderKind::Responses,
            base_url: base_url.to_string(),
            auth,
            api_key_env: api_key_env.map(str::to_string),
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            count_tokens: CountTokens::default(),
            accounts: Vec::new(),
            websocket: false,
            tool_search: false,
            retry: RetryConfig::default(),
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
                ProviderConfig::responses(
                    "https://api.openai.com/v1",
                    AuthMode::ApiKey,
                    Some("OPENAI_API_KEY"),
                ),
            ),
            (
                "codex".to_string(),
                ProviderConfig::responses(
                    "https://chatgpt.com/backend-api",
                    AuthMode::ChatgptOauth,
                    None,
                ),
            ),
            (
                "cursor".to_string(),
                ProviderConfig {
                    kind: ProviderKind::Cursor,
                    base_url: "https://api2.cursor.sh".to_string(),
                    auth: AuthMode::CursorOauth,
                    api_key_env: None,
                    api_key_header: ApiKeyHeader::Bearer,
                    effort: None,
                    count_tokens: CountTokens::default(),
                    accounts: Vec::new(),
                    websocket: false,
                    tool_search: false,
                    retry: RetryConfig::default(),
                },
            ),
            (
                // xAI Grok, API-key path: the developer API (api.x.ai), billed
                // per token against an XAI_API_KEY. A SuperGrok / X Premium+
                // subscription is NOT honored here — use the `grok` provider for
                // that (it targets the subscription surface).
                "xai".to_string(),
                ProviderConfig::responses(
                    "https://api.x.ai/v1",
                    AuthMode::ApiKey,
                    Some("XAI_API_KEY"),
                ),
            ),
            (
                // xAI Grok, subscription OAuth path: the Grok CLI chat proxy
                // (cli-chat-proxy.grok.com), which honors a SuperGrok / X
                // Premium+ login (`shunt login xai`) instead of API billing.
                // The developer API (api.x.ai) rejects a subscription bearer
                // with 402/403, so the OAuth path targets the CLI surface and
                // sends the Grok-CLI identity headers, exactly like the `codex`
                // provider reaches chatgpt.com/backend-api rather than
                // api.openai.com.
                "grok".to_string(),
                ProviderConfig::responses(
                    "https://cli-chat-proxy.grok.com/v1",
                    AuthMode::XaiOauth,
                    None,
                ),
            ),
        ]);
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:3001".to_string(),
                default_provider: "anthropic".to_string(),
                auth: None,
                admin: None,
                gateway: None,
                codex_endpoint: None,
                usage: None,
                pool: None,
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
        // Collision reporting belongs to the load boundary rather than
        // validation: RuntimeState defensively re-validates an already-loaded
        // config, and logging there would emit the same warning twice.
        config.warn_identity_collisions();
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

    fn warn_identity_collisions(&self) {
        for (name, provider) in &self.providers {
            for (identity, accounts) in identity_collisions(&provider.accounts) {
                tracing::warn!(
                    provider = %name,
                    identity = %identity,
                    accounts = ?accounts,
                    "multiple account names share one upstream identity; the pool will treat them as one account"
                );
            }
        }
    }

    pub fn validate(self) -> Result<Self, ConfigError> {
        self.server.bind_addr()?;
        // Fail closed at boot: [server.auth] without resolvable tokens is an
        // error, not an open gateway.
        if let Some(auth) = &self.server.auth {
            auth.resolve()?;
        }
        // Fail closed at boot: [server.admin] without resolvable tokens would be
        // an unauthenticated admin surface. Reject it rather than run open.
        if let Some(admin) = &self.server.admin {
            admin.resolve()?;
        }
        // Fail closed at boot: a configured gateway must have a valid issuer,
        // sufficiently strong signing secret, and at least one approval user.
        if let Some(gateway) = &self.server.gateway {
            gateway.resolve()?;
        }
        // [server.pool] thresholds are consumed unchecked by pool selection, so
        // an out-of-range (or NaN) value would silently distort load balancing
        // at runtime. Reject them at boot instead.
        if let Some(pool) = &self.server.pool {
            for (key, value) in [
                ("hard_threshold", Some(pool.hard_threshold)),
                ("default_threshold", pool.default_threshold),
                ("default_threshold_5h", pool.default_threshold_5h),
                ("default_threshold_7d", pool.default_threshold_7d),
                ("default_threshold_fable", pool.default_threshold_fable),
            ] {
                if let Some(value) = value {
                    if !(0.0..=1.0).contains(&value) {
                        return Err(ConfigError::InvalidPoolThreshold { key, value });
                    }
                }
            }
        }
        // A [sentry] section with a non-empty DSN must parse at boot; a typo'd
        // DSN silently dropping every report would defeat the point of opting
        // in. The traces sample rate must be a valid probability (NaN fails the
        // range test too): the Sentry client consumes it unchecked, so an
        // out-of-range value would silently distort sampling at runtime.
        if let Some(sentry) = &self.sentry {
            if sentry.enabled() {
                sentry.dsn.parse::<sentry::types::Dsn>().map_err(|error| {
                    ConfigError::InvalidSentryDsn {
                        message: error.to_string(),
                    }
                })?;
                if !(0.0..=1.0).contains(&sentry.traces_sample_rate) {
                    return Err(ConfigError::InvalidSentryTracesSampleRate {
                        rate: sentry.traces_sample_rate,
                    });
                }
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
                // The exporter speaks OTLP/HTTP, so a syntactically valid but
                // non-HTTP URL (e.g. `ftp://collector` or a scheme-only `mailto:`
                // with no host) would parse here yet never deliver a single
                // export. Reject it at boot rather than fail silently at runtime.
                if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
                    return Err(ConfigError::InvalidOtelEndpoint {
                        message: format!(
                            "endpoint must be an http(s) URL with a host, got `{}`",
                            otel.endpoint
                        ),
                    });
                }
                if !(0.0..=1.0).contains(&otel.sample_ratio) {
                    return Err(ConfigError::InvalidOtelSampleRatio {
                        ratio: otel.sample_ratio,
                    });
                }
                // The plaintext-`[otel.headers]` warning is emitted once at the
                // telemetry boundary (`crate::telemetry::init`), not here: this
                // validator re-runs on every hot-reload, so warning here would
                // repeat the log and mix a side effect into pure validation.
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
            // Bounded-retry sanity (issue #48): the bounds check lives on
            // RetryConfig so the invariant travels with the type.
            provider.retry.validate(name)?;
            // A cursor_oauth provider injects the operator's stored Cursor
            // subscription bearer, so — like xai_oauth below — its base_url must
            // stay on a Cursor host over https, never a gateway or plaintext
            // endpoint that would receive the token. It must also be a Cursor-kind
            // provider so the request goes through the Cursor adapter's auth
            // injection rather than forwarding the client's own credential.
            if provider.auth == AuthMode::CursorOauth {
                if provider.kind != ProviderKind::Cursor {
                    return Err(ConfigError::CursorOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                if url.scheme() != "https" {
                    return Err(ConfigError::CursorOauthNotHttps {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                if !host_is_cursor(host) {
                    return Err(ConfigError::CursorOauthNonCursorHost {
                        provider: name.clone(),
                        host: host.to_string(),
                    });
                }
            }
            if !provider.accounts.is_empty()
                && !matches!(
                    provider.auth,
                    AuthMode::ClaudeOauth | AuthMode::ChatgptOauth
                )
            {
                return Err(ConfigError::AccountsRequireOauthProvider {
                    provider: name.clone(),
                });
            }
            if provider.auth == AuthMode::ClaudeOauth {
                if provider.kind != ProviderKind::Anthropic {
                    return Err(ConfigError::ClaudeOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                // Subscription bearers must never leak to a remote third party.
                // Loopback is the operator's own machine and cannot egress the
                // bearer directly, while allowing local debugging proxies.
                if !host_is_loopback(host) {
                    if url.scheme() != "https" {
                        return Err(ConfigError::ClaudeOauthNotHttps {
                            provider: name.clone(),
                        });
                    }
                    if !host_is_anthropic(host) {
                        return Err(ConfigError::ClaudeOauthNonAnthropicHost {
                            provider: name.clone(),
                            host: host.to_string(),
                        });
                    }
                }
            }
            // A chatgpt_oauth provider injects the operator's stored Codex
            // subscription bearer, so — like claude_oauth above — its base_url
            // must stay on the ChatGPT host over https, never a gateway or
            // plaintext endpoint that would receive the token. It must also be a
            // `responses`-kind provider (the Codex backend's kind, shared with
            // plain OpenAI and xAI): the Responses adapter is what injects the
            // Codex bearer, whereas the anthropic adapter would fall through to
            // forwarding the client's own credential off-origin (same leak guard
            // as xai_oauth above).
            if provider.auth == AuthMode::ChatgptOauth {
                if provider.kind != ProviderKind::Responses {
                    return Err(ConfigError::ChatgptOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                if !host_is_loopback(host) {
                    if url.scheme() != "https" {
                        return Err(ConfigError::ChatgptOauthNotHttps {
                            provider: name.clone(),
                        });
                    }
                    if !host_is_chatgpt(host) {
                        return Err(ConfigError::ChatgptOauthNonChatgptHost {
                            provider: name.clone(),
                            host: host.to_string(),
                        });
                    }
                }
            }
            let mut account_names = HashSet::new();
            for account in &provider.accounts {
                if account.name.is_empty()
                    || !account.name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                {
                    return Err(ConfigError::InvalidAccountName {
                        provider: name.clone(),
                        name: account.name.clone(),
                    });
                }
                if !account_names.insert(&account.name) {
                    return Err(ConfigError::DuplicateAccountName {
                        provider: name.clone(),
                        name: account.name.clone(),
                    });
                }
                if account.credentials.is_some() && account.token_env.is_some() {
                    return Err(ConfigError::AccountMultipleCredentialSources {
                        provider: name.clone(),
                        name: account.name.clone(),
                    });
                }
                // Same boot-time range guard as [server.pool]: pool selection
                // consumes these unchecked.
                for (key, value) in [
                    ("threshold", account.threshold),
                    ("threshold_5h", account.threshold_5h),
                    ("threshold_7d", account.threshold_7d),
                    ("threshold_fable", account.threshold_fable),
                ] {
                    if let Some(value) = value {
                        if !(0.0..=1.0).contains(&value) {
                            return Err(ConfigError::InvalidAccountThreshold {
                                provider: name.clone(),
                                name: account.name.clone(),
                                key,
                                value,
                            });
                        }
                    }
                }
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
                if !host_is_grok_subscription(host) {
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
        // The inbound Responses endpoint injects the operator's Codex bearer, so
        // its target provider must exist and be a `chatgpt_oauth` provider (whose
        // base_url is already held to the ChatGPT host over https by the
        // per-provider guards above). Routing a raw inbound Responses request to
        // any other auth mode would inject the wrong (or no) credential.
        if let Some(codex_endpoint) = &self.server.codex_endpoint {
            match self.provider(&codex_endpoint.provider) {
                None => {
                    return Err(ConfigError::UnknownCodexEndpointProvider(
                        codex_endpoint.provider.clone(),
                    ));
                }
                Some(provider) if provider.auth != AuthMode::ChatgptOauth => {
                    return Err(ConfigError::CodexEndpointWrongAuth(
                        codex_endpoint.provider.clone(),
                    ));
                }
                Some(_) => {}
            }
        }
        // The client-facing usage endpoint identifies its caller by client token,
        // so it is only meaningful — and only safe to register — when inbound auth
        // is configured. Without it, `GET /usage` would be world-readable pool
        // telemetry; fail closed at boot rather than expose it.
        if self.server.usage.is_some() && self.server.auth.is_none() {
            return Err(ConfigError::UsageEndpointRequiresAuth);
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

    /// Resolve `[server.admin]` into the runtime admin-auth state, reading the
    /// configured tokens env. `None` when the admin surface is not configured.
    /// Fails closed (see [`AdminConfig::resolve`]). Shared by `build_router` and
    /// the hot-reload path so both re-resolve admin tokens identically.
    pub fn resolve_admin_auth(
        &self,
    ) -> Result<Option<std::sync::Arc<crate::admin::AdminAuth>>, ConfigError> {
        self.server
            .admin
            .as_ref()
            .map(|admin| admin.resolve())
            .transpose()
            .map(|admin| admin.map(std::sync::Arc::new))
    }

    /// Resolve `[server.gateway]` into the hot-reloadable JWT/users snapshot.
    pub fn resolve_gateway_auth(
        &self,
    ) -> Result<Option<std::sync::Arc<crate::gateway::GatewayAuth>>, ConfigError> {
        self.server
            .gateway
            .as_ref()
            .map(GatewayConfig::resolve)
            .transpose()
            .map(|gateway| gateway.map(std::sync::Arc::new))
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

    /// The effective storm-control initial admission allowance
    /// (`[server.pool] ramp_initial_concurrency`), or `None` when no pool is
    /// configured or the gate is disabled.
    pub fn storm_ramp_initial(&self) -> Option<u32> {
        self.server
            .pool
            .as_ref()
            .and_then(PoolConfig::storm_ramp_initial)
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
        // Hosted tools are a Grok CLI-proxy capability, not an OAuth capability:
        // an xai_oauth provider may still target the developer API at api.x.ai.
        if provider.auth == AuthMode::XaiOauth
            && (host == "grok.com" || host.ends_with(".grok.com"))
        {
            return ResponsesFlavor::Grok;
        }
        if host_is_xai(&host) {
            ResponsesFlavor::Xai
        } else {
            ResponsesFlavor::OpenAi
        }
    }

    /// Whether `provider`'s Responses translation should use the native
    /// client-executed `tool_search` protocol (issue #82) for a request routed
    /// to `model`, rather than the #43 text-based progressive-reveal shim.
    /// Requires all three: the provider opted in (`tool_search = true`), the
    /// upstream speaks a flavor known to accept it (stock OpenAI or the
    /// ChatGPT/Codex backend — xAI/Grok keep the shim), and the model advertises
    /// support (see [`model_supports_tool_search`]).
    pub fn native_tool_search(&self, provider: &str, model: &str) -> bool {
        self.provider(provider)
            .is_some_and(|config| config.tool_search)
            && matches!(
                self.responses_flavor(provider),
                ResponsesFlavor::OpenAi | ResponsesFlavor::Chatgpt
            )
            && model_supports_tool_search(model)
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

    use figment::providers::Format;

    use super::{
        config_file_candidates, default_auth_header, host_is_chatgpt, identity_collisions,
        AccountConfig, AdminConfig, AuthMode, CodexEndpointConfig, Config, ConfigError,
        ConfigFormat, GatewayConfig, GatewayPolicyConfig, GatewayPolicyMatch,
        GatewayTelemetryConfig, GatewayTelemetryDestination, InboundAuthConfig, ModelConfig,
        PoolConfig, ProviderKind, ResponsesFlavor, RetryConfig, UsageEndpointConfig,
    };

    #[test]
    fn pool_config_usage_refresh_interval_disables_and_clamps() {
        use super::PoolConfig;
        // Unset and 0 both disable polling.
        assert_eq!(PoolConfig::default().usage_refresh_interval(), None);
        assert_eq!(
            PoolConfig {
                usage_refresh_seconds: Some(0),
                ..Default::default()
            }
            .usage_refresh_interval(),
            None
        );
        // A positive value below the 60s floor is clamped up; at/above passes through.
        assert_eq!(
            PoolConfig {
                usage_refresh_seconds: Some(5),
                ..Default::default()
            }
            .usage_refresh_interval(),
            Some(60)
        );
        assert_eq!(
            PoolConfig {
                usage_refresh_seconds: Some(300),
                ..Default::default()
            }
            .usage_refresh_interval(),
            Some(300)
        );
    }

    #[test]
    fn pool_config_parses_and_defaults() {
        use super::PoolConfig;
        // An empty object exercises the `#[serde(default)]` field: no polling.
        let empty: PoolConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.usage_refresh_seconds, None);
        // The documented key deserializes.
        let set: PoolConfig = serde_json::from_str(r#"{"usage_refresh_seconds":300}"#).unwrap();
        assert_eq!(set.usage_refresh_seconds, Some(300));
    }

    #[test]
    fn admin_config_uses_defaults_for_missing_fields() {
        // An empty object exercises every `#[serde(default)]` helper.
        let admin: AdminConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(admin.header, "x-shunt-admin-token");
        assert_eq!(admin.tokens_env, "SHUNT_ADMIN_TOKENS");
        assert_eq!(admin.session_ttl_secs, 3600);
        assert_eq!(admin.pending_ttl_secs, 600);
    }

    #[test]
    fn admin_config_resolve_succeeds_and_fails_closed() {
        let base = AdminConfig {
            header: "x-shunt-admin-token".to_string(),
            tokens_env: "SHUNT_TEST_ADMIN_RESOLVE".to_string(),
            session_ttl_secs: 1800,
            pending_ttl_secs: 300,
        };

        // Success: a valid `name:token` env resolves with the configured TTLs.
        std::env::set_var("SHUNT_TEST_ADMIN_RESOLVE", "ops:secret-xyz");
        let auth = base.resolve().expect("valid tokens resolve");
        assert_eq!(auth.session_ttl(), std::time::Duration::from_secs(1800));
        assert_eq!(auth.pending_ttl(), std::time::Duration::from_secs(300));

        // Malformed token pairs are a startup error.
        std::env::set_var("SHUNT_TEST_ADMIN_RESOLVE", "no-colon-here");
        assert!(matches!(
            base.resolve(),
            Err(ConfigError::InvalidAdminTokens { .. })
        ));

        // An unset env is a startup error, never a silently-open surface.
        std::env::remove_var("SHUNT_TEST_ADMIN_RESOLVE");
        assert!(matches!(
            base.resolve(),
            Err(ConfigError::MissingAdminTokens { .. })
        ));

        // An invalid header name is rejected.
        std::env::set_var("SHUNT_TEST_ADMIN_RESOLVE", "ops:secret-xyz");
        let bad_header = AdminConfig {
            header: "invalid header".to_string(),
            ..base.clone()
        };
        assert!(matches!(
            bad_header.resolve(),
            Err(ConfigError::InvalidAdminHeader { .. })
        ));
        std::env::remove_var("SHUNT_TEST_ADMIN_RESOLVE");
    }

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

    fn account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn claude_oauth_config() -> Config {
        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().auth = AuthMode::ClaudeOauth;
        config
    }

    #[test]
    fn accounts_require_oauth_provider() {
        let mut config = Config::default();
        config
            .providers
            .get_mut("anthropic")
            .unwrap()
            .accounts
            .push(account("main"));
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::AccountsRequireOauthProvider { .. }
        ));
    }

    #[test]
    fn claude_oauth_requires_anthropic_kind() {
        let mut config = claude_oauth_config();
        config.providers.get_mut("anthropic").unwrap().kind = ProviderKind::Responses;
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ClaudeOauthWrongKind { .. }
        ));
    }

    #[test]
    fn claude_oauth_accepts_plaintext_loopback_base_urls() {
        for base_url in ["http://127.0.0.1:8080", "http://localhost:9000"] {
            let mut config = claude_oauth_config();
            config.providers.get_mut("anthropic").unwrap().base_url = base_url.to_string();
            config.validate().unwrap();
        }
    }

    #[test]
    fn claude_oauth_rejects_plaintext_remote_base_url() {
        let mut config = claude_oauth_config();
        config.providers.get_mut("anthropic").unwrap().base_url =
            "http://api.anthropic.com".to_string();
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ClaudeOauthNotHttps { .. }
        ));
    }

    #[test]
    fn claude_oauth_rejects_remote_non_anthropic_base_url() {
        let mut config = claude_oauth_config();
        config.providers.get_mut("anthropic").unwrap().base_url =
            "https://evil.example.com".to_string();
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ClaudeOauthNonAnthropicHost { .. }
        ));
    }

    #[test]
    fn claude_oauth_accepts_anthropic_https_base_url() {
        let mut config = claude_oauth_config();
        config.providers.get_mut("anthropic").unwrap().base_url =
            "https://api.anthropic.com".to_string();
        config.validate().unwrap();
    }

    #[test]
    fn claude_oauth_rejects_duplicate_and_invalid_account_names() {
        let mut config = claude_oauth_config();
        config.providers.get_mut("anthropic").unwrap().accounts =
            vec![account("main"), account("main")];
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateAccountName { .. }
        ));

        for invalid in ["", "Main", "main_account", "main.account"] {
            let mut config = claude_oauth_config();
            config.providers.get_mut("anthropic").unwrap().accounts = vec![account(invalid)];
            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::InvalidAccountName { .. }
            ));
        }
    }

    #[test]
    fn claude_oauth_rejects_multiple_credential_sources() {
        let mut config = claude_oauth_config();
        let mut configured = account("main");
        configured.credentials = Some("/tmp/credentials.json".to_string());
        configured.token_env = Some("CLAUDE_TOKEN".to_string());
        config.providers.get_mut("anthropic").unwrap().accounts = vec![configured];
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::AccountMultipleCredentialSources { .. }
        ));
    }

    #[test]
    fn pool_config_and_account_thresholds_parse_from_toml() {
        let pool: PoolConfig = figment::Figment::from(figment::providers::Toml::string(
            "default_threshold = 0.85\nburn_rate_avoidance = true\nramp_initial_concurrency = 4",
        ))
        .extract()
        .unwrap();
        assert_eq!(pool.hard_threshold, 0.98, "serde default");
        assert_eq!(pool.default_threshold, Some(0.85));
        assert_eq!(pool.default_threshold_5h, None);
        assert!(pool.burn_rate_avoidance);
        assert_eq!(pool.ramp_initial_concurrency, Some(4));
        assert_eq!(
            PoolConfig::default().ramp_initial_concurrency,
            None,
            "storm control defaults to disabled"
        );

        let account: AccountConfig = figment::Figment::from(figment::providers::Toml::string(
            "name = \"backup\"\nthreshold = 0.5\nthreshold_fable = 0.4\npriority = 10\ndisabled = true",
        ))
        .extract()
        .unwrap();
        assert_eq!(account.threshold, Some(0.5));
        assert_eq!(account.threshold_fable, Some(0.4));
        assert_eq!(account.priority, 10);
        assert!(account.disabled);

        let bare: AccountConfig =
            figment::Figment::from(figment::providers::Toml::string("name = \"main\""))
                .extract()
                .unwrap();
        assert_eq!(bare.threshold, None);
        assert_eq!(bare.priority, 100, "serde default");
        assert!(!bare.disabled);
    }

    #[test]
    fn storm_ramp_initial_treats_zero_and_absent_as_disabled() {
        for (configured, expected) in [(None, None), (Some(0), None), (Some(5), Some(5))] {
            let pool = PoolConfig {
                ramp_initial_concurrency: configured,
                ..Default::default()
            };
            assert_eq!(pool.storm_ramp_initial(), expected, "{configured:?}");
        }
    }

    #[test]
    fn validate_rejects_out_of_range_pool_thresholds() {
        for (key, pool) in [
            (
                "hard_threshold",
                PoolConfig {
                    hard_threshold: 1.5,
                    ..Default::default()
                },
            ),
            (
                "default_threshold_7d",
                PoolConfig {
                    default_threshold_7d: Some(-0.1),
                    ..Default::default()
                },
            ),
        ] {
            let mut config = Config::default();
            config.server.pool = Some(pool);
            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::InvalidPoolThreshold { key: found, .. } if found == key
            ));
        }
        let mut config = Config::default();
        config.server.pool = Some(PoolConfig::default());
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_out_of_range_account_thresholds() {
        let mut config = claude_oauth_config();
        let mut backup = account("backup");
        backup.threshold_5h = Some(1.01);
        config.providers.get_mut("anthropic").unwrap().accounts = vec![backup];
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidAccountThreshold {
                key: "threshold_5h",
                ..
            }
        ));

        let mut config = claude_oauth_config();
        let mut backup = account("backup");
        backup.threshold = Some(0.5);
        config.providers.get_mut("anthropic").unwrap().accounts = vec![backup];
        config.validate().unwrap();
    }

    #[test]
    fn claude_oauth_accepts_empty_accounts_and_default_anthropic_origin() {
        let config = claude_oauth_config().validate().unwrap();
        let anthropic = config.provider("anthropic").unwrap();
        assert!(anthropic.accounts.is_empty());
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
    }

    // The default `codex` provider already uses `auth = "chatgpt_oauth"` with
    // base_url `https://chatgpt.com/backend-api`, so unlike claude_oauth these
    // tests mutate `Config::default()` directly rather than needing a config
    // builder that flips the auth mode first.

    #[test]
    fn identity_collisions_group_only_explicit_shared_identities() {
        let mut first = account("first");
        first.uuid = Some("shared".to_string());
        let mut second = account("second");
        second.uuid = Some("shared".to_string());
        let unique = account("unique");
        let mut solo = account("solo");
        solo.uuid = Some("solo-id".to_string());

        assert_eq!(
            identity_collisions(&[first.clone(), second.clone(), unique, solo]),
            vec![(
                "shared".to_string(),
                vec!["first".to_string(), "second".to_string()]
            )]
        );

        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().accounts = vec![first, second];
        assert!(
            config.validate().is_ok(),
            "collisions are warnings, not errors"
        );
    }

    #[test]
    fn identity_collisions_catches_explicit_uuid_matching_a_name_fallback_identity() {
        // "first" has no uuid, so its runtime identity falls back to its name
        // ("first"). A second account whose *explicit* uuid is literally
        // "first" collides with it at runtime (`account_identity` uses the
        // same key for both), even though the old implementation only ever
        // compared explicit uuids against each other.
        let first = account("first");
        let mut second = account("second");
        second.uuid = Some("first".to_string());
        let unrelated = account("unrelated");

        assert_eq!(
            identity_collisions(&[first.clone(), second.clone(), unrelated]),
            vec![(
                "first".to_string(),
                vec!["first".to_string(), "second".to_string()]
            )]
        );

        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().accounts = vec![first, second];
        assert!(
            config.validate().is_ok(),
            "collisions are warnings, not errors"
        );
    }

    #[test]
    fn chatgpt_oauth_accepts_accounts_on_default_chatgpt_host() {
        let mut config = Config::default();
        config
            .providers
            .get_mut("codex")
            .unwrap()
            .accounts
            .push(account("work"));
        let config = config.validate().unwrap();
        let codex = config.provider("codex").unwrap();
        assert_eq!(codex.accounts.len(), 1);
    }

    #[test]
    fn chatgpt_oauth_rejects_remote_non_chatgpt_base_url() {
        let mut config = Config::default();
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = "https://evil.example.com".to_string();
        codex.accounts.push(account("work"));
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ChatgptOauthNonChatgptHost { .. }
        ));
    }

    #[test]
    fn chatgpt_oauth_rejects_plaintext_remote_base_url() {
        let mut config = Config::default();
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = "http://chatgpt.com/backend-api".to_string();
        codex.accounts.push(account("work"));
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ChatgptOauthNotHttps { .. }
        ));
    }

    #[test]
    fn chatgpt_oauth_requires_responses_kind() {
        // An anthropic-kind provider never injects the ChatGptOAuth credential —
        // the anthropic adapter would forward the client's own headers to
        // chatgpt.com — so the combination is rejected at boot (mirrors the
        // xai_oauth guard).
        let mut config = Config::default();
        let codex = config.providers.get_mut("codex").unwrap();
        codex.kind = ProviderKind::Anthropic;
        codex.accounts.push(account("work"));
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::ChatgptOauthWrongKind { .. }));
    }

    #[test]
    fn chatgpt_oauth_accepts_plaintext_loopback_base_url() {
        let mut config = Config::default();
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = "http://127.0.0.1:8080".to_string();
        codex.accounts.push(account("work"));
        config.validate().unwrap();
    }

    #[test]
    fn chatgpt_oauth_rejects_duplicate_account_names() {
        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().accounts =
            vec![account("work"), account("work")];
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateAccountName { .. }
        ));
    }

    #[test]
    fn codex_endpoint_accepts_a_chatgpt_oauth_provider() {
        // The built-in `codex` provider is chatgpt_oauth, so opting into the
        // inbound endpoint against it validates.
        let mut config = Config::default();
        config.server.codex_endpoint = Some(CodexEndpointConfig {
            provider: "codex".to_string(),
        });
        config.validate().unwrap();
    }

    #[test]
    fn codex_endpoint_rejects_unknown_provider() {
        let mut config = Config::default();
        config.server.codex_endpoint = Some(CodexEndpointConfig {
            provider: "nope".to_string(),
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::UnknownCodexEndpointProvider(provider) if provider == "nope"
        ));
    }

    #[test]
    fn codex_endpoint_rejects_non_chatgpt_oauth_provider() {
        // Pointing the inbound endpoint at a non-chatgpt_oauth provider (here the
        // built-in `anthropic` passthrough provider) would inject the wrong (or
        // no) credential, so it is rejected at boot.
        let mut config = Config::default();
        config.server.codex_endpoint = Some(CodexEndpointConfig {
            provider: "anthropic".to_string(),
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::CodexEndpointWrongAuth(provider) if provider == "anthropic"
        ));
    }

    #[test]
    fn gateway_state_path_defaults_on_and_empty_string_disables() {
        let parsed: GatewayConfig = figment::Figment::from(figment::providers::Toml::string(
            "public_url = \"https://gateway.example\"",
        ))
        .extract()
        .unwrap();
        assert_eq!(parsed.state_path, super::default_gateway_state_path());
        let default_path = parsed
            .session_state_path()
            .expect("test environments resolve a home directory");
        assert!(default_path.ends_with(".shunt/gateway-sessions.json"));

        let disabled: GatewayConfig = figment::Figment::from(figment::providers::Toml::string(
            "public_url = \"https://gateway.example\"\nstate_path = \"\"",
        ))
        .extract()
        .unwrap();
        assert_eq!(disabled.session_state_path(), None);

        let explicit: GatewayConfig = figment::Figment::from(figment::providers::Toml::string(
            "public_url = \"https://gateway.example\"\nstate_path = \"/tmp/sessions.json\"",
        ))
        .extract()
        .unwrap();
        assert_eq!(
            explicit.session_state_path(),
            Some(std::path::Path::new("/tmp/sessions.json"))
        );
    }

    #[test]
    fn gateway_config_fails_closed_and_resolves_valid_environment() {
        let suffix = std::process::id();
        let secret_env = format!("SHUNT_GATEWAY_CONFIG_SECRET_{suffix}");
        let users_env = format!("SHUNT_GATEWAY_CONFIG_USERS_{suffix}");
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: secret_env.clone(),
            users_env: users_env.clone(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
        };

        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayJwtSecret { .. })
        ));
        std::env::set_var(&secret_env, "too-short");
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayJwtSecret { .. })
        ));
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::MissingGatewayUsers { .. })
        ));
        std::env::set_var(&users_env, "malformed");
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayUsers { .. })
        ));
        std::env::set_var(&users_env, "dev@example.com:password");
        let resolved = gateway.resolve().expect("valid gateway config");
        assert_eq!(resolved.public_url(), "https://gateway.example");
        assert_eq!(resolved.token_ttl_seconds(), 3600);
        assert!(!resolved.trust_forwarded_for());

        let trusted = GatewayConfig {
            trust_forwarded_for: true,
            ..gateway.clone()
        }
        .resolve()
        .expect("trusted proxy opt-in resolves");
        assert!(trusted.trust_forwarded_for());

        std::env::remove_var(secret_env);
        std::env::remove_var(users_env);
    }

    #[test]
    fn gateway_config_rejects_invalid_public_url_and_zero_ttl() {
        let mut gateway = GatewayConfig {
            public_url: "not a URL".to_string(),
            jwt_secret_env: "UNUSED_GATEWAY_SECRET".to_string(),
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPublicUrl { .. })
        ));
        gateway.public_url = "https://gateway.example/path".to_string();
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPublicUrl { .. })
        ));
        gateway.public_url = "https://user:password@gateway.example".to_string();
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPublicUrl { .. })
        ));
        gateway.public_url = "http://gateway.example".to_string();
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPublicUrl { .. })
        ));
        gateway.public_url = "http://127.0.0.1:8787".to_string();
        gateway.token_ttl_seconds = 0;
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayTokenTtl)
        ));
    }

    #[test]
    fn gateway_config_rejects_invalid_managed_policy_and_telemetry() {
        let suffix = format!("{}_managed", std::process::id());
        let secret_env = format!("SHUNT_GATEWAY_CONFIG_SECRET_{suffix}");
        let users_env = format!("SHUNT_GATEWAY_CONFIG_USERS_{suffix}");
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&users_env, "dev@example.com:password");
        let base = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: secret_env.clone(),
            users_env: users_env.clone(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
        };

        let mut gateway = base.clone();
        gateway.policies = Some(vec![]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::EmptyGatewayPolicies)
        ));

        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: Some(GatewayPolicyMatch {
                emails: Some(vec![]),
            }),
            cli: toml::Value::Table(toml::Table::new()),
        }]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::EmptyGatewayPolicyEmails { index: 0 })
        ));

        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: Some(GatewayPolicyMatch {
                emails: Some(vec!["dev@example.com".to_string(), "  ".to_string()]),
            }),
            cli: toml::Value::Table(toml::Table::new()),
        }]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::EmptyGatewayPolicyEmail {
                index: 0,
                email_index: 1
            })
        ));

        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: None,
            cli: toml::Value::String("not-an-object".to_string()),
        }]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPolicyCli { index: 0 })
        ));

        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: None,
            cli: toml::toml! { availableModels = ["allowed", 3] }.into(),
        }]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayAvailableModels { index: 0 })
        ));

        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: None,
            cli: toml::toml! { env = { VALID = "yes", INVALID = ["nested"] } }.into(),
        }]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPolicyEnv { index: 0 })
        ));

        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: None,
            cli: toml::toml! {
                env = { STRING = "yes", NUMBER = 1, BOOLEAN = true }
            }
            .into(),
        }]);
        let resolved = gateway.resolve().expect("scalar env values are valid");
        let settings = resolved.managed_settings("dev@example.com").unwrap();
        assert_eq!(settings["env"]["STRING"], serde_json::json!("yes"));
        assert_eq!(settings["env"]["NUMBER"], serde_json::json!(1));
        assert_eq!(settings["env"]["BOOLEAN"], serde_json::json!(true));

        let mut cli = toml::Table::new();
        cli.insert("weight".to_string(), toml::Value::Float(f64::INFINITY));
        gateway.policies = Some(vec![GatewayPolicyConfig {
            matcher: None,
            cli: toml::Value::Table(cli),
        }]);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayPolicyValue { index: 0, .. })
        ));

        gateway.policies = None;
        gateway.telemetry = Some(GatewayTelemetryConfig {
            forward_to: vec![GatewayTelemetryDestination {
                url: "ftp://collector.example".to_string(),
                headers: None,
            }],
        });
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayTelemetryUrl { index: 0, .. })
        ));

        std::env::remove_var(secret_env);
        std::env::remove_var(users_env);
    }

    #[test]
    fn usage_endpoint_requires_inbound_auth() {
        // Opting into `[server.usage]` without `[server.auth]` is rejected at
        // boot: the endpoint must identify a non-admin caller by client token.
        let mut config = Config::default();
        config.server.usage = Some(UsageEndpointConfig::default());
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::UsageEndpointRequiresAuth
        ));
    }

    #[test]
    fn usage_endpoint_accepts_when_inbound_auth_is_configured() {
        // With `[server.auth]` present and its tokens resolvable, the pairing
        // validates. `validate()` fails closed by resolving `[server.auth]`, so
        // point it at an env var holding a valid token.
        let env = format!("SHUNT_USAGE_VALIDATE_TOKENS_{}", std::process::id());
        std::env::set_var(&env, "tester:tok");
        let mut config = Config::default();
        config.server.usage = Some(UsageEndpointConfig::default());
        config.server.auth = Some(InboundAuthConfig {
            header: default_auth_header(),
            tokens_env: env.clone(),
        });
        let result = config.validate();
        std::env::remove_var(&env);
        result.unwrap();
    }

    #[test]
    fn host_is_chatgpt_matches_chatgpt_and_subdomains_only() {
        assert!(host_is_chatgpt("chatgpt.com"));
        assert!(host_is_chatgpt("x.chatgpt.com"));
        assert!(!host_is_chatgpt("chatgpt.com.evil.com"));
        assert!(!host_is_chatgpt("openai.com"));
    }

    #[test]
    fn account_credentials_expand_home_tilde() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let account: AccountConfig = figment::Figment::from(figment::providers::Toml::string(
            "name = \"main\"\ncredentials = \"~/.claude/.credentials.json\"",
        ))
        .extract()
        .unwrap();
        assert_eq!(
            account.credentials.as_deref(),
            Some(format!("{home}/.claude/.credentials.json").as_str())
        );
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
            config.provider("anthropic").unwrap().auth,
            AuthMode::Passthrough
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
    fn default_seeds_builtin_cursor_provider() {
        let config = Config::default();
        let cursor = config.provider("cursor").unwrap();
        assert_eq!(cursor.kind, ProviderKind::Cursor);
        assert_eq!(cursor.base_url, "https://api2.cursor.sh");
        assert_eq!(cursor.auth, AuthMode::CursorOauth);
    }

    #[test]
    fn retry_config_defaults_are_conservative_and_enabled() {
        // Every built-in provider carries the on-by-default conservative policy.
        let config = Config::default();
        let retry = config.provider("anthropic").unwrap().retry;
        assert_eq!(retry, RetryConfig::default());
        assert_eq!(retry.max_retries, 2);
        assert_eq!(retry.initial_backoff_ms, 500);
        assert_eq!(retry.max_backoff_ms, 8_000);
        assert_eq!(retry.multiplier, 2.0);
        assert!(retry.policy().is_enabled());
    }

    #[test]
    fn retry_config_empty_table_fills_every_default() {
        // An empty `[providers.x.retry]` table exercises the container default.
        let retry: RetryConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(retry, RetryConfig::default());
    }

    #[test]
    fn retry_config_partial_table_overrides_only_set_fields() {
        let retry: RetryConfig = serde_json::from_str(r#"{"max_retries": 0}"#).unwrap();
        assert_eq!(retry.max_retries, 0);
        // The rest keep their defaults.
        assert_eq!(retry.initial_backoff_ms, 500);
        assert_eq!(retry.max_backoff_ms, 8_000);
        assert!(!retry.policy().is_enabled());
    }

    #[test]
    fn retry_max_retries_over_limit_is_rejected() {
        let mut config = Config::default();
        config
            .providers
            .get_mut("anthropic")
            .unwrap()
            .retry
            .max_retries = 99;
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidRetryMaxRetries {
                max_retries: 99,
                ..
            }
        ));
    }

    #[test]
    fn retry_multiplier_below_one_is_rejected() {
        let mut config = Config::default();
        config
            .providers
            .get_mut("anthropic")
            .unwrap()
            .retry
            .multiplier = 0.5;
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidRetryMultiplier { .. }));
    }

    #[test]
    fn retry_validate_accepts_limit_and_rejects_one_over() {
        // The cap is inclusive: exactly MAX_RETRIES_LIMIT is allowed, one more
        // is not — pin both sides of the boundary so a `>` vs `>=` slip is caught.
        let at_limit = RetryConfig {
            max_retries: 10,
            ..RetryConfig::default()
        };
        assert!(at_limit.validate("anthropic").is_ok());

        let over_limit = RetryConfig {
            max_retries: 11,
            ..RetryConfig::default()
        };
        assert!(matches!(
            over_limit.validate("anthropic").unwrap_err(),
            ConfigError::InvalidRetryMaxRetries {
                max_retries: 11,
                limit: 10,
                ..
            }
        ));
    }

    #[test]
    fn retry_validate_rejects_non_finite_multiplier() {
        // NaN slips past a naive `< 1.0` comparison (every comparison with NaN is
        // false), so the finiteness guard must reject it — and infinity too.
        for multiplier in [f64::NAN, f64::INFINITY] {
            let retry = RetryConfig {
                multiplier,
                ..RetryConfig::default()
            };
            assert!(matches!(
                retry.validate("anthropic").unwrap_err(),
                ConfigError::InvalidRetryMultiplier { .. }
            ));
        }
    }

    #[test]
    fn retry_validate_rejects_zero_backoff_when_enabled() {
        // Retry enabled but a zeroed backoff would spin with no delay — rejected
        // whether it's the initial, the max, or both that are zero.
        for (initial, max) in [(0, 8_000), (500, 0), (0, 0)] {
            let retry = RetryConfig {
                max_retries: 2,
                initial_backoff_ms: initial,
                max_backoff_ms: max,
                multiplier: 2.0,
            };
            assert!(matches!(
                retry.validate("anthropic").unwrap_err(),
                ConfigError::InvalidRetryBackoff { .. }
            ));
        }
        // Disabled retry (max_retries = 0) leaves the backoff unused, so a zero
        // backoff is allowed — that's the documented way to turn retry off.
        let disabled = RetryConfig {
            max_retries: 0,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            multiplier: 1.0,
        };
        assert!(disabled.validate("anthropic").is_ok());
    }

    #[test]
    fn retry_validate_accepts_multiplier_at_inclusive_lower_bound() {
        // Exactly 1.0 (a never-grows backoff, e.g. the disabled policy's own value)
        // is accepted; just below is not — pins the `< 1.0` vs `<= 1.0` boundary.
        let at_bound = RetryConfig {
            multiplier: 1.0,
            ..RetryConfig::default()
        };
        assert!(at_bound.validate("anthropic").is_ok());

        let below = RetryConfig {
            multiplier: 0.999,
            ..RetryConfig::default()
        };
        assert!(matches!(
            below.validate("anthropic").unwrap_err(),
            ConfigError::InvalidRetryMultiplier { .. }
        ));
    }

    #[test]
    fn retry_config_round_trips_through_toml_provider_table() {
        // A `[providers.anthropic.retry]` block deep-merges over the built-in
        // defaults exactly as `Config::load` does, and every field survives the
        // TOML round-trip into a policy that validates and stays enabled.
        let config: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(figment::providers::Toml::string(
                    "[providers.anthropic.retry]\n\
             max_retries = 5\n\
             initial_backoff_ms = 250\n\
             max_backoff_ms = 4000\n\
             multiplier = 1.5\n",
                ))
                .extract()
                .unwrap();

        let retry = config.provider("anthropic").unwrap().retry;
        assert_eq!(retry.max_retries, 5);
        assert_eq!(retry.initial_backoff_ms, 250);
        assert_eq!(retry.max_backoff_ms, 4_000);
        assert_eq!(retry.multiplier, 1.5);
        config.validate().unwrap();
        assert!(retry.policy().is_enabled());
    }

    #[test]
    fn cursor_oauth_requires_cursor_kind() {
        let mut config = Config::default();
        config.providers.get_mut("cursor").unwrap().kind = ProviderKind::Responses;
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::CursorOauthWrongKind { .. }));
    }

    #[test]
    fn cursor_oauth_rejects_non_cursor_host() {
        // The built-in cursor provider (api2.cursor.sh over https) is accepted.
        let config = Config::default();
        assert!(config.validate().is_ok());

        // Pointing a cursor_oauth provider off-origin is refused (bearer-leak guard).
        let mut config = Config::default();
        config.providers.get_mut("cursor").unwrap().base_url =
            "https://evil.example.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::CursorOauthNonCursorHost { .. }
        ));
        assert!(error.to_string().contains("evil.example.com"));
    }

    #[test]
    fn cursor_oauth_requires_https_base_url() {
        let mut config = Config::default();
        config.providers.get_mut("cursor").unwrap().base_url = "http://api2.cursor.sh".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::CursorOauthNotHttps { .. }));
        assert!(error.to_string().contains("plaintext"));
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
    fn native_tool_search_requires_opt_in_flavor_and_model() {
        let mut config = Config::default();
        // Off by default, even for a supported flavor + model.
        assert!(!config.native_tool_search("codex", "gpt-5.6-sol"));

        config.providers.get_mut("codex").unwrap().tool_search = true;
        config.providers.get_mut("openai").unwrap().tool_search = true;
        config.providers.get_mut("xai").unwrap().tool_search = true;

        // Opted in + supported flavor (Chatgpt / OpenAi) + supported model.
        assert!(config.native_tool_search("codex", "gpt-5.6-sol"));
        assert!(config.native_tool_search("openai", "gpt-5.4"));
        // A trailing non-digit still counts as the documented minor.
        assert!(config.native_tool_search("openai", "gpt-5.4-turbo"));

        // Boundary guard: a multi-digit minor must NOT borrow 5.4's flag — those
        // are undocumented families whose backend may reject the native wire.
        assert!(!config.native_tool_search("openai", "gpt-5.40"));
        assert!(!config.native_tool_search("openai", "gpt-5.41-turbo"));

        // Unsupported model keeps the #43 shim (gpt-5.2 and below).
        assert!(!config.native_tool_search("codex", "gpt-5.2-codex"));
        // Unsupported flavor keeps the shim (xAI), even opted in.
        assert!(!config.native_tool_search("xai", "gpt-5.6-sol"));
        // Unknown provider is never native.
        assert!(!config.native_tool_search("nope", "gpt-5.6-sol"));
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
    fn default_seeds_builtin_grok_subscription_provider() {
        let config = Config::default();
        let grok = config.provider("grok").unwrap();
        // Subscription OAuth path: the Grok CLI chat proxy, not api.x.ai.
        assert_eq!(grok.kind, ProviderKind::Responses);
        assert_eq!(grok.base_url, "https://cli-chat-proxy.grok.com/v1");
        assert_eq!(grok.auth, AuthMode::XaiOauth);
        assert!(grok.api_key_env.is_none());
        // The Grok flavor keys on the CLI proxy host and enables
        // proxy-only capabilities.
        assert_eq!(config.responses_flavor("grok"), ResponsesFlavor::Grok);
        // The default config validates: the bearer-leak guard allows grok.com.
        assert!(config.validate().is_ok());
    }

    #[test]
    fn xai_oauth_accepts_grok_com_host_but_still_rejects_other_origins() {
        // The Grok CLI chat proxy host is accepted for the subscription bearer.
        let mut config = Config::default();
        let provider = config.providers.get_mut("grok").unwrap();
        provider.base_url = "https://cli-chat-proxy.grok.com/v1".to_string();
        assert!(config.validate().is_ok());

        // A non-xAI, non-grok host is still refused off-origin.
        let mut config = Config::default();
        let provider = config.providers.get_mut("grok").unwrap();
        provider.base_url = "https://evil.example.com/v1".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::XaiOauthNonXaiHost { .. }));
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

    fn sentry_config(dsn: &str) -> super::SentryConfig {
        super::SentryConfig {
            dsn: dsn.to_string(),
            environment: None,
            metrics: false,
            traces_sample_rate: 0.0,
            include_session_id: false,
        }
    }

    #[test]
    fn sentry_section_with_valid_dsn_validates() {
        let config = Config {
            sentry: Some(super::SentryConfig {
                environment: Some("home-lab".to_string()),
                ..sentry_config("https://public@o0.ingest.sentry.io/1234")
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
            sentry: Some(sentry_config("not-a-dsn")),
            ..Config::default()
        };
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidSentryDsn { .. }));
    }

    #[test]
    fn sentry_empty_dsn_disables_reporting_and_validates() {
        // SHUNT_SENTRY__DSN="" must be able to switch a TOML section off.
        let config = Config {
            sentry: Some(sentry_config("")),
            ..Config::default()
        };
        let config = config.validate().unwrap();
        assert!(!config.sentry.as_ref().unwrap().enabled());
    }

    #[test]
    fn sentry_tracing_defaults_off_and_parses_from_toml() {
        // Tracing is a separate opt-in on top of error reporting, mirroring
        // the `metrics` flag: absent keys mean no spans and no session id.
        use figment::providers::{Format, Toml};
        let dsn = "dsn = \"https://public@o0.ingest.sentry.io/1234\"";
        let sentry: super::SentryConfig =
            figment::Figment::from(Toml::string(dsn)).extract().unwrap();
        assert_eq!(sentry.traces_sample_rate, 0.0);
        assert!(!sentry.include_session_id);
        let sentry: super::SentryConfig = figment::Figment::from(Toml::string(&format!(
            "{dsn}\ntraces_sample_rate = 0.25\ninclude_session_id = true"
        )))
        .extract()
        .unwrap();
        assert_eq!(sentry.traces_sample_rate, 0.25);
        assert!(sentry.include_session_id);
    }

    #[test]
    fn sentry_traces_sample_rate_out_of_range_is_rejected() {
        for rate in [-0.1, 1.5, f64::NAN] {
            let mut sentry = sentry_config("https://public@o0.ingest.sentry.io/1234");
            sentry.traces_sample_rate = rate;
            let config = Config {
                sentry: Some(sentry),
                ..Config::default()
            };
            let error = config.validate().unwrap_err();
            assert!(matches!(
                error,
                ConfigError::InvalidSentryTracesSampleRate { .. }
            ));
        }
    }

    #[test]
    fn sentry_disabled_section_skips_traces_sample_rate_validation() {
        // An empty DSN disables the section, so a leftover bad rate must not
        // block boot — mirroring how a disabled [otel] skips ratio validation.
        let mut sentry = sentry_config("");
        sentry.traces_sample_rate = 99.0; // ignored while disabled
        let config = Config {
            sentry: Some(sentry),
            ..Config::default()
        };
        assert!(config.validate().is_ok());
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
    fn otel_non_http_endpoint_is_rejected_at_boot() {
        // Parses as a URL but the OTLP/HTTP exporter can never use it.
        let config = Config {
            otel: Some(otel_config("ftp://collector.example")),
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
