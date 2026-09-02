use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use figment::{
    providers::{Env, Format, Serialized, Toml, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod admin_keys;
mod http_tuning;
mod presets;
mod secrets;
mod session;
mod spend;
mod upstreams;

pub use admin_keys::{AdminAccess, AdminCredential, AdminKey, AdminKeyring};
pub use http_tuning::{
    AccessControlConfig, LimitsConfig, RateLimitConfig, RateLimitsConfig, TimeoutsConfig,
};
pub use presets::{provider_presets, ProviderPresetView};
pub use secrets::Secret;
pub use session::GatewaySessionConfig;
pub use spend::{GroupLimitMode, SpendConfig, SpendEnforcementConfig};
pub use upstreams::{AccountSelection, AuthMap, UpstreamAuth, UpstreamConfig};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub providers: ProvidersConfig,
    /// Ordered user-facing upstream declarations. When non-empty, validation
    /// normalizes these entries into `providers` so existing consumers keep one
    /// name-keyed lookup path.
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,
    /// Effective upstream precedence: declaration order for `[[upstreams]]`, or
    /// name-sorted `providers` keys for the legacy form.
    #[serde(skip)]
    pub upstream_order: Vec<String>,
    /// Whether `upstream_order` came from an explicit ordered declaration.
    #[serde(skip)]
    pub upstreams_ordered: bool,
    #[serde(default = "default_auto_include_builtin_models")]
    pub auto_include_builtin_models: bool,
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

fn default_auto_include_builtin_models() -> bool {
    true
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
    ///
    /// INVARIANT: this must stay behind an `Option` that is skipped when
    /// `None`. `AdminConfig` carries `Secret` key material
    /// (`[[server.admin.write_keys]]`/`read_keys`), and `Secret` serializes as
    /// the literal string `[redacted]`; a section unconditionally present in
    /// `Config::default()` is round-tripped through `Serialized::defaults` in
    /// [`Config::load`], so figment would re-extract `[redacted]` as the
    /// operator's real key. See `config/secrets.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminConfig>,
    /// Optional spend-limit policy (`[server.spend]`). Absent ⇒ the spend-limit
    /// routes are not registered. Requires `[server.admin]`, whose credentials
    /// authenticate them — deliberately independent of `[server.gateway]`, so a
    /// deployment that never serves gateway login can still run spend limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendConfig>,
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
    /// Optional opt-in inbound `GET /api/oauth/usage` synthesizer for Claude
    /// Code's own native usage bars (see `docs/m14-oauth-usage-endpoint.md`).
    /// Absent ⇒ the route is not registered (today's HTTP surface unchanged,
    /// the path 404s as it does now). Auth is bind-topology-gated, not
    /// credential-matched — see the milestone doc for why (the CLI's own
    /// Anthropic OAuth bearer, not a shunt client token, is what actually
    /// arrives on this route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_usage: Option<OauthUsageConfig>,
    /// Optional account-pool tuning (issue #135) and opt-in usage-API
    /// reconciliation. Absent ⇒ legacy quota selection and no background polling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolConfig>,
    /// Optional opt-in upstream Statuspage polling. Absent, or present with no
    /// `sources`, ⇒ no background polling and the admin dashboard's "Upstream
    /// status" strip stays hidden. Observation-only: never consulted by
    /// routing, failover, or pool/cooldown decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusConfig>,
    /// Idle seconds before shunt injects an SSE `ping` event into a streaming
    /// response so middlebox timers (Cloudflare's 100s → 524) never expire.
    /// `0` disables injection (M5).
    #[serde(default = "default_sse_keepalive_seconds")]
    pub sse_keepalive_seconds: u64,
    /// Maximum inbound client requests in flight at once on the limited routes
    /// (`/` and `/health` are merged outside the gate and always answer). `0`
    /// disables the limit. Over-limit requests are shed with 503 rather than
    /// queued, so the number of in-flight request states is finite where it
    /// previously was not (issue #260). This bounds request *count*, not bytes:
    /// each admitted request may still buffer a body up to
    /// `MAX_REQUEST_BODY_BYTES`, and a permit is held for as long as the request
    /// is in flight, so this is not on its own a resident-memory bound.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// Inbound client-address allow/deny policy.
    #[serde(default)]
    pub access_control: AccessControlConfig,
    /// Inbound request size limits.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Upstream response-header timeout.
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    /// Per-IP limits for unauthenticated device-flow endpoints.
    #[serde(default)]
    pub rate_limits: RateLimitsConfig,
}

fn default_sse_keepalive_seconds() -> u64 {
    30
}

fn default_max_concurrent_requests() -> usize {
    1024
}

/// Upper bound accepted for `[server] max_concurrent_requests`, mirroring
/// `tokio::sync::Semaphore::MAX_PERMITS` (`usize::MAX >> 3`). Tokio's
/// `Semaphore::new` asserts on this, so validating it here turns a boot-time
/// panic into a `shunt check` failure.
pub(crate) const MAX_CONCURRENT_REQUESTS_LIMIT: usize = usize::MAX >> 3;

/// `[server.pool]` — quota-aware load-balancing tuning and optional usage-API
/// reconciliation for Claude (Anthropic) and Codex (ChatGPT) account pools
/// (issue #135). Both backends supply quota windows used by threshold and
/// burn-rate selection; per-account `priority`/`disabled` also apply to both.
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
    /// Poll Claude's `/api/oauth/usage` and Codex's `/wham/usage` every N
    /// seconds for refreshable accounts. Unset or `0` disables polling;
    /// positive values below 60 are clamped to 60 seconds.
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
    /// Interval, in seconds, at which a stale near-quota Codex/ChatGPT-family
    /// account is opportunistically promoted to the front of selection once,
    /// so it takes live traffic and refreshes its observed quota (issue
    /// #135's safety net for pools with no usage poller). Unset defaults to
    /// 900 seconds when `[server.pool]` is configured; `0` disables
    /// re-probing; a positive value below 60 is clamped up to a 60-second
    /// floor. When `[server.pool]` itself is absent, re-probing is disabled
    /// regardless of this value (pre-#135 behavior). The outbound Responses
    /// pool also suppresses re-probing for providers with WebSocket enabled;
    /// inbound HTTP selection continues to probe. Claude and Kimi accounts are
    /// never probed (see `reprobe_interval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reprobe_seconds: Option<u64>,
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
            reprobe_seconds: None,
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

/// `[server.status]` — opt-in, observation-only polling of provider Statuspage
/// JSON APIs (`summary.json`). Purely informational: the polled result is
/// exposed via a metric and the admin dashboard, and is never consulted by
/// routing, failover, or pool/cooldown decisions. Absent, or present with an
/// empty `sources` list, ⇒ no background polling.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusConfig {
    /// Poll each source every N seconds. `0` disables polling; positive
    /// values below 60 are clamped to 60 seconds.
    #[serde(default = "default_status_refresh_seconds")]
    pub refresh_seconds: u64,
    /// Provider Statuspage sources to poll. Empty ⇒ no background polling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<StatusSource>,
}

/// A single provider's Statuspage JSON endpoint (e.g.
/// `https://status.claude.com/api/v2/summary.json`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusSource {
    /// Label used for the metric's `provider` attribute and the admin
    /// dashboard row. Must be unique across `sources`.
    pub provider: String,
    /// The Statuspage `summary.json` URL. Must be `http` or `https`.
    pub url: String,
}

pub(crate) fn default_status_refresh_seconds() -> u64 {
    300
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            refresh_seconds: default_status_refresh_seconds(),
            sources: Vec::new(),
        }
    }
}

impl StatusConfig {
    /// The effective poll interval, or `None` when polling is disabled.
    pub fn refresh_interval(&self) -> Option<u64> {
        match self.refresh_seconds {
            0 => None,
            seconds => Some(seconds.max(60)),
        }
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
/// from `[server.auth]`: client tokens are handed to devices, admin credentials
/// add upstream accounts and administer spend limits. Tokens live in the
/// environment as `name:token` pairs (`SHUNT_ADMIN_TOKENS="ops:3f9c…"`),
/// reusing the inbound-auth format and constant-time compare;
/// `[[server.admin.write_keys]]` / `[[server.admin.read_keys]]` add
/// per-credential ids and a read-only tier. Absent ⇒ no admin routes exist.
/// See `docs/m9-admin-surface.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminConfig {
    /// Header carrying the admin credential for API/curl calls. `x-api-key` is
    /// accepted alongside it on the admin and spend routers.
    #[serde(default = "default_admin_header")]
    pub header: String,
    /// Env var holding the `name:token` admin pairs. Retained for
    /// compatibility; these are the write tier (full access is read plus
    /// write). New deployments should prefer the key arrays below.
    #[serde(default = "default_admin_tokens_env")]
    pub tokens_env: String,
    /// Optional file holding `name:token` admin pairs, as an alternative to the
    /// environment variable — written by `shunt dashboard setup` so the admin
    /// surface works without exporting a secret into the launch env. One pair
    /// per line (or comma-separated). `tokens_env`, when non-empty, wins.
    #[serde(default, deserialize_with = "deserialize_optional_credentials_path")]
    pub tokens_file: Option<String>,
    /// Full-access keys: read plus write, the same tier as `tokens_env`.
    /// Each key must be supplied by a `${VAR}` / `${file:}` reference or a
    /// `SHUNT_*` override — a literal in the config file is rejected at load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_keys: Vec<AdminKey>,
    /// Read-only keys: they pass every GET on the admin and spend surfaces and
    /// are refused on every mutation, including the browser login form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_keys: Vec<AdminKey>,
    /// Browser session lifetime after login.
    #[serde(default = "default_admin_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// Pending-login lifetime (time to open the authorize URL and paste back).
    #[serde(default = "default_admin_pending_ttl_secs")]
    pub pending_ttl_secs: u64,
    /// Optional external identity provider for browser sign-in.
    #[serde(default)]
    pub oidc: Option<AdminOidcConfig>,
}

/// Fields shared by every `[*.oidc]` provider table. Kept in one struct so the
/// admin and gateway OIDC configs cannot drift and are not counted as duplicated.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OidcProviderConfig {
    pub issuer: String,
    pub client_id: String,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
}

/// `[server.admin.oidc]` — optional OIDC provider for admin browser sign-in.
/// Admin tokens remain mandatory; OIDC is an additional browser login path.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminOidcConfig {
    /// Externally reachable admin origin used to build the callback URL.
    pub public_url: String,
    #[serde(default = "default_admin_oidc_secret_env")]
    pub client_secret_env: String,
    #[serde(flatten)]
    pub provider: OidcProviderConfig,
}

fn default_admin_header() -> String {
    "x-shunt-admin-token".to_string()
}

fn default_admin_tokens_env() -> String {
    "SHUNT_ADMIN_TOKENS".to_string()
}

/// `~/.shunt/admin-token` (`HOME`, falling back to `USERPROFILE` on Windows), or
/// `None` when neither is set. This is where `shunt dashboard setup` writes the
/// generated admin token and what it records as `[server.admin].tokens_file`.
pub fn default_admin_token_file() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
        .map(|home| home.join(".shunt").join("admin-token"))
}

fn default_admin_oidc_secret_env() -> String {
    "SHUNT_ADMIN_OIDC_SECRET".to_string()
}

fn default_admin_session_ttl_secs() -> u64 {
    3600
}

fn default_admin_pending_ttl_secs() -> u64 {
    600
}

impl AdminConfig {
    /// Resolve the configured admin credentials into the runtime admin-auth
    /// state. Fails closed exactly like [`InboundAuthConfig::resolve`]: a
    /// present `[server.admin]` whose three credential sources
    /// (`tokens_env`/`tokens_file`, `write_keys`, `read_keys`) are *all* empty
    /// is a startup error, never a silently-open admin surface. An array-only
    /// deployment, with `tokens_env` unset, boots.
    ///
    /// `Config::validate` reaches every check here through this method, so the
    /// pure array shape checks run on the validation path too.
    pub fn resolve(&self) -> Result<crate::admin::AdminAuth, ConfigError> {
        admin_keys::validate_key_arrays(&self.write_keys, &self.read_keys)?;
        let header = axum::http::HeaderName::from_bytes(self.header.as_bytes()).map_err(|_| {
            ConfigError::InvalidAdminHeader {
                header: self.header.clone(),
            }
        })?;
        let mut raw = std::env::var(&self.tokens_env).unwrap_or_default();
        let mut source = self.tokens_env.clone();
        // The env var wins; fall back to the token file only when it is unset or
        // empty, so an explicit export always overrides the on-disk secret.
        if raw.trim().is_empty() {
            if let Some(path) = &self.tokens_file {
                let contents = std::fs::read_to_string(path).map_err(|error| {
                    ConfigError::UnreadableAdminTokensFile {
                        path: path.clone(),
                        message: error.to_string(),
                    }
                })?;
                // `parse_tokens` splits on commas; normalise newlines so a
                // one-pair-per-line file parses the same as a comma list.
                raw = contents.replace(['\r', '\n'], ",");
                source = path.clone();
            }
        }
        let tokens = if raw.trim().is_empty() {
            Vec::new()
        } else {
            crate::auth::inbound::parse_tokens(&raw).map_err(|message| {
                ConfigError::InvalidAdminTokens {
                    env: self.tokens_env.clone(),
                    message,
                }
            })?
        };
        if tokens.is_empty() && self.write_keys.is_empty() && self.read_keys.is_empty() {
            return Err(ConfigError::MissingAdminTokens {
                env: self.tokens_env.clone(),
            });
        }
        admin_keys::check_key_uniqueness(&tokens, &self.write_keys, &self.read_keys)?;
        admin_keys::warn_short_tokens(&tokens, &source);
        let mut auth = crate::admin::AdminAuth::new(
            header,
            AdminKeyring::new(&tokens, &self.write_keys, &self.read_keys),
            std::time::Duration::from_secs(self.session_ttl_secs),
            std::time::Duration::from_secs(self.pending_ttl_secs),
        );
        if let Some(oidc) = &self.oidc {
            let public_url = resolve_public_origin(&oidc.public_url, |message| {
                ConfigError::InvalidAdminOidc { message }
            })?;
            auth = auth.with_oidc(
                public_url.as_str().trim_end_matches('/').to_string(),
                oidc.resolve()?,
            );
        }
        Ok(auth)
    }
}

/// `[server.gateway.oidc]` — optional OIDC provider for browser approval.
/// Secrets remain environment-backed and an allowlist is mandatory.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayOidcConfig {
    #[serde(default = "default_gateway_oidc_secret_env")]
    pub client_secret_env: String,
    #[serde(flatten)]
    pub provider: OidcProviderConfig,
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
    /// Deprecated in favor of `[server.gateway.session] jwt_secret`; set at
    /// most one of the two (both unset falls back to the historical default
    /// env var name below).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_secret_env: Option<String>,
    /// Env var holding comma-separated `email:secret` approval users.
    #[serde(default = "default_gateway_users_env")]
    pub users_env: String,
    /// Access-token lifetime in seconds. Deprecated in favor of
    /// `[server.gateway.session] ttl_hours`; set at most one of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_ttl_seconds: Option<u64>,
    /// Honor `X-Forwarded-For`/`X-Real-IP` for `/device` rate limiting.
    /// Enable only behind a trusted proxy that replaces client-supplied values.
    #[serde(default)]
    pub trust_forwarded_for: bool,
    /// Ordered per-user managed-settings policies. `None` keeps the endpoint at
    /// its explicit "no managed policy" 404 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<GatewayPolicyConfig>>,
    /// Client telemetry configuration. A non-empty `forward_to` list both
    /// pushes the telemetry enable flag plus five `OTEL_*` environment
    /// variables through managed settings (M-B) and relays the OTLP payloads
    /// those clients then post to `POST /v1/{metrics,logs,traces}` (M-C, #189).
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
    /// Optional external identity provider for browser approval.
    #[serde(default)]
    pub oidc: Option<GatewayOidcConfig>,
    /// `[server.gateway.session]` — the upstream Claude apps gateway
    /// `session:` block; see `GatewaySessionConfig`. Supersedes
    /// `jwt_secret_env` and `token_ttl_seconds` above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<GatewaySessionConfig>,
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

/// One inbound-telemetry relay destination: a base OTLP/HTTP endpoint, the
/// same shape as `OTEL_EXPORTER_OTLP_ENDPOINT`. shunt appends the signal path
/// (`/v1/metrics`, `/v1/logs`, `/v1/traces`) when relaying.
///
/// Signals are opted in per destination. Metrics default on; logs and traces
/// default off because Claude Code log records and spans can carry command
/// lines, prompts, and file paths, so forwarding them off-host is an explicit
/// operator decision.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayTelemetryDestination {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, Secret>>,
    #[serde(default = "default_true")]
    pub metrics: bool,
    #[serde(default)]
    pub logs: bool,
    #[serde(default)]
    pub traces: bool,
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

fn default_gateway_oidc_secret_env() -> String {
    "SHUNT_GATEWAY_OIDC_SECRET".to_string()
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
        let public_url = resolve_public_origin(&self.public_url, |message| {
            ConfigError::InvalidGatewayPublicUrl { message }
        })?;
        // `session` is required to carry `jwt_secret` (no default), so its
        // presence alone means the replacement key is set — fail closed
        // rather than silently pick a side when both the legacy key and its
        // replacement are configured.
        if self.jwt_secret_env.is_some() && self.session.is_some() {
            return Err(ConfigError::GatewaySessionJwtSecretConflict);
        }
        if self.token_ttl_seconds.is_some()
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.ttl_hours.is_some())
        {
            return Err(ConfigError::GatewaySessionTtlConflict);
        }
        let ttl_seconds = match self.session.as_ref().and_then(|session| session.ttl_hours) {
            Some(hours) => {
                if hours == 0 {
                    return Err(ConfigError::InvalidGatewaySessionTtlHours);
                }
                hours
                    .checked_mul(3600)
                    .ok_or(ConfigError::GatewaySessionTtlHoursOverflow)?
            }
            None => match self.token_ttl_seconds {
                Some(0) => return Err(ConfigError::InvalidGatewayTokenTtl),
                Some(seconds) => seconds,
                None => default_gateway_token_ttl_seconds(),
            },
        };
        let secrets = match &self.session {
            Some(session) => resolve_session_jwt_secrets(session)?,
            None => {
                let env = self
                    .jwt_secret_env
                    .clone()
                    .unwrap_or_else(default_gateway_jwt_secret_env);
                let secret = std::env::var(&env).unwrap_or_default();
                if secret.len() < 32 {
                    return Err(ConfigError::InvalidGatewayJwtSecret { env });
                }
                vec![secret.into_bytes()]
            }
        };
        let raw_users = std::env::var(&self.users_env).unwrap_or_default();
        let approval = if raw_users.trim().is_empty() {
            if self.oidc.is_none() {
                return Err(ConfigError::MissingGatewayUsers {
                    env: self.users_env.clone(),
                });
            }
            None
        } else {
            let users =
                crate::gateway::approval::StaticUsers::parse(&raw_users).map_err(|message| {
                    ConfigError::InvalidGatewayUsers {
                        env: self.users_env.clone(),
                        message,
                    }
                })?;
            Some(std::sync::Arc::new(users)
                as std::sync::Arc<
                    dyn crate::gateway::approval::ApprovalProvider,
                >)
        };
        let policies = resolve_gateway_policies(self.policies.as_deref())?;
        let telemetry_push = validate_gateway_telemetry(self.telemetry.as_ref())?;
        let mut auth = crate::gateway::GatewayAuth::with_optional_approval(
            public_url.as_str().trim_end_matches('/').to_string(),
            secrets[0].clone(),
            ttl_seconds,
            self.trust_forwarded_for,
            approval,
        )
        .with_signing_secrets(secrets);
        if let Some(oidc) = &self.oidc {
            auth = auth.with_oidc(oidc.resolve()?);
        }
        Ok(auth.with_managed_policies(policies, telemetry_push))
    }

    /// Boot-time deprecation warnings for the legacy `jwt_secret_env` /
    /// `token_ttl_seconds` keys, which `[server.gateway.session]`
    /// supersedes. Pure and independently testable so `Config::load` only
    /// has to log whatever it returns.
    pub fn deprecations(&self) -> Vec<String> {
        let mut messages = Vec::new();
        if self.jwt_secret_env.is_some() {
            messages.push(
                "[server.gateway] jwt_secret_env is deprecated; use \
                 [server.gateway.session] jwt_secret instead"
                    .to_string(),
            );
        }
        if self.token_ttl_seconds.is_some() {
            messages.push(
                "[server.gateway] token_ttl_seconds is deprecated; use \
                 [server.gateway.session] ttl_hours instead"
                    .to_string(),
            );
        }
        messages
    }
}

/// Validates every secret in `session.jwt_secret` (non-empty list, each
/// entry at least 32 bytes, the offending index named on failure) and
/// returns them as signing/verification key bytes in file order — index 0
/// signs, every entry verifies.
fn resolve_session_jwt_secrets(
    session: &GatewaySessionConfig,
) -> Result<Vec<Vec<u8>>, ConfigError> {
    if session.jwt_secret.is_empty() {
        return Err(ConfigError::EmptyGatewaySessionJwtSecret);
    }
    session
        .jwt_secret
        .iter()
        .enumerate()
        .map(|(index, secret)| {
            if secret.expose().len() < 32 {
                Err(ConfigError::InvalidGatewaySessionJwtSecret { index })
            } else {
                Ok(secret.expose().as_bytes().to_vec())
            }
        })
        .collect()
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
) -> Result<crate::gateway::managed::TelemetryPush, ConfigError> {
    let mut push = crate::gateway::managed::TelemetryPush::default();
    let Some(telemetry) = telemetry else {
        return Ok(push);
    };
    for (index, destination) in telemetry.forward_to.iter().enumerate() {
        validate_gateway_telemetry_destination(destination, index)?;
        push.metrics |= destination.metrics;
        push.logs |= destination.logs;
        push.traces |= destination.traces;
    }
    Ok(push)
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
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::InvalidGatewayTelemetryUrl {
            index,
            message: format!(
                "must be an http(s) URL with a host, got `{}`",
                destination.url
            ),
        });
    }
    // The destination is a *base* endpoint that shunt extends by string
    // concatenation to reach `/v1/<signal>`. A query or fragment would land on
    // the wrong side of that join (`…?tenant=x/v1/metrics`), and userinfo would
    // put a credential in a URL that reaches error paths and logs. All three
    // are rejected at boot rather than silently misrouted.
    let offending = if url.query().is_some() {
        Some("a query string")
    } else if url.fragment().is_some() {
        Some("a fragment")
    } else if !url.username().is_empty() || url.password().is_some() {
        Some("embedded credentials")
    } else {
        None
    };
    if let Some(offending) = offending {
        return Err(ConfigError::InvalidGatewayTelemetryUrl {
            index,
            message: format!(
                "must be a base OTLP endpoint (scheme, host, optional path) without {offending}; \
                 shunt appends `/v1/<signal>` to it"
            ),
        });
    }
    // Headers are validated here, at boot and on every reload, so a typo in a
    // collector's auth header fails `shunt check` instead of being dropped at
    // relay time — where it would silently exclude that header for the life of
    // the process after a single warning.
    for (name, value) in destination.headers.iter().flatten() {
        let part = if reqwest::header::HeaderName::try_from(name.as_str()).is_err() {
            "name"
        } else if reqwest::header::HeaderValue::try_from(value.expose()).is_err() {
            "value"
        } else {
            continue;
        };
        return Err(ConfigError::InvalidGatewayTelemetryHeader {
            index,
            name: name.clone(),
            part,
        });
    }
    Ok(())
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

impl GatewayOidcConfig {
    fn resolve(&self) -> Result<crate::gateway::ResolvedIdp, ConfigError> {
        resolve_oidc(
            OidcSection::Gateway,
            &self.provider.issuer,
            &self.provider.client_id,
            &self.client_secret_env,
            &self.provider.allowed_domains,
            &self.provider.allowed_emails,
            &self.provider.scopes,
            &self.provider.authorization_endpoint,
            &self.provider.token_endpoint,
            &self.provider.userinfo_endpoint,
        )
    }
}

impl AdminOidcConfig {
    fn resolve(&self) -> Result<crate::gateway::ResolvedIdp, ConfigError> {
        resolve_oidc(
            OidcSection::Admin,
            &self.provider.issuer,
            &self.provider.client_id,
            &self.client_secret_env,
            &self.provider.allowed_domains,
            &self.provider.allowed_emails,
            &self.provider.scopes,
            &self.provider.authorization_endpoint,
            &self.provider.token_endpoint,
            &self.provider.userinfo_endpoint,
        )
    }
}

#[derive(Clone, Copy)]
enum OidcSection {
    Gateway,
    Admin,
}

impl OidcSection {
    fn invalid(self, message: impl Into<String>) -> ConfigError {
        let message = message.into();
        match self {
            Self::Gateway => ConfigError::InvalidGatewayOidc { message },
            Self::Admin => ConfigError::InvalidAdminOidc { message },
        }
    }

    fn missing_secret(self, env: String) -> ConfigError {
        match self {
            Self::Gateway => ConfigError::MissingGatewayOidcSecret { env },
            Self::Admin => ConfigError::MissingAdminOidcSecret { env },
        }
    }

    fn missing_allowlist(self) -> ConfigError {
        match self {
            Self::Gateway => ConfigError::MissingGatewayOidcAllowlist,
            Self::Admin => ConfigError::MissingAdminOidcAllowlist,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_oidc(
    section: OidcSection,
    issuer: &str,
    client_id: &str,
    client_secret_env: &str,
    allowed_domains: &[String],
    allowed_emails: &[String],
    scopes: &[String],
    authorization_endpoint: &Option<String>,
    token_endpoint: &Option<String>,
    userinfo_endpoint: &Option<String>,
) -> Result<crate::gateway::ResolvedIdp, ConfigError> {
    let issuer = issuer.trim();
    if issuer.is_empty() {
        return Err(section.invalid("issuer must not be empty"));
    }
    let issuer_url = validate_idp_url(section, issuer, true, "issuer")?;
    let issuer = if issuer_url.path() == "/" {
        issuer_url.as_str().trim_end_matches('/').to_string()
    } else {
        issuer_url.as_str().to_string()
    };
    if client_id.trim().is_empty() {
        return Err(section.invalid("client_id must not be empty"));
    }
    let client_secret = std::env::var(client_secret_env).unwrap_or_default();
    if client_secret.trim().is_empty() {
        return Err(section.missing_secret(client_secret_env.to_string()));
    }
    let allowed_domains: Vec<_> = allowed_domains
        .iter()
        .map(|domain| domain.trim().to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
        .collect();
    let allowed_emails: Vec<_> = allowed_emails
        .iter()
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| !email.is_empty())
        .collect();
    if allowed_domains.is_empty() && allowed_emails.is_empty() {
        return Err(section.missing_allowlist());
    }
    let endpoint = |value: &Option<String>, key| {
        value
            .as_deref()
            .map(|raw| validate_idp_url(section, raw, false, key))
            .transpose()
            .map(|url| url.map(Into::into))
    };
    let scopes = if scopes.is_empty() {
        ["openid", "email", "profile"]
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        let scopes: Vec<_> = scopes
            .iter()
            .map(|scope| scope.trim())
            .filter(|scope| !scope.is_empty())
            .map(str::to_string)
            .collect();
        for required in ["openid", "email"] {
            if !scopes.iter().any(|scope| scope == required) {
                return Err(section.invalid(format!("scopes must include {required}")));
            }
        }
        scopes
    };
    Ok(crate::gateway::ResolvedIdp {
        issuer,
        client_id: client_id.trim().to_string(),
        client_secret,
        allowed_domains,
        allowed_emails,
        scopes,
        authorization_endpoint: endpoint(authorization_endpoint, "authorization_endpoint")?,
        token_endpoint: endpoint(token_endpoint, "token_endpoint")?,
        userinfo_endpoint: endpoint(userinfo_endpoint, "userinfo_endpoint")?,
    })
}

fn resolve_public_origin(
    raw: &str,
    invalid: impl Fn(String) -> ConfigError,
) -> Result<reqwest::Url, ConfigError> {
    let public_url = reqwest::Url::parse(raw.trim()).map_err(|error| invalid(error.to_string()))?;
    let secure_origin = url_uses_safe_transport(&public_url);
    let bare_origin = public_url.host_str().is_some()
        && public_url.username().is_empty()
        && public_url.password().is_none()
        && public_url.path() == "/"
        && public_url.query().is_none()
        && public_url.fragment().is_none();
    if !secure_origin || !bare_origin {
        return Err(invalid(
            "must be an https origin (http is allowed only on loopback) with no userinfo, path, query, or fragment"
                .to_string(),
        ));
    }
    Ok(public_url)
}

fn url_uses_safe_transport(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        || url.scheme() == "http" && host_is_loopback(url.host_str().unwrap_or_default())
}

fn validate_idp_url(
    section: OidcSection,
    raw: &str,
    issuer: bool,
    key: &str,
) -> Result<reqwest::Url, ConfigError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|error| section.invalid(format!("{key} is not a valid URL: {error}")))?;
    let invalid_issuer_parts = issuer && url.query().is_some();
    if !url_uses_safe_transport(&url)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || invalid_issuer_parts
    {
        let parts = if issuer {
            "userinfo, query, or fragment"
        } else {
            "userinfo or fragment"
        };
        return Err(section.invalid(format!(
            "{key} must use https (or http on loopback), include a host, and contain no {parts}"
        )));
    }
    Ok(url)
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

/// `[server.oauth_usage]` — opt-in inbound `GET /api/oauth/usage` synthesizer.
/// When present, shunt registers the exact route the Claude Code CLI's own
/// `fetchUtilization` calls, so its native usage bars (`Current session`,
/// `Current week`, and — when a Fable-scoped bucket is tracked — `Current
/// week (Fable)`) show real numbers when the CLI is pointed at shunt.
/// Presence alone opts in; the table has no fields today. Unlike
/// `[server.usage]`, this endpoint is **not** gated by `[server.auth]` on a
/// loopback bind (the CLI presents its own Anthropic OAuth bearer here, not a
/// shunt client token — see `docs/m14-oauth-usage-endpoint.md`, "Auth
/// gating"); on a non-loopback bind it requires `[server.auth]` or
/// `[server.gateway]` to be configured. See that milestone doc for the
/// verified precondition (which CLI login modes actually trigger the fetch)
/// and the aggregation policy (Claude-only, routing-aware priority-tiered
/// worst case — deliberately not the same aggregate `[server.usage]` uses).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OauthUsageConfig {}

/// `[sentry]` — opt-in error reporting to the operator's own Sentry project.
/// Reports gateway-owned diagnostics (fatal startup/serve errors, panics, and
/// `error!` log events, with `warn!`/`info!` as breadcrumbs) plus,
/// unconditionally once a client is bound, an event whenever an upstream
/// provider itself returns a failure: `error` for a 5xx response, `warning`
/// for 429/529 (rate limit/overload), each tagged only with `model`,
/// `provider`, and `upstream_status`; request/response bodies, headers, and
/// credentials never are. Metrics and performance tracing are each a
/// further, separate opt-in (`metrics` / `traces_sample_rate`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SentryConfig {
    /// DSN of the operator's Sentry project. An empty string disables
    /// reporting, so `SHUNT_SENTRY__DSN=""` can turn a TOML-configured section
    /// off without editing the file.
    pub dsn: Secret,
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
        !self.dsn.expose().trim().is_empty()
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
    pub headers: BTreeMap<String, Secret>,
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
    /// Optional default Responses `service_tier` for `kind = "responses"`
    /// providers -- Codex CLI's "Fast" mode (faster responses, increased usage).
    /// Accepts `fast` (Codex's original name for this tier, normalized to
    /// the `priority` wire value), `priority`, `flex`, or `default` (a
    /// client-only sentinel meaning "unset"; never sent on the wire).
    /// Normalized and validated at config load -- see
    /// `Config::normalize_service_tiers`. Off by default (opt-in).
    #[serde(default)]
    pub service_tier: Option<String>,
    /// How `POST /v1/messages/count_tokens` is answered for this provider.
    #[serde(default)]
    pub count_tokens: CountTokens,
    /// Explicit OAuth accounts for a `claude_oauth` (Anthropic), `chatgpt_oauth`
    /// (Codex), or `kimi_oauth` (Kimi Code) provider. An empty list means the
    /// account store directory will be scanned by the account-pool layer.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    /// Names of account-store entries selected by the `[[upstreams]].auth`
    /// scoping syntax. Inline account tables remain in `accounts`; an empty scope
    /// preserves the existing whole-store scan behavior. For `chatgpt_oauth`, an
    /// empty store still falls back to `~/.codex/auth.json` as before.
    #[serde(skip)]
    pub account_scope: Vec<String>,
    /// Opt in to the Codex Responses WebSocket v2 transport for this provider
    /// (issue #32). Only honored for the ChatGPT/Codex backend; ignored for
    /// stock OpenAI/xAI upstreams, which have no v2 websocket endpoint. When on,
    /// shunt reaches the backend over `wss://…/codex/responses` with the
    /// `responses_websockets` beta protocol, transparently falling back to HTTP
    /// if the websocket cannot be established (a mid-stream failure surfaces as an
    /// error event). Off by default — HTTP stays the default transport.
    #[serde(default)]
    pub websocket: bool,
    /// Use the OpenAI Responses native client-executed `tool_search` protocol
    /// for Claude Code's tool search (issue #82) instead of the #43 text-based
    /// progressive-reveal shim. The shim must add each revealed deferred tool
    /// to the `tools` array, which is part of the cacheable prompt prefix, so
    /// every reveal invalidates the cached prefix and forces a full
    /// re-process; native keeps deferred tools out of `tools` and appends a
    /// `tool_search_output` item instead, leaving the prefix stable (issue
    /// #286). Three states: `Some(true)` forces native, `Some(false)` forces
    /// the shim, and the unset default (`None`, "auto") takes the native path
    /// only for a host known to implement the protocol — stock OpenAI
    /// (`api.openai.com`) and the ChatGPT/Codex backend. Any other
    /// OpenAI-compatible endpoint (LiteLLM, vLLM, OpenRouter, a self-hosted
    /// proxy, ...) keeps the shim unless it opts in explicitly, since most
    /// third-party Responses backends don't implement `tool_search` items and
    /// would otherwise fail the turn instead of falling back (issue #289).
    /// Still gated on flavor and model either way — see
    /// [`Config::native_tool_search`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_search: Option<bool>,
    /// zstd-compress this provider's Responses request bodies (issue #285),
    /// mirroring what the Codex CLI itself sends to the ChatGPT backend
    /// (`Content-Encoding: zstd`, level 3). On by default, but only effective on
    /// the ChatGPT/Codex flavor — see [`Config::responses_request_compression`]
    /// for the gate. Set `false` to send uncompressed bodies (e.g. through a
    /// middlebox that mishandles a compressed request body).
    #[serde(default = "default_true")]
    pub request_compression: bool,
    /// Bounded upstream retry/backoff for transient failures (issue #48).
    /// Applies to this provider's single-credential upstream calls (the
    /// `passthrough`/`api_key` Anthropic path, the single-credential Responses
    /// path — `api_key`, `xai_oauth`, or an unpooled `chatgpt_oauth` provider —
    /// and the Cursor path); the `claude_oauth`/`chatgpt_oauth`/`kimi_oauth`
    /// account pools have their own account-rotation failover and are
    /// unaffected. On by default with conservative settings — set
    /// `max_retries = 0` to disable.
    #[serde(default)]
    pub retry: RetryConfig,
    /// Directories the Antigravity CLI may be pointed at by request content
    /// (`kind = "antigravity_cli"` only).
    ///
    /// `agy` runs with `--dangerously-skip-permissions`, so whatever directory
    /// it starts in is a directory an unattended agent can read, write, and run
    /// shell commands in. The adapter can take that directory from a
    /// `Working directory:` line in the request's system prompt, which is
    /// client-controlled text — and system prompts routinely quote fetched
    /// documents and tool output, so it is prompt-injectable.
    ///
    /// This list is the trust boundary. A prompt-derived path is canonicalized
    /// (resolving symlinks and `..`) and used only if it lands inside one of
    /// these roots. A path outside them is refused, but it is the *path* that
    /// is dropped, not the request: the run logs the rejection and falls back
    /// to the gateway's own working directory, exactly as it would had the
    /// prompt named no directory at all. Failing the turn instead would let
    /// anyone able to inject one line of system-prompt text break every
    /// request. Empty (the default) means no prompt-derived path is ever
    /// honored, and only `SHUNT_AGY_WORKSPACE` or the gateway's own directory
    /// is used.
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    /// Run the Antigravity CLI with `--sandbox` (`kind = "antigravity_cli"` only).
    ///
    /// On by default. Without it, `--dangerously-skip-permissions` leaves an
    /// unattended agent with shell access and no workspace boundary: refusing a
    /// directory in [`workspace_roots`](Self::workspace_roots) only changes
    /// where the agent *starts*, and a path named in the prompt can still be
    /// reached from there. `--sandbox` is what actually keeps reads and writes
    /// inside the workspace.
    ///
    /// Set `false` only where the agent genuinely needs unrestricted terminal
    /// access and the caller is trusted.
    #[serde(default = "default_true")]
    pub sandbox: bool,
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
#[serde(deny_unknown_fields)]
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
    /// Applies to Claude, Codex, and Kimi pools alike.
    #[serde(default = "default_account_priority")]
    pub priority: u32,
    /// Exclude this account from pool selection entirely without removing its
    /// configuration. Applies to Claude, Codex, and Kimi pools alike.
    #[serde(default)]
    pub disabled: bool,
    /// Runtime-only provenance used to distinguish a store entry from an inline
    /// account whose UUID-less name fallback must remain upstream-scoped.
    #[doc(hidden)]
    #[serde(skip)]
    pub store_entry: bool,
    /// Runtime-only store namespace assigned while resolving an OAuth pool.
    #[doc(hidden)]
    #[serde(skip)]
    pub store_family: Option<crate::accounts::StoreFamily>,
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
            store_entry: false,
            store_family: None,
        }
    }
}

/// Configured accounts that will share a single pool slot, grouped by their
/// shared identity. Grouping is by the pool's own [`crate::accounts::account_key`]
/// (not the display string), so an explicit `uuid` (`Verified`) is never reported
/// as colliding with a UUID-less name fallback (`UpstreamInline` / `StoreEntry`)
/// even when the strings happen to match — the pool keeps those distinct, so
/// warning on a bare string match would tell operators a separate account is
/// coalesced when it is not.
pub(crate) fn identity_collisions(
    upstream: &str,
    accounts: &[AccountConfig],
) -> Vec<(String, Vec<String>)> {
    let mut groups =
        std::collections::HashMap::<crate::accounts::AccountKey, (String, Vec<String>)>::new();
    for account in accounts {
        let key = crate::accounts::account_key(upstream, account);
        groups
            .entry(key)
            .or_insert_with(|| {
                (
                    crate::accounts::account_identity(account).to_string(),
                    Vec::new(),
                )
            })
            .1
            .push(account.name.clone());
    }
    let mut collisions: Vec<(String, Vec<String>)> = groups
        .into_values()
        .filter(|(_, names)| names.len() > 1)
        .collect();
    // Deterministic order for logging/tests (the source `HashMap` is unordered).
    collisions.sort();
    collisions
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
    /// Google Gemini via the Code Assist backend — Anthropic Messages are
    /// translated to Gemini `generateContent`/`streamGenerateContent`, wrapped
    /// in the Code Assist `{model,project,request}` envelope. Auth reuses a
    /// Google OAuth subscription token (`google_oauth`).
    Gemini,
    /// Antigravity over its native HTTP backend. Wire-identical to the Code
    /// Assist path `kind = "gemini"` speaks (`v1internal:generateContent` under
    /// the `{model,project,request}` envelope), but authenticated with an
    /// Antigravity subscription token rather than a Gemini CLI one, and carrying
    /// `ideType: ANTIGRAVITY` through project discovery. Requires
    /// `auth = "antigravity_oauth"`.
    Antigravity,
    /// Local Antigravity CLI binary (`agy`) execution.
    ///
    /// **Deprecated.** Superseded by `kind = "antigravity"`, which reaches the
    /// same service over HTTP without depending on an installed binary, on
    /// `PATH`, or on a subprocess. Retained so existing deployments keep working
    /// while they migrate; it will be removed once the HTTP transport reaches
    /// parity.
    AntigravityCli,
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
    /// Kimi Code subscription OAuth, acquired via the device-code flow
    /// (`shunt login kimi --name <account-name>`) and stored per-account in
    /// `~/.shunt/accounts/kimi/<name>.json`.
    KimiOauth,
    /// Google OAuth subscription token (Gemini Code Assist / Google One AI Pro),
    /// reused from the gemini CLI login (`~/.gemini/oauth_creds.json`). shunt
    /// can refresh it when operator-supplied client credentials are present.
    /// Only valid for
    /// `kind = "gemini"`.
    GoogleOauth,
    /// Antigravity subscription OAuth, acquired via `shunt login antigravity`
    /// and stored in ~/.shunt/antigravity-auth.json. Shares Google's OAuth
    /// endpoints with [`AuthMode::GoogleOauth`] but uses the Antigravity client
    /// and scopes, so the two credentials are not interchangeable. Only valid
    /// for `kind = "antigravity"`.
    AntigravityOauth,
    /// No authentication header sent (e.g. local subprocess CLI adapters).
    None,
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

/// Whether `host` is the stock OpenAI Responses API host, exactly
/// (`api.openai.com`, no subdomains). Used by [`Config::native_tool_search`]
/// to decide whether an "auto" (unset `tool_search`) provider may default to
/// the native protocol. Unlike `host_is_xai`/`host_is_cursor`/
/// `host_is_anthropic`, which widen to any subdomain to avoid leaking a
/// subscription bearer off one operator's origin, this check is narrowed to
/// the single documented Responses endpoint on purpose: other `openai.com`
/// subdomains (e.g. `chat.openai.com`, `platform.openai.com`) are different
/// products with no guarantee they implement `tool_search` items the same
/// way, so trusting the whole domain would risk silently promoting an
/// unverified host to the native wire shape.
fn host_is_openai(host: &str) -> bool {
    host == "api.openai.com"
}

/// Whether `host` belongs to the ChatGPT/Codex backend (`chatgpt.com` or any
/// subdomain). Used to reject a `chatgpt_oauth` provider pointed at a
/// non-ChatGPT host, so shunt never leaks a Codex subscription bearer to
/// another origin.
pub fn host_is_chatgpt(host: &str) -> bool {
    host == "chatgpt.com" || host.ends_with(".chatgpt.com")
}

/// Whether `host` belongs to the Google Code Assist backend
/// (`cloudcode-pa.googleapis.com`). Used to reject a `google_oauth`
/// provider pointed at a non-Code-Assist host, so shunt never
/// leaks the reused Google subscription bearer off-origin.
pub fn host_is_google_codeassist(host: &str) -> bool {
    host == "cloudcode-pa.googleapis.com"
}

/// Whether `host` is the `daily-` Code Assist control plane
/// (`daily-cloudcode-pa.googleapis.com`). This is the backend the Antigravity
/// client itself addresses for both discovery and inference, and is shunt's
/// default for `kind = "antigravity"`.
pub fn host_is_antigravity_daily(host: &str) -> bool {
    host == "daily-cloudcode-pa.googleapis.com"
}

/// Whether `host` is a backend an `antigravity_oauth` provider may address.
///
/// Deliberately wider than [`host_is_google_codeassist`] and deliberately not
/// "any googleapis.com host": the Antigravity backend answers on the `daily-`
/// control plane as well as on production, and the two are the only hosts that
/// answer for Antigravity. Every other `googleapis.com` name — the `sandbox`
/// spellings included — is a different product, so a subscription bearer must
/// not reach it.
pub fn host_is_antigravity_backend(host: &str) -> bool {
    host_is_google_codeassist(host) || host_is_antigravity_daily(host)
}

/// Whether `host` belongs to Kimi (`kimi.com` or any subdomain, which covers
/// the measured `api.kimi.com` API host). Used to reject a `kimi_oauth`
/// provider pointed at a non-Kimi host, so shunt never leaks a Kimi Code
/// subscription bearer to another origin.
pub fn host_is_kimi(host: &str) -> bool {
    host == "kimi.com" || host.ends_with(".kimi.com")
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
    /// See [`ProviderConfig::service_tier`]; a route-level value wins over
    /// the provider-level default.
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub id: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub upstream_model: Option<BTreeMap<String, String>>,
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
    #[error("declare either [[upstreams]] or [providers.*], not both; use exactly one provider declaration form")]
    MixedProviderDeclarationForms,
    #[error("failed to parse config file: {message}")]
    InvalidConfigSyntax { message: String },
    #[error("{path} has an unterminated ${{ reference: no closing brace found")]
    UnterminatedReference { path: String },
    #[error("{path} has an empty ${{}} reference; name an environment variable or use ${{file:/abs/path}}")]
    EmptyReferenceName { path: String },
    #[error("{path} references \"{name}\", which is not a valid environment variable name (must match [A-Za-z_][A-Za-z0-9_]*)")]
    InvalidReferenceVarName { path: String, name: String },
    #[error("{path} references environment variable \"{var}\", which is not set")]
    UndefinedReferenceVar { path: String, var: String },
    #[error("{path} references environment variable \"{var}\", whose value is not valid Unicode")]
    NonUnicodeReferenceVar { path: String, var: String },
    #[error("{path} uses unknown reference scheme \"{scheme}\"; only \"file\" is supported")]
    UnknownReferenceScheme { path: String, scheme: String },
    #[error("{path} references file \"{file}\", which is not an absolute path")]
    RelativeFileReference { path: String, file: String },
    #[error("{path} embeds a ${{file:...}} reference inside a longer string; a file reference must be the field's entire value")]
    EmbeddedFileReference { path: String },
    #[error("{path} references file \"{file}\", which could not be read: {message}")]
    UnreadableReferenceFile {
        path: String,
        file: String,
        message: String,
    },
    #[error(
        "provider \"{provider}\" runs the Antigravity CLI with sandbox = false, which gives an \
         autonomous agent shell access as the user running shunt; that cannot be served on the \
         non-loopback bind \"{bind}\". Remove `sandbox = false`, or bind to loopback (127.0.0.1)."
    )]
    UnsandboxedAntigravityOnPublicBind { provider: String, bind: String },
    #[error("upstreams[{index}].name must be non-empty and non-whitespace")]
    EmptyUpstreamName { index: usize },
    #[error("duplicate [[upstreams]] name \"{name}\"; upstream names must be unique")]
    DuplicateUpstreamName { name: String },
    #[error("upstreams[{upstream}].provider references unknown preset \"{preset}\"; available presets: {available}")]
    UnknownProviderPreset {
        upstream: String,
        preset: String,
        available: String,
    },
    #[error("upstreams[{upstream}].kind is required when provider preset is not set")]
    MissingUpstreamKind { upstream: String },
    #[error("upstreams[{upstream}].base_url is required when provider preset is not set")]
    MissingUpstreamBaseUrl { upstream: String },
    #[error("upstreams[{upstream}].auth uses mode = \"api_key\" but env is not set and the preset supplies no default")]
    MissingUpstreamApiKeyEnv { upstream: String },
    #[error("upstreams[{upstream}].auth must set at most one of account or accounts")]
    UpstreamAuthAccountConflict { upstream: String },
    #[error("upstreams[{upstream}].auth accounts must not be explicitly empty; omit accounts to use the whole account store")]
    EmptyUpstreamAccountList { upstream: String },
    #[error("upstreams[{upstream}].auth account references must be non-empty and non-whitespace")]
    EmptyUpstreamAccountReference { upstream: String },
    #[error("SHUNT_PROVIDERS__{upstream}__... references an undeclared [[upstreams]] name")]
    UnknownUpstreamEnvOverride { upstream: String },
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
    #[error("providers.{provider} uses auth = \"google_oauth\" but kind is not \"gemini\"; the anthropic adapter would forward the client's own credential instead of the Google token")]
    GoogleOauthWrongKind { provider: String },
    #[error("providers.{provider} uses auth = \"google_oauth\" but base_url host {host} is not a googleapis.com host; refusing to send a subscription token off-origin")]
    GoogleOauthNonGoogleHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"google_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    GoogleOauthNotHttps { provider: String },
    #[error("providers.{provider} uses auth = \"antigravity_oauth\" but kind is not \"antigravity\"; another adapter would forward the client's own credential instead of the Antigravity token")]
    AntigravityOauthWrongKind { provider: String },
    #[error("providers.{provider} uses auth = \"antigravity_oauth\" but base_url host {host} is neither daily-cloudcode-pa.googleapis.com nor cloudcode-pa.googleapis.com; refusing to send a subscription token off-origin")]
    AntigravityOauthNonGoogleHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"antigravity_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    AntigravityOauthNotHttps { provider: String },
    #[error(
        "providers.{provider} uses kind = \"antigravity\" with auth = \"{auth}\", but \
         kind = \"antigravity\" is now the native HTTP upstream and requires \
         auth = \"antigravity_oauth\". The local `agy` CLI transport moved to \
         kind = \"antigravity_cli\" (built-in provider `antigravity-cli`), which is \
         deprecated. Pick one explicitly rather than have the transport change underneath you."
    )]
    AntigravityKindRequiresOauth { provider: String, auth: String },
    #[error(
        "providers.antigravity has no `auth` key, so it deep-merges the built-in \
         antigravity_oauth default instead of being caught by the kind = \"antigravity\" \
         guard. kind = \"antigravity\" is now the native HTTP upstream and requires an \
         explicit auth = \"antigravity_oauth\". The local `agy` CLI transport moved to \
         kind = \"antigravity_cli\" (built-in provider `antigravity-cli`), which is \
         deprecated. Pick one explicitly rather than have the transport change underneath you."
    )]
    AntigravityLegacyTableMissingAuth,
    #[error("{0}")]
    AntigravityMigrationRequired(String),
    #[error("providers.{provider}.accounts requires auth = \"claude_oauth\", \"chatgpt_oauth\", or \"kimi_oauth\"")]
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
    #[error("providers.{provider} uses auth = \"kimi_oauth\" but kind is not \"anthropic\"")]
    KimiOauthWrongKind { provider: String },
    #[error("providers.{provider} uses auth = \"kimi_oauth\" but base_url host {host} is not kimi.com; refusing to send a subscription token off-origin")]
    KimiOauthNonKimiHost { provider: String, host: String },
    #[error("providers.{provider} uses auth = \"kimi_oauth\" but base_url is not https; refusing to send a subscription token over plaintext")]
    KimiOauthNotHttps { provider: String },
    #[error("providers.{provider}.accounts contains duplicate account name \"{name}\"")]
    DuplicateAccountName { provider: String, name: String },
    #[error("providers.{provider}.accounts account name \"{name}\" must match [a-z0-9-]+")]
    InvalidAccountName { provider: String, name: String },
    #[error("providers.{provider}.accounts account \"{name}\" sets both credentials and token_env; set at most one credential source")]
    AccountMultipleCredentialSources { provider: String, name: String },
    #[error("server.pool.{key} must be between 0.0 and 1.0, got {value}")]
    InvalidPoolThreshold { key: &'static str, value: f64 },
    #[error("[server.status].sources[{index}].provider must not be empty")]
    InvalidStatusSourceProvider { index: usize },
    #[error("[server.status].sources[{index}].url is invalid: {message}")]
    InvalidStatusSourceUrl { index: usize, message: String },
    #[error("[server.status] has more than one source named \"{provider}\"; provider names must be unique")]
    DuplicateStatusSourceProvider { provider: String },
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
    #[error("[server.oauth_usage] on a non-loopback [server.bind] requires [server.auth] or [server.gateway]: without one, Claude subscription quota telemetry would be served to any caller on the network")]
    OauthUsageEndpointRequiresAuthOnNonLoopback,
    #[error("providers.{provider} (claude_oauth) base_url resolves to this gateway's own [server.bind] with [server.oauth_usage] enabled: the outbound usage poller would read back its own synthesized aggregate instead of Anthropic's real usage")]
    OauthUsageSelfPollLoop { provider: String },
    #[error("route for model {model} references unknown provider: {provider}")]
    UnknownRouteProvider { model: String, provider: String },
    #[error("providers.{provider}.service_tier must be one of fast, priority, flex, default; got \"{value}\"")]
    InvalidProviderServiceTier { provider: String, value: String },
    #[error("route for model {model} service_tier must be one of fast, priority, flex, default; got \"{value}\"")]
    InvalidRouteServiceTier { model: String, value: String },
    #[error("models entry {model} upstream_model references unknown provider: {provider}")]
    UnknownModelProvider { model: String, provider: String },
    #[error("models entry {model} upstream_model must name exactly one provider (got {count}) when using legacy [providers.*]; rewrite as ordered upstreams:\n{rewrite}")]
    ModelUpstreamProviderCount {
        model: String,
        count: usize,
        rewrite: String,
    },
    #[error("models entry {model} upstream_model provider name must not be empty")]
    EmptyModelUpstreamProvider { model: String },
    #[error("models entry {model} upstream_model for provider {provider} must not be empty")]
    EmptyModelUpstream { model: String, provider: String },
    #[error("model {model} is declared both in [[routes]] and in a [[models]] upstream_model entry; remove one")]
    ModelRouteConflict { model: String },
    #[error("models entry {model} has an upstream_model map but its id ends with a [1m] or [1M] context-window hint; clients strip that hint before model matching, so the entry is unreachable")]
    ModelUpstreamContextWindowHint { model: String },
    #[error("duplicate [[models]] id {model}; ids must be unique when any matching entry has an upstream_model map")]
    DuplicateModelId { model: String },
    #[error("route prefix {prefix} references unknown provider: {provider}")]
    UnknownPrefixProvider { prefix: String, provider: String },
    #[error("server.auth.header is not a valid header name: {header}")]
    InvalidAuthHeader { header: String },
    #[error("server.admin.header is not a valid header name: {header}")]
    InvalidAdminHeader { header: String },
    #[error(
        "[server.admin] is set but resolved no credentials ({env} is unset or empty and no \
         write_keys/read_keys are configured); refusing to run open"
    )]
    MissingAdminTokens { env: String },
    #[error("[server.admin] tokens_file {path} could not be read: {message}")]
    UnreadableAdminTokensFile { path: String, message: String },
    #[error("[server.admin] tokens in {env} are invalid: {message}")]
    InvalidAdminTokens { env: String, message: String },
    #[error("[server.admin.oidc] is invalid: {message}")]
    InvalidAdminOidc { message: String },
    #[error("[server.admin.oidc] requires {env} to contain a non-empty client secret")]
    MissingAdminOidcSecret { env: String },
    #[error("[server.admin.oidc] requires at least one allowed_domains or allowed_emails entry")]
    MissingAdminOidcAllowlist,
    #[error("[server.gateway] public_url is invalid: {message}")]
    InvalidGatewayPublicUrl { message: String },
    #[error("[server.gateway] token_ttl_seconds must be greater than zero")]
    InvalidGatewayTokenTtl,
    #[error(
        "[server.gateway] requires {env} to contain a JWT signing secret of at least 32 bytes"
    )]
    InvalidGatewayJwtSecret { env: String },
    #[error(
        "[server.gateway] jwt_secret_env conflicts with [server.gateway.session] jwt_secret; \
         set exactly one"
    )]
    GatewaySessionJwtSecretConflict,
    #[error(
        "[server.gateway] token_ttl_seconds conflicts with [server.gateway.session] ttl_hours; \
         set exactly one"
    )]
    GatewaySessionTtlConflict,
    #[error("[server.gateway.session] jwt_secret must contain at least one secret")]
    EmptyGatewaySessionJwtSecret,
    #[error("[server.gateway.session] jwt_secret[{index}] must be at least 32 bytes")]
    InvalidGatewaySessionJwtSecret { index: usize },
    #[error("[server.gateway.session] ttl_hours must be greater than zero")]
    InvalidGatewaySessionTtlHours,
    #[error("[server.gateway.session] ttl_hours is too large to convert to seconds")]
    GatewaySessionTtlHoursOverflow,
    #[error(
        "[server.gateway] is set but {env} is unset or empty; no approval users are configured"
    )]
    MissingGatewayUsers { env: String },
    #[error("[server.gateway] users in {env} are invalid: {message}")]
    InvalidGatewayUsers { env: String, message: String },
    #[error("[server.gateway.oidc] is invalid: {message}")]
    InvalidGatewayOidc { message: String },
    #[error("[server.gateway.oidc] requires {env} to contain a non-empty client secret")]
    MissingGatewayOidcSecret { env: String },
    #[error("[server.gateway.oidc] requires at least one allowed_domains or allowed_emails entry")]
    MissingGatewayOidcAllowlist,
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
    #[error("[[server.admin.{field}]][{index}].id must be a non-empty name")]
    BlankAdminKeyId { field: &'static str, index: usize },
    #[error("[[server.admin.{field}]] key {id:?} must be at least 32 characters")]
    ShortAdminKey { field: &'static str, id: String },
    #[error(
        "[server.admin] credential id {id:?} is duplicated across tokens_env, write_keys, and read_keys"
    )]
    DuplicateAdminKeyId { id: String },
    #[error(
        "[server.admin] credential value is duplicated by ids {first_id:?} and {second_id:?} across tokens_env, write_keys, and read_keys"
    )]
    DuplicateAdminKeyValue { first_id: String, second_id: String },
    /// The key value is deliberately not echoed — it is an admin credential.
    #[error(
        "[{path}] holds an admin key written literally in the config file; supply it with \
         ${{VAR}} or ${{file:}} (or a SHUNT_* override) so the key does not live in the file"
    )]
    LiteralAdminKey { path: String },
    #[error("[server.spend] requires [server.admin]: the spend-limit API authenticates with the admin credential")]
    SpendRequiresAdmin,
    /// The header value is deliberately not echoed — it is typically a
    /// collector API key.
    #[error(
        "[server.gateway.telemetry].forward_to[{index}].headers has an invalid entry `{name}`: \
         header {part} is not valid HTTP"
    )]
    InvalidGatewayTelemetryHeader {
        index: usize,
        name: String,
        part: &'static str,
    },
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
        "server.max_concurrent_requests must be at most {limit}, got {max_concurrent_requests}"
    )]
    InvalidMaxConcurrentRequests {
        max_concurrent_requests: usize,
        limit: usize,
    },
    #[error("server.access_control.{field}[{index}] is not a valid CIDR `{value}`: {message}")]
    InvalidAccessControlCidr {
        field: &'static str,
        index: usize,
        value: String,
        message: String,
    },
    #[error("server.limits.max_request_bytes must be greater than zero")]
    InvalidMaxRequestBytes,
    #[error("server.limits.max_request_header_bytes must be greater than zero when set")]
    InvalidMaxRequestHeaderBytes,
    #[error("server.limits.max_url_length must be greater than zero when set")]
    InvalidMaxUrlLength,
    #[error("server.rate_limits.{limit}.max must be greater than zero")]
    InvalidRateLimitMax { limit: &'static str },
    #[error("server.rate_limits.{limit}.window_seconds must be greater than zero")]
    InvalidRateLimitWindow { limit: &'static str },
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
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: Vec::new(),
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: RetryConfig::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
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
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: Vec::new(),
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: RetryConfig::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
        }
    }

    /// A `Gemini`-kind provider on the Google Code Assist backend, reusing a
    /// Google OAuth subscription token (`google_oauth`). Used for the built-in
    /// `gemini` provider.
    fn gemini(base_url: &str) -> Self {
        Self {
            kind: ProviderKind::Gemini,
            base_url: base_url.to_string(),
            auth: AuthMode::GoogleOauth,
            api_key_env: None,
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: Vec::new(),
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: RetryConfig::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
        }
    }

    /// An `Antigravity`-kind provider on the Antigravity HTTP backend, reusing
    /// an Antigravity subscription token (`antigravity_oauth`). Used for the
    /// built-in `antigravity` provider.
    fn antigravity(base_url: &str) -> Self {
        Self {
            kind: ProviderKind::Antigravity,
            base_url: base_url.to_string(),
            auth: AuthMode::AntigravityOauth,
            api_key_env: None,
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: Vec::new(),
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: RetryConfig::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
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
                    service_tier: None,
                    count_tokens: CountTokens::default(),
                    accounts: Vec::new(),
                    account_scope: Vec::new(),
                    websocket: false,
                    tool_search: None,
                    request_compression: true,
                    retry: RetryConfig::default(),
                    workspace_roots: Vec::new(),
                    sandbox: true,
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
            (
                // Google Gemini via the Code Assist backend, reusing the Google
                // One AI Pro subscription token (google_oauth reads the gemini
                // CLI login; optional refresh uses operator-supplied client
                // credentials). Reached by routing model ids
                // like gemini-3.1-pro-preview / gemini-3-flash-preview to it,
                // exactly as the codex/grok subscription providers are routed.
                "gemini".to_string(),
                ProviderConfig::gemini("https://cloudcode-pa.googleapis.com"),
            ),
            (
                // Antigravity over its native HTTP backend, authenticated with
                // `shunt login antigravity`. Same service the `agy` CLI reaches,
                // without the subprocess — including the `daily-` control
                // plane that client addresses for both discovery and
                // inference.
                "antigravity".to_string(),
                ProviderConfig::antigravity("https://daily-cloudcode-pa.googleapis.com"),
            ),
            (
                // Local Antigravity CLI binary (`agy`) execution for Gemini
                // models. Deprecated in favour of the `antigravity` provider
                // above; retained for deployments still on the subprocess
                // transport.
                "antigravity-cli".to_string(),
                ProviderConfig {
                    kind: ProviderKind::AntigravityCli,
                    base_url: "http://localhost".to_string(),
                    auth: AuthMode::None,
                    api_key_env: None,
                    api_key_header: ApiKeyHeader::Bearer,
                    effort: None,
                    service_tier: None,
                    count_tokens: CountTokens::default(),
                    accounts: Vec::new(),
                    account_scope: Vec::new(),
                    websocket: false,
                    tool_search: None,
                    request_compression: true,
                    retry: RetryConfig::default(),
                    workspace_roots: Vec::new(),
                    sandbox: true,
                },
            ),
        ]);
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:3001".to_string(),
                default_provider: "anthropic".to_string(),
                auth: None,
                admin: None,
                spend: None,
                gateway: None,
                codex_endpoint: None,
                usage: None,
                oauth_usage: None,
                pool: None,
                status: None,
                sse_keepalive_seconds: default_sse_keepalive_seconds(),
                max_concurrent_requests: default_max_concurrent_requests(),
                access_control: AccessControlConfig::default(),
                limits: LimitsConfig::default(),
                timeouts: TimeoutsConfig::default(),
                rate_limits: RateLimitsConfig::default(),
            },
            providers,
            upstreams: Vec::new(),
            upstream_order: ["anthropic", "codex", "cursor", "grok", "openai", "xai"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            upstreams_ordered: false,
            auto_include_builtin_models: true,
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
pub(crate) const CONFIG_FILENAMES: [&str; 3] = ["shunt.toml", "shunt.yaml", "shunt.yml"];

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
pub(crate) enum ConfigFormat {
    Toml,
    Yaml,
}

impl ConfigFormat {
    /// Detect the format from a path's extension. `.yaml`/`.yml` (any case)
    /// are YAML; everything else — including no extension — is TOML, which
    /// preserves the historical `shunt.toml` default.
    pub(crate) fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") => {
                ConfigFormat::Yaml
            }
            _ => ConfigFormat::Toml,
        }
    }
}

/// Refuses a `[providers.antigravity]` shape that omits `auth` before it ever
/// reaches the merge against the built-in `antigravity` default.
///
/// Figment deep-merges nested tables: any key the *effective* config omits is
/// filled in from the base (`Serialized::defaults(Config::default())`)
/// layer, and the built-in `antigravity` entry already sets
/// `auth = "antigravity_oauth"`. A pre-#372 config never carried an `auth`
/// key under this name -- `kind = "antigravity"` meant "run the local `agy`
/// binary", and there was nothing to authenticate over HTTP with -- so after
/// the merge it would end up with `auth = "antigravity_oauth"` anyway,
/// passing the `AntigravityKindRequiresOauth` guard in [`Config::validate`]
/// (which only ever sees the already-merged value) and silently switching
/// transport. `effective_figment` is the file's own table (if any) merged
/// with any `providers.antigravity.*` env overrides layered on top --
/// exactly the layering `Config::load` itself applies below -- rather than
/// the built-in-defaults figment. Probing the file layer alone (the previous
/// shape of this check) missed both directions: a legacy shape assembled
/// entirely, or completed, via `SHUNT_PROVIDERS__ANTIGRAVITY__*` env vars was
/// invisible to a check that only ever looked at the file, and a file table
/// an env var legitimately completes with `auth` was rejected before the env
/// layer had a chance to complete it.
fn reject_legacy_antigravity_table_without_auth(
    effective_figment: &Figment,
) -> Result<(), ConfigError> {
    let Ok(value) = effective_figment.find_value("providers.antigravity") else {
        return Ok(());
    };
    let Some(dict) = value.as_dict() else {
        return Ok(());
    };
    // An explicit `kind` naming something else (e.g. migrating straight to
    // `antigravity_cli` under this table name) is not the ambiguous legacy
    // shape; an omitted `kind` inherits the built-in default, which is
    // `antigravity`.
    let kind_is_antigravity = dict
        .get("kind")
        .and_then(figment::value::Value::as_str)
        .map(|kind| kind == "antigravity")
        .unwrap_or(true);
    if kind_is_antigravity && !dict.contains_key("auth") {
        return Err(ConfigError::AntigravityLegacyTableMissingAuth);
    }
    Ok(())
}

/// Normalizes a raw `service_tier` config value to its Responses wire form.
/// `default` is preserved as its own sentinel string rather than collapsed to
/// `None`: `None` also means "not configured", and collapsing the two made an
/// explicit route-level `default` indistinguishable from an unset route, so
/// it silently inherited the provider-level tier instead of overriding it
/// (issue #301). The sentinel is stripped only at the wire-emission site
/// (`model/responses_request.rs`), which never sends the literal string
/// `"default"`. `None` here means the input was invalid.
fn normalize_service_tier_value(value: &str) -> Option<&'static str> {
    match value {
        "fast" | "priority" => Some("priority"),
        "flex" => Some("flex"),
        "default" => Some("default"),
        _ => None,
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(path) => Some(path.to_path_buf()),
            None => Self::find_config_file(),
        };
        let mut figment = Figment::from(Serialized::defaults(Self::default()));
        let mut file_declares_upstreams = false;
        // The file layer alone (no built-in defaults merged in), kept around
        // past this block so it can be re-merged with the env layer below to
        // decide the legacy-antigravity-table guard against the *effective*
        // config rather than the file alone. `None` when there is no config
        // file.
        let mut file_figment: Option<Figment> = None;
        // Literal (non-reference) string values found in the file, keyed by
        // value and valued by the dotted field path(s) they appeared at —
        // used only to warn about a `Secret` field holding a plaintext
        // credential. Left empty when there is no config file.
        let mut literal_values = HashMap::new();
        // Values that must never be mistaken for a literal secret: every
        // value produced by resolving a `${VAR}`/`${file:...}` reference in
        // the file, plus every current `SHUNT_*` env var's value (a `Secret`
        // fed by either can coincidentally share text with an unrelated
        // literal elsewhere without that literal's warning misfiring onto
        // it). See `secrets::record_literal_hit`.
        let mut never_literal_values = HashSet::new();
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
            // Resolve `${VAR}`/`${file:...}` references in every string value
            // of the file tree before it ever reaches figment. This is the
            // file layer only — `SHUNT_*` env overrides below are never
            // passed through this pass, so they are never re-resolved.
            let format = ConfigFormat::from_path(path);
            let substituted = secrets::substitute(&raw, format)?;
            // An admin key written literally in the file is rejected outright
            // rather than warned about like the older `Secret` fields: the key
            // arrays are new, so no deployment holds a literal there yet, and
            // rejecting closes the path where an admin credential is committed
            // to a config file. A value fed by `${VAR}`/`${file:}` lands in
            // `resolved_values`, not `literals`, so only a real literal trips
            // this. Env overrides never reach the file layer at all.
            secrets::reject_literal_admin_keys(&substituted.literals)?;
            literal_values = substituted.literals;
            never_literal_values = substituted.resolved_values;
            // Probe only the file layer: serialized defaults always contain the
            // built-in providers, and env overrides are allowed under either form.
            let probed_file_figment = match format {
                ConfigFormat::Toml => Figment::from(Toml::string(&substituted.text)),
                ConfigFormat::Yaml => Figment::from(Yaml::string(&substituted.text)),
            };
            let file_declares_providers = probed_file_figment.find_value("providers").is_ok();
            file_declares_upstreams = probed_file_figment.find_value("upstreams").is_ok();
            if file_declares_providers && file_declares_upstreams {
                return Err(ConfigError::MixedProviderDeclarationForms);
            }
            // The parser is chosen by extension so TOML and YAML configs are
            // both accepted; an unknown extension is treated as TOML.
            figment = figment.merge(&probed_file_figment);
            file_figment = Some(probed_file_figment);
        }
        never_literal_values.extend(secrets::shunt_env_values());
        let env = Env::prefixed("SHUNT_").split("__");
        // Decide the legacy-antigravity-table guard against the *effective*
        // providers.antigravity shape: the file's own table (if any) merged
        // with any `providers.antigravity.*` env overrides layered on top,
        // mirroring exactly how the env layer is scoped for the real
        // extraction below (`file_declares_upstreams` excludes `providers.*`
        // env keys under the ordered-upstreams form). Built without the
        // `Serialized::defaults` base layer, so an omitted `auth` is still
        // visibly absent rather than already backfilled from the built-in
        // default.
        let provider_env = if file_declares_upstreams {
            env.clone().filter(|key| !key.starts_with("providers."))
        } else {
            env.clone()
        };
        let mut effective_provider_figment = Figment::new();
        if let Some(file_figment) = &file_figment {
            effective_provider_figment = effective_provider_figment.merge(file_figment);
        }
        effective_provider_figment = effective_provider_figment.merge(provider_env);
        reject_legacy_antigravity_table_without_auth(&effective_provider_figment)?;
        // Scopes the literal-value map for the extraction below so
        // `Secret::deserialize` can record which config-file paths held a
        // secret written verbatim, for the aggregated warning after
        // validation. Dropped (and the thread-local cleared) once this
        // function returns.
        let literal_scope = secrets::LiteralScope::enter(literal_values, never_literal_values);
        let mut config: Self = if file_declares_upstreams {
            // Provider env overrides address normalized upstreams by name; applying
            // them to the defaults first would let an env var create a legacy
            // provider and make `providers` leak back into the ordered namespace.
            figment
                .merge(env.clone().filter(|key| !key.starts_with("providers.")))
                .extract()
                .map_err(Box::new)?
        } else {
            figment.merge(env.clone()).extract().map_err(Box::new)?
        };
        config.normalize_upstreams()?;
        if file_declares_upstreams {
            config.apply_ordered_provider_env(env)?;
        }
        config.backfill_antigravity_cli_migration_auth(&effective_provider_figment);
        let config = config.validate()?;
        // This diagnostic belongs to the successful load boundary. Runtime
        // defensive validation and `shunt check` also call `validate`, so
        // keeping it there would repeat the same warning on every validation.
        config.warn_reprobe_seconds_below_floor();
        // One aggregated warning per load naming every `Secret` field whose
        // value was written literally in the config file — never the value
        // itself. A `Secret` populated from an env override, a `${...}`
        // reference, or a default has no entry here and is not reported. A
        // literal value whose path could not be attributed is reported as a
        // count rather than guessed at. Attribution fails for two unrelated
        // reasons — the same value sits at more than one Secret-shaped path,
        // or no path matched because the allowlist has drifted behind a newly
        // added Secret field — so the message states only that the field is
        // unidentified, never why. Advisory only: a literal secret is allowed
        // and never fails the load.
        let literal_hits = secrets::LiteralScope::hits();
        let unattributed_hits = secrets::LiteralScope::unattributed_count();
        drop(literal_scope);
        if !literal_hits.is_empty() || unattributed_hits > 0 {
            let paths = literal_hits
                .iter()
                .map(|path| secrets::format_literal_path(path))
                .collect::<Vec<_>>()
                .join(", ");
            match (literal_hits.is_empty(), unattributed_hits) {
                (true, unattributed) if unattributed > 0 => {
                    tracing::warn!(
                        "{unattributed} config value(s) are written literally in the config \
                         file but could not be attributed to a specific field; if they are \
                         credentials, prefer ${{VAR}} or ${{file:}} so the secret does not \
                         live in the file"
                    );
                }
                (false, 0) => {
                    tracing::warn!(
                        "config values at {paths} are written literally in the config file; if \
                         they are credentials, prefer ${{VAR}} or ${{file:}} so the secret does \
                         not live in the file"
                    );
                }
                (false, unattributed) => {
                    tracing::warn!(
                        "config values at {paths} are written literally in the config file, and \
                         {unattributed} additional value(s) could not be attributed to a \
                         specific field; if they are credentials, prefer ${{VAR}} or \
                         ${{file:}} so the secret does not live in the file"
                    );
                }
                _ => unreachable!("guarded by the outer `if`"),
            }
        }
        // Collision reporting belongs to the load boundary rather than
        // validation: RuntimeState defensively re-validates an already-loaded
        // config, and logging there would emit the same warning twice.
        config.warn_identity_collisions();
        if let Some(gateway) = config.server.gateway.as_ref() {
            for message in gateway.deprecations() {
                tracing::warn!("{message}");
            }
        }
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
            for (identity, accounts) in identity_collisions(name, &provider.accounts) {
                tracing::warn!(
                    provider = %name,
                    identity = %identity,
                    accounts = ?accounts,
                    "multiple account names share one upstream identity; the pool will treat them as one account"
                );
            }
        }
    }

    fn normalize_upstreams(&mut self) -> Result<(), ConfigError> {
        if self.upstreams.is_empty() {
            self.upstream_order = self.providers.keys().cloned().collect();
            self.upstreams_ordered = false;
        } else if !self.upstreams_ordered {
            let (providers, order) = upstreams::normalize(&self.upstreams)?;
            self.providers = providers;
            self.upstream_order = order;
            self.upstreams_ordered = true;
        }
        Ok(())
    }

    /// Writing `kind = "antigravity_cli"` under `[providers.antigravity]`,
    /// with nothing else, is the documented way to migrate that name back
    /// to the deprecated `agy` subprocess transport (see
    /// `reject_legacy_antigravity_table_without_auth` above, which lets
    /// this exact shape through). Figment's deep-merge still backfills the
    /// omitted `auth` from the built-in `antigravity` default's `auth =
    /// "antigravity_oauth"`, since the merge has no notion that changing
    /// `kind` invalidates the rest of that default -- `validate` then
    /// rejects the result as `AntigravityOauthWrongKind`, breaking the very
    /// migration path the guard above exists to allow. Reassign `auth` to
    /// what the built-in `antigravity-cli` provider itself uses instead.
    ///
    /// Scoped to this one documented name/kind pair rather than generalized
    /// to arbitrary provider names: generalizing would also silently
    /// resolve unrelated kind-mismatched legacy tables (e.g. repurposing
    /// `[providers.antigravity]` to some unrelated kind without an explicit
    /// `auth`) that today fail loudly and correctly at `validate`, turning
    /// that clear, actionable error into a silently invented `auth` choice
    /// instead.
    fn backfill_antigravity_cli_migration_auth(&mut self, effective_figment: &Figment) {
        let Ok(value) = effective_figment.find_value("providers.antigravity") else {
            return;
        };
        let Some(dict) = value.as_dict() else {
            return;
        };
        let migrating_to_cli = dict
            .get("kind")
            .and_then(figment::value::Value::as_str)
            .map(|kind| kind == "antigravity_cli")
            .unwrap_or(false);
        if !migrating_to_cli || dict.contains_key("auth") {
            return;
        }
        if let Some(cli_auth) = Self::default()
            .providers
            .get("antigravity-cli")
            .map(|provider| provider.auth)
        {
            if let Some(provider) = self.providers.get_mut("antigravity") {
                provider.auth = cli_auth;
            }
        }
    }

    /// Validates and normalizes every configured `service_tier` (provider-level
    /// and route-level) to its Responses wire form, in place, fail-closed:
    /// `fast` is Codex's original name for this tier, normalized to `priority`;
    /// `default` is preserved as its own sentinel string rather than collapsed
    /// to `None`, so an explicit route-level `default` stays distinguishable
    /// from an unset route (see `normalize_service_tier_value`); any other
    /// value is rejected. Mirrors the shape of `normalize_upstreams`, but
    /// unconditionally revisits every entry rather than switching on a
    /// declaration-form flag, since `service_tier` is orthogonal to which of
    /// `[[upstreams]]`/`[providers.*]` populated `self.providers`.
    fn normalize_service_tiers(&mut self) -> Result<(), ConfigError> {
        for (name, provider) in self.providers.iter_mut() {
            if let Some(raw) = provider.service_tier.take() {
                provider.service_tier = Some(
                    normalize_service_tier_value(&raw)
                        .ok_or_else(|| ConfigError::InvalidProviderServiceTier {
                            provider: name.clone(),
                            value: raw.clone(),
                        })?
                        .to_string(),
                );
            }
        }
        for route in self.routes.iter_mut() {
            if let Some(raw) = route.service_tier.take() {
                route.service_tier = Some(
                    normalize_service_tier_value(&raw)
                        .ok_or_else(|| ConfigError::InvalidRouteServiceTier {
                            model: route.model.clone(),
                            value: raw.clone(),
                        })?
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    /// Warns once at load when `[server.pool] reprobe_seconds` is a positive
    /// value below the 60-second floor `reprobe_interval` (accounts.rs)
    /// silently clamps up to. The effective interval is read on each HTTP
    /// pool selection, so a warning there would spam one line per request;
    /// surfacing it at the successful load boundary means it fires exactly
    /// once, while repeated runtime validation stays side-effect free.
    fn warn_reprobe_seconds_below_floor(&self) {
        let Some(pool) = &self.server.pool else {
            return;
        };
        if let Some(configured) = pool.reprobe_seconds {
            if configured > 0 && configured < crate::accounts::REPROBE_FLOOR_SECS {
                tracing::warn!(
                    configured_seconds = configured,
                    effective_seconds = crate::accounts::REPROBE_FLOOR_SECS,
                    "reprobe_seconds is below the floor; using the floor"
                );
            }
        }
    }

    /// Warns when a provider or route has an explicitly configured
    /// `service_tier` that resolves to the `xai`/`grok` Responses flavor:
    /// that flavor never sends `service_tier` on the wire (xAI's Responses
    /// endpoint rejects it with a 400), so the configured value is silently
    /// withheld at request time with no other signal. Non-fatal -- existing
    /// configs that happen to set `service_tier` alongside an xai/grok
    /// provider must not start failing.
    fn warn_service_tier_withheld_for_flavor(&self) {
        for (name, provider) in &self.providers {
            if provider
                .service_tier
                .as_deref()
                .is_some_and(|v| v != "default")
                && matches!(
                    self.responses_flavor(name),
                    ResponsesFlavor::Xai | ResponsesFlavor::Grok
                )
            {
                tracing::warn!(
                    provider = %name,
                    service_tier = ?provider.service_tier,
                    "service_tier is configured but withheld on the xai/grok Responses flavor"
                );
            }
        }
        for route in &self.routes {
            if route
                .service_tier
                .as_deref()
                .is_some_and(|v| v != "default")
                && matches!(
                    self.responses_flavor(&route.provider),
                    ResponsesFlavor::Xai | ResponsesFlavor::Grok
                )
            {
                tracing::warn!(
                    model = %route.model,
                    provider = %route.provider,
                    service_tier = ?route.service_tier,
                    "service_tier is configured but withheld on the xai/grok Responses flavor"
                );
            }
        }
    }

    fn apply_ordered_provider_env(&mut self, env: Env) -> Result<(), ConfigError> {
        if !self.upstreams_ordered {
            return Ok(());
        }
        for (key, _) in env.iter() {
            let key = key.as_str();
            let Some(rest) = key.strip_prefix("providers.") else {
                continue;
            };
            let name = rest.split('.').next().unwrap_or_default();
            if !self.providers.contains_key(name) {
                return Err(ConfigError::UnknownUpstreamEnvOverride {
                    upstream: name.to_string(),
                });
            }
        }
        let account_scopes = self
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.account_scope.clone()))
            .collect::<BTreeMap<_, _>>();
        self.providers = Figment::from(Serialized::default("providers", &self.providers))
            .merge(env.filter(|key| key.starts_with("providers.")))
            .extract_inner("providers")
            .map_err(Box::new)?;
        for (name, scope) in account_scopes {
            if let Some(provider) = self.providers.get_mut(&name) {
                provider.account_scope = scope;
            }
        }
        Ok(())
    }

    pub fn validate(mut self) -> Result<Self, ConfigError> {
        self.normalize_upstreams()?;
        // Runs after `normalize_upstreams` so it sees `self.providers` merged
        // from either declaration form ([[upstreams]] or [providers.*]).
        self.normalize_service_tiers()?;
        self.server.bind_addr()?;
        // `tokio::sync::Semaphore::new` panics above `MAX_PERMITS`, so an
        // out-of-range limit would pass `shunt check` and then abort at boot
        // inside `build_router`. The bound is `usize::MAX >> 3`, which is
        // platform-dependent (~5.4e8 on a 32-bit target), so reject it here
        // rather than let it differ by build target. `0` stays the documented
        // way to disable the limit entirely.
        if self.server.max_concurrent_requests > MAX_CONCURRENT_REQUESTS_LIMIT {
            return Err(ConfigError::InvalidMaxConcurrentRequests {
                max_concurrent_requests: self.server.max_concurrent_requests,
                limit: MAX_CONCURRENT_REQUESTS_LIMIT,
            });
        }
        self.server.access_control.validate()?;
        if self.server.limits.max_request_bytes == 0 {
            return Err(ConfigError::InvalidMaxRequestBytes);
        }
        if self.server.limits.max_request_header_bytes == Some(0) {
            return Err(ConfigError::InvalidMaxRequestHeaderBytes);
        }
        if self.server.limits.max_url_length == Some(0) {
            return Err(ConfigError::InvalidMaxUrlLength);
        }
        for (limit, configured) in [
            (
                "device_authorization",
                &self.server.rate_limits.device_authorization,
            ),
            ("device_verify", &self.server.rate_limits.device_verify),
        ] {
            if configured.max == 0 {
                return Err(ConfigError::InvalidRateLimitMax { limit });
            }
            if configured.window_seconds == 0 {
                return Err(ConfigError::InvalidRateLimitWindow { limit });
            }
        }
        // Fail closed at boot: [server.auth] without resolvable tokens is an
        // error, not an open gateway.
        if let Some(auth) = &self.server.auth {
            auth.resolve()?;
        }
        // Fail closed at boot: [server.admin] without resolvable credentials
        // would be an unauthenticated admin surface. Reject it rather than run
        // open. This also runs the admin key arrays' shape and cross-set
        // uniqueness checks (see `AdminConfig::resolve`).
        if let Some(admin) = &self.server.admin {
            admin.resolve()?;
        }
        // `[server.spend]` authenticates with the admin credential, so it
        // cannot be served without one.
        if self.server.spend.is_some() && self.server.admin.is_none() {
            return Err(ConfigError::SpendRequiresAdmin);
        }
        // Fail closed at boot: an unsandboxed Antigravity provider runs an
        // autonomous agent with shell access and no workspace boundary, as the
        // user running shunt. That is defensible as a personal loopback
        // integration; reachable from the network it hands arbitrary local
        // execution to anyone who can post a Messages request. Authentication
        // is not sufficient on its own, so refuse the combination outright
        // rather than document it as merely discouraged. The Antigravity
        // adapter repeats this check against AppState::boot_is_loopback on every
        // request because a reload can change this config value but not the
        // listener the process actually bound.
        if let Some(name) = self.providers.iter().find_map(|(name, provider)| {
            (provider.kind == ProviderKind::AntigravityCli && !provider.sandbox).then_some(name)
        }) {
            if !self.server.bind_is_loopback() {
                return Err(ConfigError::UnsandboxedAntigravityOnPublicBind {
                    provider: name.clone(),
                    bind: self.server.bind.clone(),
                });
            }
        }
        // Fail closed at boot: a configured gateway must have a valid issuer,
        // sufficiently strong signing secret, and at least one approval path.
        if let Some(gateway) = &mut self.server.gateway {
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
        // Fail closed at boot: [server.status] sources are polled unattended in
        // the background, so a malformed URL or a duplicate provider label
        // would otherwise surface only as a silent, permanently-failing poller
        // (or a metric/dashboard row silently overwritten by another source).
        // This is the config-time counterpart to the runtime fail-open rule
        // below: bad config is rejected loudly here, but a *reachable* source
        // that later fails to answer must degrade to `Indicator::Unknown`,
        // never to a silent `None` ("operational").
        if let Some(status) = &self.server.status {
            let mut seen_providers = HashSet::new();
            for (index, source) in status.sources.iter().enumerate() {
                if source.provider.trim().is_empty() {
                    return Err(ConfigError::InvalidStatusSourceProvider { index });
                }
                if !seen_providers.insert(source.provider.as_str()) {
                    return Err(ConfigError::DuplicateStatusSourceProvider {
                        provider: source.provider.clone(),
                    });
                }
                let url = reqwest::Url::parse(source.url.trim()).map_err(|error| {
                    ConfigError::InvalidStatusSourceUrl {
                        index,
                        message: error.to_string(),
                    }
                })?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(ConfigError::InvalidStatusSourceUrl {
                        index,
                        message: format!(
                            "must be an http(s) URL with a host, got `{}`",
                            source.url
                        ),
                    });
                }
                if url.query().is_some()
                    || url.fragment().is_some()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err(ConfigError::InvalidStatusSourceUrl {
                        index,
                        message: "must not contain a query, fragment, or embedded credentials"
                            .to_string(),
                    });
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
                sentry
                    .dsn
                    .expose()
                    .parse::<sentry::types::Dsn>()
                    .map_err(|error| ConfigError::InvalidSentryDsn {
                        message: error.to_string(),
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
                if self.upstreams_ordered {
                    return Err(ConfigError::MissingUpstreamApiKeyEnv {
                        upstream: name.clone(),
                    });
                }
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
            // A google_oauth provider injects the operator's reused Google
            // subscription bearer (Gemini Code Assist), so — like the oauth
            // guards above — its base_url must stay on a googleapis.com host over
            // https (loopback allowed for local debugging proxies), never a
            // gateway or plaintext endpoint that would receive the token. It must
            // also be a `gemini`-kind provider so the Gemini adapter injects the
            // token, rather than the anthropic adapter forwarding the client's
            // own credential off-origin.
            if provider.auth == AuthMode::GoogleOauth {
                if provider.kind != ProviderKind::Gemini {
                    return Err(ConfigError::GoogleOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                if !host_is_loopback(host) {
                    if url.scheme() != "https" {
                        return Err(ConfigError::GoogleOauthNotHttps {
                            provider: name.clone(),
                        });
                    }
                    if !host_is_google_codeassist(host) {
                        return Err(ConfigError::GoogleOauthNonGoogleHost {
                            provider: name.clone(),
                            host: host.to_string(),
                        });
                    }
                }
            }
            // `kind = "antigravity"` used to mean "run the local `agy` binary".
            // It now means the native HTTP upstream, so a config carrying the
            // old meaning must not resolve quietly to a different transport
            // with different credentials, egress, and failure modes. Anything
            // that is not the new auth mode is rejected by name.
            if provider.kind == ProviderKind::Antigravity
                && provider.auth != AuthMode::AntigravityOauth
            {
                return Err(ConfigError::AntigravityKindRequiresOauth {
                    provider: name.clone(),
                    auth: serde_json::to_value(provider.auth)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".to_string()),
                });
            }
            // Mirrors the `google_oauth` guards above: the Antigravity token is
            // a subscription bearer on the same Google host family, so it must
            // stay on one of the two Antigravity backends over https and be
            // carried by the adapter that injects it rather than one that
            // forwards the client's own credential.
            if provider.auth == AuthMode::AntigravityOauth {
                if provider.kind != ProviderKind::Antigravity {
                    return Err(ConfigError::AntigravityOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                if !host_is_loopback(host) {
                    if url.scheme() != "https" {
                        return Err(ConfigError::AntigravityOauthNotHttps {
                            provider: name.clone(),
                        });
                    }
                    if !host_is_antigravity_backend(host) {
                        return Err(ConfigError::AntigravityOauthNonGoogleHost {
                            provider: name.clone(),
                            host: host.to_string(),
                        });
                    }
                }
            }
            if !provider.accounts.is_empty()
                && !matches!(
                    provider.auth,
                    AuthMode::ClaudeOauth | AuthMode::ChatgptOauth | AuthMode::KimiOauth
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
                } else if self.server.oauth_usage.is_some() {
                    // A loopback claude_oauth base_url is allowed to be "any
                    // host" so a local debugging proxy or mock can receive the
                    // bearer (see the comment above) — but if it happens to
                    // land on this gateway's own listener with the usage
                    // synthesizer enabled, the outbound poller would read back
                    // its own synthesized aggregate instead of Anthropic's
                    // real usage. Match on host *and* port so a proxy on a
                    // *different* loopback address (e.g. `[::1]:P` or
                    // `127.0.0.2:P` while shunt binds `127.0.0.1:P`) is not
                    // wrongly rejected — that address cannot reach shunt's
                    // listener. A wildcard bind (`0.0.0.0`/`::`) accepts every
                    // local address, so any same-port loopback host reaches it.
                    // Still a heuristic, not an exhaustive topology check (it
                    // does not resolve DNS names or account for a reverse proxy
                    // in between): it exists to catch the realistic mistake of
                    // copy-pasting shunt's own address into a `claude_oauth`
                    // provider's `base_url`.
                    if let Ok(bind) = self.server.bind_addr() {
                        let port = url.port_or_known_default().unwrap_or(0);
                        // `host_str()` returns a bracketed `[::1]` for an IPv6
                        // literal, which does not parse as an `IpAddr` — strip
                        // the brackets first so an IPv6 literal compares.
                        let host_reaches_bind = match url
                            .host_str()
                            .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
                            .and_then(|h| h.parse::<std::net::IpAddr>().ok())
                        {
                            // IP literal: reaches the listener on an exact
                            // match, or on a wildcard bind. An IPv4 wildcard
                            // (`0.0.0.0`) listens on IPv4 only, so it does not
                            // reach an IPv6 literal like `[::1]`. An IPv6
                            // wildcard (`[::]`) is dual-stack by default and can
                            // accept IPv4 too, so treat it conservatively as
                            // reaching any literal.
                            Some(ip) => {
                                bind.ip() == ip
                                    || (bind.ip().is_unspecified()
                                        && (bind.ip().is_ipv6() || ip.is_ipv4()))
                            }
                            // Non-IP host (e.g. `localhost`): not resolvable
                            // here without DNS, so keep the conservative
                            // same-port match to still catch the copy-paste.
                            None => true,
                        };
                        if port == bind.port() && host_reaches_bind {
                            return Err(ConfigError::OauthUsageSelfPollLoop {
                                provider: name.clone(),
                            });
                        }
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
            // A kimi_oauth provider injects the operator's stored Kimi Code
            // subscription bearer, so — like claude_oauth above — its base_url
            // must stay on a Kimi host over https, never a gateway or plaintext
            // endpoint that would receive the token. It must also be an
            // `anthropic`-kind provider (Kimi's coding API speaks the Anthropic
            // Messages wire shape): the anthropic adapter is what injects the
            // Kimi bearer and X-Msh-* headers, whereas any other adapter would
            // forward the client's own credential off-origin.
            if provider.auth == AuthMode::KimiOauth {
                if provider.kind != ProviderKind::Anthropic {
                    return Err(ConfigError::KimiOauthWrongKind {
                        provider: name.clone(),
                    });
                }
                let host = url.host_str().unwrap_or_default();
                if !host_is_loopback(host) {
                    if url.scheme() != "https" {
                        return Err(ConfigError::KimiOauthNotHttps {
                            provider: name.clone(),
                        });
                    }
                    if !host_is_kimi(host) {
                        return Err(ConfigError::KimiOauthNonKimiHost {
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
        // `[server.oauth_usage]` serves Claude subscription quota telemetry
        // unauthenticated on a loopback bind (the request cannot have
        // originated off the operator's own machine — see the milestone
        // doc's "Auth gating"). A non-loopback bind has no such guarantee, so
        // require at least one of `[server.auth]`/`[server.gateway]` to be
        // configured: the handler validates the caller against that credential
        // (a client-token match or a valid gateway JWT, like `/v1/messages`),
        // so this boot gate guarantees a validator exists rather than leaving
        // the route open to the network.
        if self.server.oauth_usage.is_some() {
            let non_loopback = self
                .server
                .bind_addr()
                .is_ok_and(|addr| !addr.ip().is_loopback());
            if non_loopback && self.server.auth.is_none() && self.server.gateway.is_none() {
                return Err(ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback);
            }
        }
        for route in &self.routes {
            if !self.has_provider(&route.provider) {
                return Err(ConfigError::UnknownRouteProvider {
                    model: route.model.clone(),
                    provider: route.provider.clone(),
                });
            }
        }
        let mut model_ids = HashSet::new();
        let mut model_upstream_ids = HashSet::new();
        for model in &self.models {
            let duplicate_id = !model_ids.insert(&model.id);
            let Some(upstream_models) = &model.upstream_model else {
                if duplicate_id && model_upstream_ids.contains(&model.id) {
                    return Err(ConfigError::DuplicateModelId {
                        model: model.id.clone(),
                    });
                }
                continue;
            };
            if crate::routing::strip_context_window_hint(&model.id) != model.id {
                return Err(ConfigError::ModelUpstreamContextWindowHint {
                    model: model.id.clone(),
                });
            }
            if duplicate_id {
                return Err(ConfigError::DuplicateModelId {
                    model: model.id.clone(),
                });
            }
            model_upstream_ids.insert(&model.id);
            if upstream_models.is_empty() {
                return Err(ConfigError::ModelUpstreamProviderCount {
                    model: model.id.clone(),
                    count: 0,
                    rewrite: String::new(),
                });
            }
            if upstream_models.len() > 1 && !self.upstreams_ordered {
                let rewrite = upstream_models
                    .keys()
                    .map(|provider| {
                        format!("[providers.{provider}] -> [[upstreams]] + name = \"{provider}\"")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(ConfigError::ModelUpstreamProviderCount {
                    model: model.id.clone(),
                    count: upstream_models.len(),
                    rewrite,
                });
            }
            for (provider, upstream_model) in upstream_models {
                if provider.trim().is_empty() {
                    return Err(ConfigError::EmptyModelUpstreamProvider {
                        model: model.id.clone(),
                    });
                }
                if upstream_model.trim().is_empty() {
                    return Err(ConfigError::EmptyModelUpstream {
                        model: model.id.clone(),
                        provider: provider.clone(),
                    });
                }
                if !self.has_provider(provider) {
                    return Err(ConfigError::UnknownModelProvider {
                        model: model.id.clone(),
                        provider: provider.clone(),
                    });
                }
            }
            if self.routes.iter().any(|route| route.model == model.id) {
                return Err(ConfigError::ModelRouteConflict {
                    model: model.id.clone(),
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
            if model.upstream_model.is_none()
                && !self.routes.iter().any(|route| route.model == model.id)
            {
                tracing::warn!(
                    model_id = %model.id,
                    "configured discovery model has no matching route"
                );
            }
        }
        self.warn_service_tier_withheld_for_flavor();
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

    /// Resolve `[server.gateway]` into the hot-reloadable JWT/approval snapshot.
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

    /// Whether to zstd-compress `provider`'s Responses **request** bodies
    /// (issue #285). Requires the provider's `request_compression` flag (on by
    /// default) and the ChatGPT/Codex flavor, mirroring the two gates codex
    /// itself applies (`responses_request_compression`: `is_openai()` — the
    /// ChatGPT backend — plus `uses_codex_backend()` — ChatGPT OAuth rather than
    /// an API key). The flag is inert on every other flavor: no stock
    /// OpenAI-compatible, xAI, or Grok upstream has been verified to accept a
    /// compressed request body, and one that rejects it fails the whole turn.
    pub fn responses_request_compression(&self, provider: &str) -> bool {
        self.provider(provider)
            .is_some_and(|config| config.request_compression)
            && self.responses_flavor(provider) == ResponsesFlavor::Chatgpt
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
    /// Requires the upstream flavor to be one known to accept it (stock
    /// OpenAI or the ChatGPT/Codex backend — xAI/Grok always keep the shim)
    /// and the model to advertise support (see [`model_supports_tool_search`]).
    /// Within that gate, `ProviderConfig::tool_search` decides the rest:
    /// `Some(explicit)` is honored as-is (an explicit `true` can force native
    /// even off a known-good host; an explicit `false` always forces the
    /// shim); the unset default (`None`, "auto") takes the native path only
    /// when the upstream is a host already verified to implement it — the
    /// ChatGPT/Codex backend (`AuthMode::ChatgptOauth`) or a `base_url` host
    /// of exactly `api.openai.com`. A user-declared provider on any other
    /// OpenAI-compatible host (LiteLLM, vLLM, OpenRouter, a self-hosted
    /// proxy, ...) therefore keeps the shim unless it opts in with
    /// `tool_search = true` (issue #289).
    pub fn native_tool_search(&self, provider: &str, model: &str) -> bool {
        let Some(config) = self.provider(provider) else {
            return false;
        };
        if !matches!(
            self.responses_flavor(provider),
            ResponsesFlavor::OpenAi | ResponsesFlavor::Chatgpt
        ) {
            return false;
        }
        if !model_supports_tool_search(model) {
            return false;
        }
        match config.tool_search {
            Some(explicit) => explicit,
            None => {
                config.auth == AuthMode::ChatgptOauth
                    || reqwest::Url::parse(&config.base_url)
                        .ok()
                        .and_then(|url| url.host_str().map(host_is_openai))
                        .unwrap_or(false)
            }
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

    /// Whether the bind address only accepts connections from this machine.
    ///
    /// An unparseable bind is treated as non-loopback: security gates built on
    /// this must fail closed, and `bind_addr` reports the parse error itself.
    pub fn bind_is_loopback(&self) -> bool {
        self.bind_addr()
            .map(|addr| addr.ip().is_loopback())
            .unwrap_or(false)
    }
}

/// Serializes every test that reads or writes the process environment against
/// every test that calls [`Config::load`]. The environment is process-global, so
/// a `SHUNT_*` var set inside one test's window is visible to a `Config::load`
/// running concurrently in another module: a leaked
/// `SHUNT_PROVIDERS__<NAME>__<FIELD>` makes figment synthesize a partial provider
/// table for a name the victim's config never declared, failing it with an
/// unrelated `MissingField("kind")`.
///
/// Acquisitions must tolerate poisoning (`unwrap_or_else(PoisonError::into_inner)`):
/// with ~11 tests sharing this guard, a panic in any one of them would
/// otherwise poison the mutex and cascade-fail every other holder with a
/// confusing `PoisonError` instead of the one real failure. The lock guards no
/// invariant that a panic could corrupt — it only orders env access — so
/// recovering the guard is correct.
#[cfg(test)]
pub(crate) static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use figment::providers::Format;

    use super::{
        config_file_candidates, default_auth_header, host_is_chatgpt, host_is_kimi,
        identity_collisions, AccountConfig, AdminConfig, AdminKey, AdminOidcConfig, AuthMode,
        CodexEndpointConfig, Config, ConfigError, ConfigFormat, GatewayConfig, GatewayOidcConfig,
        GatewayPolicyConfig, GatewayPolicyMatch, GatewaySessionConfig, GatewayTelemetryConfig,
        GatewayTelemetryDestination, InboundAuthConfig, ModelConfig, OauthUsageConfig,
        OidcProviderConfig, PoolConfig, ProviderConfig, ProviderKind, ResponsesFlavor, RetryConfig,
        Secret, SpendConfig, StatusConfig, StatusSource, UsageEndpointConfig, CONFIG_ENV_LOCK,
    };

    fn model_config(id: &str, upstream_model: Option<BTreeMap<String, String>>) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            display_name: None,
            upstream_model,
        }
    }

    fn model_upstream(provider: &str, upstream_model: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(provider.to_string(), upstream_model.to_string())])
    }

    type ValidationCase = (Config, fn(&ConfigError) -> bool);

    #[test]
    fn http_tuning_validation_rejects_each_invalid_value() {
        let cases: Vec<ValidationCase> = vec![
            (
                {
                    let mut config = Config::default();
                    config.server.access_control.allow_cidrs = vec!["not-a-cidr".into()];
                    config
                },
                |error| matches!(error, ConfigError::InvalidAccessControlCidr { .. }),
            ),
            (
                {
                    let mut config = Config::default();
                    config.server.limits.max_request_bytes = 0;
                    config
                },
                |error| matches!(error, ConfigError::InvalidMaxRequestBytes),
            ),
            (
                {
                    let mut config = Config::default();
                    config.server.limits.max_request_header_bytes = Some(0);
                    config
                },
                |error| matches!(error, ConfigError::InvalidMaxRequestHeaderBytes),
            ),
            (
                {
                    let mut config = Config::default();
                    config.server.limits.max_url_length = Some(0);
                    config
                },
                |error| matches!(error, ConfigError::InvalidMaxUrlLength),
            ),
            (
                {
                    let mut config = Config::default();
                    config.server.rate_limits.device_authorization.max = 0;
                    config
                },
                |error| {
                    matches!(
                        error,
                        ConfigError::InvalidRateLimitMax {
                            limit: "device_authorization"
                        }
                    )
                },
            ),
            (
                {
                    let mut config = Config::default();
                    config.server.rate_limits.device_verify.window_seconds = 0;
                    config
                },
                |error| {
                    matches!(
                        error,
                        ConfigError::InvalidRateLimitWindow {
                            limit: "device_verify"
                        }
                    )
                },
            ),
        ];
        for (config, matches_error) in cases {
            let error = config.validate().unwrap_err();
            assert!(matches_error(&error), "unexpected error: {error}");
        }
    }

    #[test]
    fn server_max_concurrent_requests_parses_and_defaults() {
        let absent: super::ServerConfig =
            serde_json::from_str(r#"{"bind":"127.0.0.1:3001","default_provider":"anthropic"}"#)
                .unwrap();
        assert_eq!(absent.max_concurrent_requests, 1024);

        for configured in [0, 37] {
            let server: super::ServerConfig = serde_json::from_value(serde_json::json!({
                "bind": "127.0.0.1:3001",
                "default_provider": "anthropic",
                "max_concurrent_requests": configured,
            }))
            .unwrap();
            assert_eq!(server.max_concurrent_requests, configured);
        }
    }

    #[test]
    fn validate_rejects_unsandboxed_antigravity_on_a_public_bind() {
        use crate::config::ProviderConfig;

        let antigravity = |sandbox: bool| {
            let mut provider = ProviderConfig::gemini("http://localhost");
            provider.kind = ProviderKind::AntigravityCli;
            provider.auth = AuthMode::None;
            provider.sandbox = sandbox;
            provider
        };

        // Loopback: a personal integration, allowed even unsandboxed.
        let mut config = Config::default();
        config.server.bind = "127.0.0.1:3001".to_string();
        config
            .providers
            .insert("antigravity".to_string(), antigravity(false));
        config
            .validate()
            .expect("an unsandboxed provider on loopback stays allowed");

        // Reachable from the network, it hands local shell access to anyone
        // who can post a Messages request.
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:3001".to_string();
        config
            .providers
            .insert("antigravity".to_string(), antigravity(false));
        let error = config
            .validate()
            .expect_err("sandbox = false must not be servable off-loopback");
        assert!(
            matches!(
                error,
                ConfigError::UnsandboxedAntigravityOnPublicBind { .. }
            ),
            "unexpected error: {error}"
        );

        // With the sandbox on, the same bind is fine.
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:3001".to_string();
        config
            .providers
            .insert("antigravity".to_string(), antigravity(true));
        config
            .validate()
            .expect("a sandboxed provider is servable off-loopback");
    }

    #[test]
    fn bind_is_loopback_fails_closed_on_an_unparseable_bind() {
        let mut config = Config::default();
        config.server.bind = "not-an-address".to_string();
        assert!(
            !config.server.bind_is_loopback(),
            "an unparseable bind must not be treated as loopback by a security gate"
        );
    }

    #[test]
    fn validate_rejects_max_concurrent_requests_above_semaphore_limit() {
        use super::MAX_CONCURRENT_REQUESTS_LIMIT;

        // The limit must track tokio's own bound: `Semaphore::new` asserts on
        // it, so a value validate() accepts must never panic the constructor.
        assert_eq!(
            MAX_CONCURRENT_REQUESTS_LIMIT,
            tokio::sync::Semaphore::MAX_PERMITS,
            "validation bound drifted from tokio::sync::Semaphore::MAX_PERMITS"
        );

        let mut config = Config::default();
        config.server.max_concurrent_requests = MAX_CONCURRENT_REQUESTS_LIMIT + 1;
        let error = config
            .validate()
            .expect_err("an out-of-range limit must be rejected at config time");
        assert!(
            matches!(
                error,
                ConfigError::InvalidMaxConcurrentRequests { limit, .. }
                    if limit == MAX_CONCURRENT_REQUESTS_LIMIT
            ),
            "unexpected error: {error}"
        );

        // The boundary itself is accepted, and constructing the semaphore at
        // that value does not panic — which is the behavior the guard protects.
        let mut config = Config::default();
        config.server.max_concurrent_requests = MAX_CONCURRENT_REQUESTS_LIMIT;
        config
            .validate()
            .expect("the boundary value itself is valid");
        let _ = tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS_LIMIT);
    }

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
    fn status_config_refresh_interval_disables_and_clamps() {
        use super::StatusConfig;
        // `0` disables polling.
        assert_eq!(
            StatusConfig {
                refresh_seconds: 0,
                ..Default::default()
            }
            .refresh_interval(),
            None
        );
        // A positive value below the 60s floor is clamped up; at/above passes through.
        assert_eq!(
            StatusConfig {
                refresh_seconds: 30,
                ..Default::default()
            }
            .refresh_interval(),
            Some(60)
        );
        assert_eq!(
            StatusConfig {
                refresh_seconds: 300,
                ..Default::default()
            }
            .refresh_interval(),
            Some(300)
        );
        // The default itself (300s) is unaffected by the floor.
        assert_eq!(StatusConfig::default().refresh_interval(), Some(300));
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
        assert!(admin.oidc.is_none());
    }

    #[test]
    fn admin_oidc_config_resolves_defaults_and_fails_closed() {
        let suffix = std::process::id();
        let tokens_env = format!("SHUNT_ADMIN_OIDC_CONFIG_TOKENS_{suffix}");
        let secret_env = format!("SHUNT_ADMIN_OIDC_CONFIG_SECRET_{suffix}");
        std::env::set_var(&tokens_env, "ops:admin-token");
        let mut admin = AdminConfig {
            header: "x-shunt-admin-token".into(),
            tokens_env: tokens_env.clone(),
            tokens_file: None,
            write_keys: Vec::new(),
            read_keys: Vec::new(),
            session_ttl_secs: 3600,
            pending_ttl_secs: 600,
            oidc: Some(AdminOidcConfig {
                public_url: "http://127.0.0.1:8787".into(),
                client_secret_env: secret_env.clone(),
                provider: OidcProviderConfig {
                    issuer: "https://accounts.example.com/dex".into(),
                    client_id: "client-id".into(),
                    allowed_domains: vec![" Example.COM ".into()],
                    allowed_emails: vec![],
                    scopes: vec![],
                    authorization_endpoint: None,
                    token_endpoint: None,
                    userinfo_endpoint: None,
                },
            }),
        };

        assert!(matches!(
            admin.resolve(),
            Err(ConfigError::MissingAdminOidcSecret { .. })
        ));
        std::env::set_var(&secret_env, "client-secret");
        let auth = admin.resolve().expect("valid admin OIDC config resolves");
        let idp = auth.oidc().expect("OIDC is attached");
        assert_eq!(idp.scopes, ["openid", "email", "profile"]);
        assert!(idp.email_allowed("developer@example.com"));
        assert_eq!(
            auth.oidc_callback_url().as_deref(),
            Some("http://127.0.0.1:8787/admin/oidc/callback")
        );

        admin
            .oidc
            .as_mut()
            .unwrap()
            .provider
            .allowed_domains
            .clear();
        assert!(matches!(
            admin.resolve(),
            Err(ConfigError::MissingAdminOidcAllowlist)
        ));
        {
            let oidc = admin.oidc.as_mut().unwrap();
            oidc.provider
                .allowed_emails
                .push("developer@example.com".into());
            oidc.public_url = "https://admin.example/path".into();
        }
        assert!(matches!(
            admin.resolve(),
            Err(ConfigError::InvalidAdminOidc { .. })
        ));
        admin.oidc.as_mut().unwrap().public_url = "http://admin.example".into();
        assert!(matches!(
            admin.resolve(),
            Err(ConfigError::InvalidAdminOidc { .. })
        ));
        {
            let oidc = admin.oidc.as_mut().unwrap();
            oidc.public_url = "https://admin.example".into();
            oidc.provider.issuer.clear();
        }
        assert!(matches!(
            admin.resolve(),
            Err(ConfigError::InvalidAdminOidc { .. })
        ));
        {
            let oidc = admin.oidc.as_mut().unwrap();
            oidc.provider.issuer = "https://accounts.example.com".into();
            oidc.provider.client_id.clear();
        }
        assert!(matches!(
            admin.resolve(),
            Err(ConfigError::InvalidAdminOidc { .. })
        ));

        std::env::remove_var(tokens_env);
        std::env::remove_var(secret_env);
    }

    #[test]
    fn admin_oidc_deserialization_defaults_secret_env_and_scopes() {
        let oidc: AdminOidcConfig = serde_json::from_str(
            r#"{
                "public_url":"https://admin.example",
                "issuer":"https://accounts.example.com",
                "client_id":"client-id",
                "allowed_emails":["developer@example.com"]
            }"#,
        )
        .unwrap();
        assert_eq!(oidc.client_secret_env, "SHUNT_ADMIN_OIDC_SECRET");
        assert!(
            oidc.provider.scopes.is_empty(),
            "empty config scopes resolve to defaults"
        );
    }

    #[test]
    fn admin_config_resolve_succeeds_and_fails_closed() {
        let base = AdminConfig {
            header: "x-shunt-admin-token".to_string(),
            tokens_env: "SHUNT_TEST_ADMIN_RESOLVE".to_string(),
            tokens_file: None,
            write_keys: Vec::new(),
            read_keys: Vec::new(),
            session_ttl_secs: 1800,
            pending_ttl_secs: 300,
            oidc: None,
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

    #[test]
    fn admin_config_resolve_falls_back_to_tokens_file() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-admin-tokens-file-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let token_path = dir.join("admin-token");
        // A one-pair-per-line file, as `shunt dashboard setup` writes it.
        std::fs::write(&token_path, "admin:file-secret\n").unwrap();

        let env_name = "SHUNT_TEST_ADMIN_FILE_FALLBACK";
        std::env::remove_var(env_name);
        let base = AdminConfig {
            header: "x-shunt-admin-token".to_string(),
            tokens_env: env_name.to_string(),
            tokens_file: Some(token_path.to_string_lossy().into_owned()),
            write_keys: Vec::new(),
            read_keys: Vec::new(),
            session_ttl_secs: 1800,
            pending_ttl_secs: 300,
            oidc: None,
        };

        // Env unset ⇒ the file is read.
        base.resolve()
            .expect("token file resolves when env is unset");

        // Env set ⇒ it wins over the file.
        std::env::set_var(env_name, "ops:env-secret");
        base.resolve()
            .expect("env tokens resolve and take precedence");
        std::env::remove_var(env_name);

        // A configured-but-unreadable file is a startup error, not open access.
        let missing = AdminConfig {
            tokens_file: Some(dir.join("does-not-exist").to_string_lossy().into_owned()),
            ..base.clone()
        };
        assert!(matches!(
            missing.resolve(),
            Err(ConfigError::UnreadableAdminTokensFile { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
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

    fn capture_logs<F, T>(operation: F) -> (T, String)
    where
        F: FnOnce() -> T,
    {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let result = tracing::subscriber::with_default(subscriber, operation);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        (result, logs)
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

    fn kimi_oauth_config() -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "kimi".to_string(),
            ProviderConfig::anthropic("https://api.kimi.com"),
        );
        config.providers.get_mut("kimi").unwrap().auth = AuthMode::KimiOauth;
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
    fn kimi_oauth_requires_anthropic_kind() {
        let mut config = kimi_oauth_config();
        config.providers.get_mut("kimi").unwrap().kind = ProviderKind::Responses;
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::KimiOauthWrongKind { .. }
        ));
    }

    #[test]
    fn kimi_oauth_accepts_plaintext_loopback_base_urls() {
        for base_url in ["http://127.0.0.1:8080", "http://localhost:9000"] {
            let mut config = kimi_oauth_config();
            config.providers.get_mut("kimi").unwrap().base_url = base_url.to_string();
            config.validate().unwrap();
        }
    }

    #[test]
    fn kimi_oauth_rejects_plaintext_remote_base_url() {
        let mut config = kimi_oauth_config();
        config.providers.get_mut("kimi").unwrap().base_url = "http://api.kimi.com".to_string();
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::KimiOauthNotHttps { .. }
        ));
    }

    #[test]
    fn kimi_oauth_rejects_remote_non_kimi_base_url() {
        let mut config = kimi_oauth_config();
        config.providers.get_mut("kimi").unwrap().base_url = "https://evil.example.com".to_string();
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::KimiOauthNonKimiHost { .. }
        ));
    }

    #[test]
    fn kimi_oauth_accepts_kimi_https_base_url() {
        let mut config = kimi_oauth_config();
        config.providers.get_mut("kimi").unwrap().base_url = "https://api.kimi.com".to_string();
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
    fn service_tier_fast_alias_normalizes_to_priority() {
        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().service_tier = Some("fast".to_string());
        let config = config.validate().unwrap();
        assert_eq!(
            config
                .providers
                .get("anthropic")
                .unwrap()
                .service_tier
                .as_deref(),
            Some("priority")
        );
    }

    #[test]
    fn service_tier_default_sentinel_is_preserved() {
        // "default" is a client-only sentinel that must never reach the wire,
        // but validation preserves it as Some("default") rather than clearing
        // it to None -- collapsing the two made an explicit route-level
        // "default" indistinguishable from an unset route, so it silently
        // inherited the provider-level tier instead of overriding it
        // (issue #301). The wire-emission site strips the sentinel instead.
        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().service_tier = Some("default".to_string());
        let config = config.validate().unwrap();
        assert_eq!(
            config
                .providers
                .get("anthropic")
                .unwrap()
                .service_tier
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn service_tier_rejects_invalid_provider_value() {
        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().service_tier = Some("turbo".to_string());
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidProviderServiceTier { .. }
        ));
    }

    #[test]
    fn service_tier_rejects_invalid_route_value() {
        let mut config = Config::default();
        config.routes.push(super::RouteConfig {
            model: "gpt-special".to_string(),
            provider: "anthropic".to_string(),
            upstream_model: None,
            effort: None,
            service_tier: Some("turbo".to_string()),
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidRouteServiceTier { .. }
        ));
    }

    #[test]
    fn service_tier_flex_and_priority_pass_through_unchanged() {
        // "flex" and literal "priority" are already wire values -- the
        // normalize match arms that pass them through as-is (as opposed to
        // the "fast" -> "priority" alias and the "default" sentinel) were
        // previously unasserted.
        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().service_tier = Some("flex".to_string());
        let config = config.validate().unwrap();
        assert_eq!(
            config
                .providers
                .get("anthropic")
                .unwrap()
                .service_tier
                .as_deref(),
            Some("flex")
        );

        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().service_tier = Some("priority".to_string());
        let config = config.validate().unwrap();
        assert_eq!(
            config
                .providers
                .get("anthropic")
                .unwrap()
                .service_tier
                .as_deref(),
            Some("priority")
        );
    }

    #[test]
    fn service_tier_route_level_fast_alias_normalizes_to_priority() {
        // Mirrors service_tier_fast_alias_normalizes_to_priority, but for the
        // route-level field rather than the provider-level one -- the two are
        // normalized by separate loops in normalize_service_tiers.
        let mut config = Config::default();
        config.routes.push(super::RouteConfig {
            model: "gpt-special".to_string(),
            provider: "anthropic".to_string(),
            upstream_model: None,
            effort: None,
            service_tier: Some("fast".to_string()),
        });
        let config = config.validate().unwrap();
        assert_eq!(config.routes[0].service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn service_tier_rejects_mixed_case_value() {
        // Case-sensitivity pin: normalize_service_tier_value matches exact
        // lowercase literals only, so "Fast" must be rejected rather than
        // silently treated as the "fast" alias.
        let mut config = Config::default();
        config.providers.get_mut("anthropic").unwrap().service_tier = Some("Fast".to_string());
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidProviderServiceTier { .. }
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
    fn reprobe_floor_warning_is_load_only_and_disabled_values_are_silent() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-reprobe-floor-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let low_path = dir.join("low.toml");
        std::fs::write(&low_path, "[server.pool]\nreprobe_seconds = 1\n").unwrap();
        let (loaded, load_logs) = capture_logs(|| Config::load(Some(&low_path)));
        let config = loaded.expect("a positive below-floor reprobe value loads");
        assert_eq!(
            load_logs
                .matches("reprobe_seconds is below the floor")
                .count(),
            1,
            "the successful load emits one floor warning: {load_logs}"
        );

        let (_, validate_logs) = capture_logs(|| {
            config
                .clone()
                .validate()
                .expect("first validation succeeds");
            config.validate().expect("second validation succeeds");
        });
        assert_eq!(
            validate_logs
                .matches("reprobe_seconds is below the floor")
                .count(),
            0,
            "runtime validation must not repeat the load warning: {validate_logs}"
        );

        let zero_path = dir.join("zero.toml");
        std::fs::write(&zero_path, "[server.pool]\nreprobe_seconds = 0\n").unwrap();
        let (_, zero_logs) = capture_logs(|| Config::load(Some(&zero_path)));
        assert_eq!(
            zero_logs
                .matches("reprobe_seconds is below the floor")
                .count(),
            0,
            "zero disables reprobes and must not warn: {zero_logs}"
        );

        let absent_path = dir.join("absent.toml");
        std::fs::write(&absent_path, "").unwrap();
        let (_, absent_logs) = capture_logs(|| Config::load(Some(&absent_path)));
        assert_eq!(
            absent_logs
                .matches("reprobe_seconds is below the floor")
                .count(),
            0,
            "an absent pool must not warn: {absent_logs}"
        );

        let _ = std::fs::remove_dir_all(dir);
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
    fn validate_rejects_invalid_status_sources() {
        // An empty provider label is rejected.
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: vec![StatusSource {
                provider: String::new(),
                url: "https://status.claude.com/api/v2/summary.json".to_string(),
            }],
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidStatusSourceProvider { index: 0 }
        ));

        // An unparseable URL is rejected.
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: vec![StatusSource {
                provider: "claude".to_string(),
                url: "not-a-url".to_string(),
            }],
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidStatusSourceUrl { index: 0, .. }
        ));

        // A non-http(s) scheme is rejected even though it parses fine.
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: vec![StatusSource {
                provider: "claude".to_string(),
                url: "ftp://status.claude.com/summary.json".to_string(),
            }],
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidStatusSourceUrl { index: 0, .. }
        ));

        // Query strings, fragments, and embedded credentials are rejected.
        for url in [
            "https://status.claude.com/api/v2/summary.json?token=secret",
            "https://status.claude.com/api/v2/summary.json#status",
            "https://user:secret@status.claude.com/api/v2/summary.json",
        ] {
            let mut config = Config::default();
            config.server.status = Some(StatusConfig {
                refresh_seconds: 300,
                sources: vec![StatusSource {
                    provider: "claude".to_string(),
                    url: url.to_string(),
                }],
            });
            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::InvalidStatusSourceUrl { index: 0, .. }
            ));
        }

        // Duplicate provider labels are rejected.
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: vec![
                StatusSource {
                    provider: "claude".to_string(),
                    url: "https://status.claude.com/api/v2/summary.json".to_string(),
                },
                StatusSource {
                    provider: "claude".to_string(),
                    url: "https://status.openai.com/api/v2/summary.json".to_string(),
                },
            ],
        });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateStatusSourceProvider { provider } if provider == "claude"
        ));

        // Distinct, well-formed http(s) sources pass validation.
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: vec![
                StatusSource {
                    provider: "claude".to_string(),
                    url: "https://status.claude.com/api/v2/summary.json".to_string(),
                },
                StatusSource {
                    provider: "openai".to_string(),
                    url: "https://status.openai.com/api/v2/summary.json".to_string(),
                },
            ],
        });
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
            identity_collisions("codex", &[first.clone(), second.clone(), unique, solo]),
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
    fn identity_collisions_does_not_flag_explicit_uuid_against_a_name_fallback() {
        // "first" has no uuid, so its pool identity is name-based
        // (`AccountStateIdentity::UpstreamInline`). A second account whose
        // *explicit* uuid is literally "first" resolves to
        // `AccountStateIdentity::Verified` — a different `AccountKey` variant, so
        // the pool keeps them as two separate accounts. The collision warning must
        // therefore NOT report them as sharing a slot, even though their display
        // identity strings match.
        let first = account("first");
        let mut second = account("second");
        second.uuid = Some("first".to_string());
        let unrelated = account("unrelated");

        assert!(
            identity_collisions("codex", &[first.clone(), second.clone(), unrelated]).is_empty(),
            "a Verified uuid and a name fallback are distinct pool identities"
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
            jwt_secret_env: Some(secret_env.clone()),
            users_env: users_env.clone(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: None,
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

    /// Build an `[server.admin]` with the given key arrays and a tokens env var
    /// name the caller controls.
    fn admin_config_with_keys(
        tokens_env: &str,
        write_keys: Vec<AdminKey>,
        read_keys: Vec<AdminKey>,
    ) -> AdminConfig {
        AdminConfig {
            header: "x-shunt-admin-token".to_string(),
            tokens_env: tokens_env.to_string(),
            tokens_file: None,
            write_keys,
            read_keys,
            session_ttl_secs: 3600,
            pending_ttl_secs: 600,
            oidc: None,
        }
    }

    fn admin_key(id: &str, key: &str) -> AdminKey {
        AdminKey {
            id: id.to_string(),
            key: Secret::from(key),
        }
    }

    const ADMIN_KEY_A: &str = "0123456789abcdef0123456789abcdef";
    const ADMIN_KEY_B: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn admin_key_arrays_reject_short_keys_blank_ids_and_cross_set_duplicates() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tokens_env = format!("SHUNT_TEST_ADMIN_KEYS_{}", std::process::id());
        std::env::set_var(&tokens_env, "ops:ops-token-0123456789abcdef01234567");
        let mut config = Config::default();

        // A key shorter than 32 characters is rejected on the arrays (the
        // legacy `tokens_env` tier only warns — see the test below).
        config.server.admin = Some(admin_config_with_keys(
            &tokens_env,
            vec![admin_key("terraform", "short")],
            Vec::new(),
        ));
        assert!(matches!(
            config.clone().validate(),
            Err(ConfigError::ShortAdminKey {
                field: "write_keys",
                ..
            })
        ));

        // Every array entry needs a name to attribute audit records to.
        config.server.admin = Some(admin_config_with_keys(
            &tokens_env,
            Vec::new(),
            vec![admin_key("  ", ADMIN_KEY_A)],
        ));
        assert!(matches!(
            config.clone().validate(),
            Err(ConfigError::BlankAdminKeyId {
                field: "read_keys",
                index: 0
            })
        ));

        // Ids must be unique across all three sets, including the env tokens.
        config.server.admin = Some(admin_config_with_keys(
            &tokens_env,
            vec![admin_key("ops", ADMIN_KEY_A)],
            Vec::new(),
        ));
        assert!(matches!(
            config.clone().validate(),
            Err(ConfigError::DuplicateAdminKeyId { ref id }) if id == "ops"
        ));

        // So must values: the same key in two tiers makes privilege ambiguous.
        config.server.admin = Some(admin_config_with_keys(
            &tokens_env,
            vec![admin_key("terraform", ADMIN_KEY_A)],
            vec![admin_key("reporting", ADMIN_KEY_A)],
        ));
        let error = config
            .clone()
            .validate()
            .expect_err("duplicate key values must fail validation");
        assert!(matches!(
            error,
            ConfigError::DuplicateAdminKeyValue {
                ref first_id,
                ref second_id,
            } if first_id == "terraform" && second_id == "reporting"
        ));
        assert!(!error.to_string().contains(ADMIN_KEY_A));

        config.server.admin = Some(admin_config_with_keys(
            &tokens_env,
            vec![admin_key("terraform", ADMIN_KEY_A)],
            vec![admin_key("reporting", ADMIN_KEY_B)],
        ));
        config
            .validate()
            .expect("distinct ids and values across all three sets validate");
        std::env::remove_var(&tokens_env);
    }

    #[test]
    fn admin_resolve_needs_only_the_key_arrays_and_fails_when_every_source_is_empty() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tokens_env = format!("SHUNT_TEST_ADMIN_ARRAY_ONLY_{}", std::process::id());
        std::env::remove_var(&tokens_env);

        // Array-only: `tokens_env` is unset and the surface still boots.
        admin_config_with_keys(
            &tokens_env,
            vec![admin_key("terraform", ADMIN_KEY_A)],
            vec![admin_key("reporting", ADMIN_KEY_B)],
        )
        .resolve()
        .expect("write_keys/read_keys alone must resolve");
        admin_config_with_keys(
            &tokens_env,
            Vec::new(),
            vec![admin_key("reporting", ADMIN_KEY_B)],
        )
        .resolve()
        .expect("read_keys alone must resolve");

        // All three sources empty is still fail-closed: a present
        // `[server.admin]` never yields an open surface.
        assert!(matches!(
            admin_config_with_keys(&tokens_env, Vec::new(), Vec::new()).resolve(),
            Err(ConfigError::MissingAdminTokens { .. })
        ));
    }

    #[test]
    fn short_legacy_admin_token_warns_but_still_resolves() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tokens_env = format!("SHUNT_TEST_ADMIN_SHORT_TOKEN_{}", std::process::id());
        std::env::set_var(&tokens_env, "ops:short");
        let admin = admin_config_with_keys(&tokens_env, Vec::new(), Vec::new());

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            admin
                .resolve()
                .expect("a short legacy token must not fail startup");
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("shorter than 32 characters"), "{logs}");
        assert!(logs.contains("ops"), "{logs}");
        assert!(
            !logs.contains("short\""),
            "the token value must not be logged: {logs}"
        );
        std::env::remove_var(&tokens_env);
    }

    #[test]
    fn spend_section_requires_the_admin_section() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut config = Config::default();
        config.server.spend = Some(SpendConfig::default());
        assert!(matches!(
            config.clone().validate(),
            Err(ConfigError::SpendRequiresAdmin)
        ));

        let tokens_env = format!("SHUNT_TEST_SPEND_NEEDS_ADMIN_{}", std::process::id());
        std::env::remove_var(&tokens_env);
        config.server.admin = Some(admin_config_with_keys(
            &tokens_env,
            vec![admin_key("terraform", ADMIN_KEY_A)],
            Vec::new(),
        ));
        config
            .validate()
            .expect("[server.spend] with [server.admin] validates");
    }

    /// `Config` derives `Debug`, and shunt's convention elsewhere is to keep only
    /// env var *names* in it. The admin key arrays hold resolved key material,
    /// so assert the redaction directly: without `Secret`, a `{:?}` of the
    /// config would leak admin keys into any log line or panic message.
    #[test]
    fn debug_formatting_redacts_admin_array_keys_but_keeps_ids() {
        let admin = admin_config_with_keys(
            "SHUNT_TEST_ADMIN_DEBUG",
            vec![admin_key("terraform", ADMIN_KEY_A)],
            vec![admin_key("reporting", ADMIN_KEY_B)],
        );

        let rendered = format!("{admin:?}");
        assert!(
            !rendered.contains(ADMIN_KEY_A) && !rendered.contains(ADMIN_KEY_B),
            "resolved key material leaked into Debug output: {rendered}"
        );
        // The ids must survive — they are what the audit trail attributes to.
        assert!(rendered.contains("terraform"), "{rendered}");
        assert!(rendered.contains("reporting"), "{rendered}");

        // And the same must hold when it is nested inside a whole `Config`.
        let mut config = Config::default();
        config.server.admin = Some(admin);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(ADMIN_KEY_A) && !rendered.contains(ADMIN_KEY_B));
    }

    /// `Config::load` seeds figment with `Serialized::defaults(Config::default())`,
    /// and `Secret` serializes as the literal `[redacted]`. `[server.admin]` now
    /// carries `Secret` key material, so it must stay behind an `Option` that is
    /// skipped when `None` — hoisting it would make figment re-extract
    /// `[redacted]` as the operator's real key. Pins both halves: the default
    /// omits the section entirely, and the section really does render
    /// `[redacted]` when present.
    #[test]
    fn admin_section_is_omitted_from_serialized_defaults_so_secrets_never_round_trip() {
        let defaults = serde_json::to_value(Config::default()).expect("serialize defaults");
        assert!(
            defaults["server"].get("admin").is_none(),
            "[server.admin] must not be serialized into the figment defaults: {defaults}"
        );

        let mut config = Config::default();
        config.server.admin = Some(admin_config_with_keys(
            "SHUNT_TEST_ADMIN_ROUNDTRIP",
            vec![admin_key("terraform", ADMIN_KEY_A)],
            Vec::new(),
        ));
        let serialized = serde_json::to_value(&config).expect("serialize config");
        assert_eq!(
            serialized["server"]["admin"]["write_keys"][0]["key"],
            serde_json::json!("[redacted]"),
            "this is exactly what would be re-extracted as the real key if the \
             section were unconditionally present"
        );
    }

    #[test]
    fn gateway_config_rejects_invalid_public_url_and_zero_ttl() {
        let mut gateway = GatewayConfig {
            public_url: "not a URL".to_string(),
            jwt_secret_env: Some("UNUSED_GATEWAY_SECRET".to_string()),
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: None,
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
        gateway.token_ttl_seconds = Some(0);
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewayTokenTtl)
        ));
    }

    #[test]
    fn gateway_oidc_config_fails_closed() {
        let suffix = std::process::id();
        let secret_env = format!("SHUNT_GATEWAY_OIDC_CONFIG_SECRET_{suffix}");
        let mut oidc = GatewayOidcConfig {
            client_secret_env: secret_env.clone(),
            provider: OidcProviderConfig {
                issuer: "https://accounts.example.com/dex".into(),
                client_id: "client-id".into(),
                allowed_domains: vec![],
                allowed_emails: vec![],
                scopes: vec![],
                authorization_endpoint: None,
                token_endpoint: None,
                userinfo_endpoint: None,
            },
        };
        assert!(matches!(
            oidc.resolve(),
            Err(ConfigError::MissingGatewayOidcSecret { .. })
        ));
        std::env::set_var(&secret_env, "client-secret");
        assert!(matches!(
            oidc.resolve(),
            Err(ConfigError::MissingGatewayOidcAllowlist)
        ));
        oidc.provider.allowed_domains.push("example.com".into());
        oidc.provider.issuer.clear();
        assert!(matches!(
            oidc.resolve(),
            Err(ConfigError::InvalidGatewayOidc { .. })
        ));
        oidc.provider.issuer = "https://accounts.example.com/dex?tenant=x".into();
        assert!(matches!(
            oidc.resolve(),
            Err(ConfigError::InvalidGatewayOidc { .. })
        ));
        oidc.provider.issuer = "https://accounts.example.com/dex/".into();
        assert_eq!(
            oidc.resolve().unwrap().issuer,
            "https://accounts.example.com/dex/"
        );
        oidc.provider.issuer = "https://accounts.example.com/dex".into();
        oidc.provider.scopes = vec!["openid".into(), "profile".into()];
        assert!(matches!(
            oidc.resolve(),
            Err(ConfigError::InvalidGatewayOidc { .. })
        ));
        oidc.provider.scopes.push("email".into());
        oidc.provider.authorization_endpoint = Some("http://idp.example/authorize".into());
        assert!(matches!(
            oidc.resolve(),
            Err(ConfigError::InvalidGatewayOidc { .. })
        ));
        oidc.provider.authorization_endpoint = Some("http://127.0.0.1:8787/authorize".into());
        assert!(oidc.resolve().is_ok());
        std::env::remove_var(secret_env);
    }

    #[test]
    fn gateway_oidc_requires_issuer_when_deserializing() {
        let missing = serde_json::from_str::<GatewayOidcConfig>(r#"{"client_id":"client-id"}"#);
        assert!(missing.is_err());
    }

    #[test]
    fn gateway_oidc_makes_static_users_optional() {
        let suffix = std::process::id();
        let jwt_env = format!("SHUNT_GATEWAY_OPTIONAL_USERS_JWT_{suffix}");
        let users_env = format!("SHUNT_GATEWAY_OPTIONAL_USERS_LIST_{suffix}");
        let oidc_env = format!("SHUNT_GATEWAY_OPTIONAL_USERS_OIDC_{suffix}");
        std::env::set_var(&jwt_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&oidc_env, "client-secret");
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".into(),
            jwt_secret_env: Some(jwt_env.clone()),
            users_env: users_env.clone(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: Some(GatewayOidcConfig {
                client_secret_env: oidc_env.clone(),
                provider: OidcProviderConfig {
                    issuer: "https://accounts.example.com".into(),
                    client_id: "client-id".into(),
                    allowed_domains: vec!["example.com".into()],
                    allowed_emails: vec![],
                    scopes: vec![],
                    authorization_endpoint: None,
                    token_endpoint: None,
                    userinfo_endpoint: None,
                },
            }),
            session: None,
        };
        let resolved = gateway.resolve().expect("OIDC-only gateway resolves");
        assert!(resolved.approval_provider().is_none());
        assert!(resolved.oidc().is_some());
        std::env::remove_var(jwt_env);
        std::env::remove_var(users_env);
        std::env::remove_var(oidc_env);
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
            jwt_secret_env: Some(secret_env.clone()),
            users_env: users_env.clone(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: None,
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
                metrics: true,
                logs: false,
                traces: false,
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
    fn gateway_session_jwt_secret_conflicts_with_legacy_jwt_secret_env() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: Some("UNUSED_GATEWAY_SECRET".to_string()),
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![Secret::from("0123456789abcdef0123456789abcdef")],
                ttl_hours: None,
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::GatewaySessionJwtSecretConflict)
        ));
    }

    #[test]
    fn gateway_session_ttl_hours_conflicts_with_legacy_token_ttl_seconds() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![Secret::from("0123456789abcdef0123456789abcdef")],
                ttl_hours: Some(2),
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::GatewaySessionTtlConflict)
        ));
    }

    #[test]
    fn gateway_session_jwt_secret_scalar_and_array_wire_forms_both_resolve() {
        // Proves the `session.jwt_secret` string-or-array wire format survives
        // the full figment extraction -> resolve() pipeline, not just the
        // GatewaySessionConfig-level deserialize tests in config/session.rs.
        let suffix = std::process::id();
        let users_env = format!("SHUNT_GATEWAY_SESSION_WIRE_USERS_{suffix}");
        std::env::set_var(&users_env, "dev@example.com:password");

        let scalar_toml = format!(
            "public_url = \"https://gateway.example\"\n\
             users_env = \"{users_env}\"\n\
             [session]\n\
             jwt_secret = \"0123456789abcdef0123456789abcdef\"\n"
        );
        let scalar: GatewayConfig =
            figment::Figment::from(figment::providers::Toml::string(&scalar_toml))
                .extract()
                .unwrap();
        let resolved = scalar.resolve().expect("scalar jwt_secret resolves");
        // Absent ttl_hours falls back to the historical 3600s default.
        assert_eq!(resolved.token_ttl_seconds(), 3600);

        let array_toml = format!(
            "public_url = \"https://gateway.example\"\n\
             users_env = \"{users_env}\"\n\
             [session]\n\
             jwt_secret = [\"0123456789abcdef0123456789abcdef\", \"fedcba9876543210fedcba9876543210\"]\n\
             ttl_hours = 2\n"
        );
        let array: GatewayConfig =
            figment::Figment::from(figment::providers::Toml::string(&array_toml))
                .extract()
                .unwrap();
        let resolved = array.resolve().expect("array jwt_secret resolves");
        assert_eq!(resolved.token_ttl_seconds(), 7200);

        std::env::remove_var(users_env);
    }

    #[test]
    fn gateway_session_jwt_secret_rejects_a_short_scalar_secret() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![Secret::from("too-short")],
                ttl_hours: None,
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewaySessionJwtSecret { index: 0 })
        ));
    }

    #[test]
    fn gateway_session_jwt_secret_names_the_offending_index_past_zero() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![
                    Secret::from("0123456789abcdef0123456789abcdef"),
                    Secret::from("too-short"),
                ],
                ttl_hours: None,
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewaySessionJwtSecret { index: 1 })
        ));
    }

    #[test]
    fn gateway_session_jwt_secret_rejects_an_empty_array() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![],
                ttl_hours: None,
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::EmptyGatewaySessionJwtSecret)
        ));
    }

    #[test]
    fn gateway_session_ttl_hours_zero_is_rejected() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![Secret::from("0123456789abcdef0123456789abcdef")],
                ttl_hours: Some(0),
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::InvalidGatewaySessionTtlHours)
        ));
    }

    #[test]
    fn gateway_session_ttl_hours_overflow_is_rejected_not_panicked() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![Secret::from("0123456789abcdef0123456789abcdef")],
                ttl_hours: Some(u64::MAX),
            }),
        };
        assert!(matches!(
            gateway.resolve(),
            Err(ConfigError::GatewaySessionTtlHoursOverflow)
        ));
    }

    #[test]
    fn gateway_deprecations_is_empty_when_legacy_keys_are_unset() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: None,
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: None,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: Some(GatewaySessionConfig {
                jwt_secret: vec![Secret::from("0123456789abcdef0123456789abcdef")],
                ttl_hours: Some(2),
            }),
        };
        assert!(gateway.deprecations().is_empty());
    }

    #[test]
    fn gateway_deprecations_reports_both_legacy_keys_when_set() {
        let gateway = GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: Some("SHUNT_GATEWAY_JWT_SECRET".to_string()),
            users_env: "UNUSED_GATEWAY_USERS".to_string(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            oidc: None,
            session: None,
        };
        let messages = gateway.deprecations();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("jwt_secret_env"));
        assert!(messages[1].contains("token_ttl_seconds"));
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
    fn oauth_usage_endpoint_alone_on_loopback_bind_validates_without_auth() {
        // Loopback is the safe, ungated default deployment — no
        // `[server.auth]`/`[server.gateway]` required.
        let mut config = Config::default();
        config.server.bind = "127.0.0.1:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        config.validate().unwrap();
    }

    #[test]
    fn oauth_usage_endpoint_on_non_loopback_bind_requires_auth_or_gateway() {
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback
        ));
    }

    #[test]
    fn claude_oauth_provider_pointing_at_this_gateways_own_bind_is_rejected() {
        // A `claude_oauth` provider's `base_url` resolving to this gateway's
        // own loopback bind port, with `[server.oauth_usage]` enabled, would
        // make the outbound usage poller read back its own synthesized
        // aggregate instead of Anthropic's real usage.
        let mut config = Config::default();
        config.server.bind = "127.0.0.1:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        let anthropic = config.providers.get_mut("anthropic").unwrap();
        anthropic.auth = AuthMode::ClaudeOauth;
        anthropic.base_url = "http://127.0.0.1:3001".to_string();
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::OauthUsageSelfPollLoop { provider } if provider == "anthropic"
        ));
    }

    #[test]
    fn claude_oauth_provider_on_a_different_loopback_port_is_unaffected() {
        // A local debugging proxy/mock on a *different* loopback port must
        // stay allowed — the self-poll-loop guard only fires on a matching
        // port.
        let mut config = Config::default();
        config.server.bind = "127.0.0.1:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        let anthropic = config.providers.get_mut("anthropic").unwrap();
        anthropic.auth = AuthMode::ClaudeOauth;
        anthropic.base_url = "http://127.0.0.1:9999".to_string();
        config.validate().unwrap();
    }

    #[test]
    fn claude_oauth_provider_on_a_different_loopback_host_same_port_is_unaffected() {
        // A proxy on a *different* loopback address but the same port cannot
        // reach a listener bound to a specific loopback IP, so it must not trip
        // the self-poll guard: shunt binds `127.0.0.1:3001`, the provider is on
        // `[::1]:3001` (or `127.0.0.2:3001`).
        for base in ["http://[::1]:3001", "http://127.0.0.2:3001"] {
            let mut config = Config::default();
            config.server.bind = "127.0.0.1:3001".to_string();
            config.server.oauth_usage = Some(OauthUsageConfig::default());
            let anthropic = config.providers.get_mut("anthropic").unwrap();
            anthropic.auth = AuthMode::ClaudeOauth;
            anthropic.base_url = base.to_string();
            config
                .validate()
                .unwrap_or_else(|error| panic!("base {base} should be allowed, got {error:?}"));
        }
    }

    #[test]
    fn claude_oauth_provider_on_wildcard_bind_same_port_loopback_is_rejected() {
        // A wildcard bind (`0.0.0.0`) listens on every local address, so a
        // same-port loopback host does reach it and must still trip the guard.
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        // A wildcard bind also needs inbound auth to satisfy the non-loopback
        // precondition; give it a token so validation reaches the self-poll
        // check rather than failing earlier.
        let env = format!("SHUNT_SELF_POLL_WILDCARD_{}", std::process::id());
        std::env::set_var(&env, "tester:tok-secret");
        config.server.auth = Some(InboundAuthConfig {
            header: "x-shunt-token".to_string(),
            tokens_env: env.clone(),
        });
        let anthropic = config.providers.get_mut("anthropic").unwrap();
        anthropic.auth = AuthMode::ClaudeOauth;
        anthropic.base_url = "http://127.0.0.1:3001".to_string();
        let result = config.validate();
        std::env::remove_var(&env);
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::OauthUsageSelfPollLoop { provider } if provider == "anthropic"
        ));
    }

    #[test]
    fn claude_oauth_provider_on_cross_family_wildcard_bind_is_unaffected() {
        // An IPv4 wildcard bind (`0.0.0.0`) does not listen on an IPv6 loopback
        // literal, so a same-port `[::1]` provider cannot self-poll it and must
        // not be rejected.
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        let env = format!("SHUNT_SELF_POLL_XFAMILY_{}", std::process::id());
        std::env::set_var(&env, "tester:tok-secret");
        config.server.auth = Some(InboundAuthConfig {
            header: "x-shunt-token".to_string(),
            tokens_env: env.clone(),
        });
        let anthropic = config.providers.get_mut("anthropic").unwrap();
        anthropic.auth = AuthMode::ClaudeOauth;
        anthropic.base_url = "http://[::1]:3001".to_string();
        let result = config.validate();
        std::env::remove_var(&env);
        result.unwrap();
    }

    #[test]
    fn claude_oauth_provider_on_dual_stack_wildcard_bind_same_port_ipv4_is_rejected() {
        // An IPv6 wildcard bind (`[::]`) is dual-stack by default and accepts
        // IPv4 connections, so a same-port `127.0.0.1` provider *can* self-poll
        // it and must still trip the guard.
        let mut config = Config::default();
        config.server.bind = "[::]:3001".to_string();
        config.server.oauth_usage = Some(OauthUsageConfig::default());
        let env = format!("SHUNT_SELF_POLL_DUALSTACK_{}", std::process::id());
        std::env::set_var(&env, "tester:tok-secret");
        config.server.auth = Some(InboundAuthConfig {
            header: "x-shunt-token".to_string(),
            tokens_env: env.clone(),
        });
        let anthropic = config.providers.get_mut("anthropic").unwrap();
        anthropic.auth = AuthMode::ClaudeOauth;
        anthropic.base_url = "http://127.0.0.1:3001".to_string();
        let result = config.validate();
        std::env::remove_var(&env);
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::OauthUsageSelfPollLoop { provider } if provider == "anthropic"
        ));
    }

    #[test]
    fn oauth_usage_config_serde_round_trip() {
        // Presence-as-opt-in: an empty object deserializes, and the type
        // round-trips through JSON like `UsageEndpointConfig`.
        let empty: OauthUsageConfig = serde_json::from_str("{}").unwrap();
        let value = serde_json::to_value(&empty).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn host_is_chatgpt_matches_chatgpt_and_subdomains_only() {
        assert!(host_is_chatgpt("chatgpt.com"));
        assert!(host_is_chatgpt("x.chatgpt.com"));
        assert!(!host_is_chatgpt("chatgpt.com.evil.com"));
        assert!(!host_is_chatgpt("openai.com"));
    }

    #[test]
    fn host_is_kimi_matches_kimi_and_subdomains_only() {
        assert!(host_is_kimi("kimi.com"));
        assert!(host_is_kimi("api.kimi.com"));
        assert!(!host_is_kimi("evilkimi.com"));
        assert!(!host_is_kimi("notkimi.com"));
        assert!(!host_is_kimi("kimi.com.evil.com"));
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
    fn model_upstream_map_parses_from_toml_and_remains_optional() {
        let config: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(figment::providers::Toml::string(
                    r#"
[[models]]
id = "claude-opus-4-8"
display_name = "Claude Opus 4.8"
[models.upstream_model]
codex = "gpt-5.2"

[[models]]
id = "claude-sonnet-5"
"#,
                ))
                .extract()
                .unwrap();

        assert_eq!(
            config.models[0].upstream_model,
            Some(model_upstream("codex", "gpt-5.2"))
        );
        assert!(config.models[1].upstream_model.is_none());
    }

    #[test]
    fn model_upstream_map_rejects_unknown_provider() {
        let config = Config {
            models: vec![model_config(
                "claude-opus-4-8",
                Some(model_upstream("missing", "gpt-5.2")),
            )],
            ..Config::default()
        };

        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::UnknownModelProvider { model, provider }
                if model == "claude-opus-4-8" && provider == "missing"
        ));
    }

    #[test]
    fn model_upstream_map_rejects_multiple_providers() {
        let config = Config {
            models: vec![model_config(
                "claude-opus-4-8",
                Some(BTreeMap::from([
                    ("codex".to_string(), "gpt-5.2".to_string()),
                    ("openai".to_string(), "gpt-5.2".to_string()),
                ])),
            )],
            ..Config::default()
        };

        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::ModelUpstreamProviderCount { count: 2, .. }
        ));
        assert!(error
            .to_string()
            .contains("[providers.codex] -> [[upstreams]] + name = \"codex\""));
        assert!(error
            .to_string()
            .contains("[providers.openai] -> [[upstreams]] + name = \"openai\""));
    }

    #[test]
    fn model_upstream_map_rejects_empty_table() {
        let config = Config {
            models: vec![model_config("claude-opus-4-8", Some(BTreeMap::new()))],
            ..Config::default()
        };

        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ModelUpstreamProviderCount { count: 0, .. }
        ));
    }

    #[test]
    fn model_upstream_map_rejects_empty_upstream_model() {
        for upstream_model in ["", "   \t\n"] {
            let config = Config {
                models: vec![model_config(
                    "claude-opus-4-8",
                    Some(model_upstream("codex", upstream_model)),
                )],
                ..Config::default()
            };

            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::EmptyModelUpstream { model, provider }
                    if model == "claude-opus-4-8" && provider == "codex"
            ));
        }
    }

    #[test]
    fn model_upstream_map_rejects_empty_provider_name() {
        for provider in ["", "   \t\n"] {
            let mut config = Config::default();
            let provider_config = config.providers["codex"].clone();
            config
                .providers
                .insert(provider.to_string(), provider_config);
            config.models = vec![model_config(
                "claude-opus-4-8",
                Some(model_upstream(provider, "gpt-5.2")),
            )];

            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::EmptyModelUpstreamProvider { model }
                    if model == "claude-opus-4-8"
            ));
        }
    }

    #[test]
    fn model_upstream_map_rejects_explicit_route_conflict() {
        let config = Config {
            models: vec![model_config(
                "claude-opus-4-8",
                Some(model_upstream("codex", "gpt-5.2")),
            )],
            routes: vec![super::RouteConfig {
                model: "claude-opus-4-8".to_string(),
                provider: "codex".to_string(),
                upstream_model: Some("gpt-5.2".to_string()),
                effort: None,
                service_tier: None,
            }],
            ..Config::default()
        };

        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::ModelRouteConflict { model } if model == "claude-opus-4-8"
        ));
    }

    #[test]
    fn model_upstream_map_rejects_context_window_hint_in_id() {
        for id in ["claude-opus-4-8[1m]", "claude-opus-4-8[1M]"] {
            let config = Config {
                models: vec![model_config(id, Some(model_upstream("codex", "gpt-5.2")))],
                ..Config::default()
            };

            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::ModelUpstreamContextWindowHint { model } if model == id
            ));
        }
    }

    #[test]
    fn model_upstream_map_rejects_duplicate_map_bearing_ids() {
        let config = Config {
            models: vec![
                model_config("claude-opus-4-8", Some(model_upstream("codex", "gpt-5.2"))),
                model_config("claude-opus-4-8", Some(model_upstream("codex", "gpt-5.2"))),
            ],
            ..Config::default()
        };

        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateModelId { model } if model == "claude-opus-4-8"
        ));
    }

    #[test]
    fn model_upstream_map_rejects_duplicate_map_less_id_after_mapped_id() {
        let config = Config {
            models: vec![
                model_config("claude-opus-4-8", Some(model_upstream("codex", "gpt-5.2"))),
                model_config("claude-opus-4-8", None),
            ],
            ..Config::default()
        };

        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateModelId { model } if model == "claude-opus-4-8"
        ));
    }

    #[test]
    fn model_upstream_map_rejects_duplicate_map_less_id_before_mapped_id() {
        let config = Config {
            models: vec![
                model_config("claude-opus-4-8", None),
                model_config("claude-opus-4-8", Some(model_upstream("codex", "gpt-5.2"))),
            ],
            ..Config::default()
        };

        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateModelId { model } if model == "claude-opus-4-8"
        ));
    }

    #[test]
    fn duplicate_map_less_model_ids_remain_valid() {
        let config = Config {
            models: vec![
                model_config("claude-opus-4-8", None),
                model_config("claude-opus-4-8", None),
            ],
            ..Config::default()
        };

        config.validate().unwrap();
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
                upstream_model: None,
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
    fn validate_does_not_warn_for_routable_model_upstream_map() {
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
            models: vec![model_config(
                "claude-opus-via-codex",
                Some(model_upstream("codex", "gpt-5.2")),
            )],
            ..Config::default()
        };

        tracing::subscriber::with_default(subscriber, || {
            config.validate().unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(!logs.contains("configured discovery model has no matching route"));
        assert!(!logs.contains("claude-opus-via-codex"));
    }

    #[test]
    fn validate_warns_when_service_tier_is_withheld_for_xai_flavor() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let mut config = Config::default();
        config.providers.get_mut("xai").unwrap().service_tier = Some("priority".to_string());

        let config = tracing::subscriber::with_default(subscriber, || config.validate().unwrap());
        assert_eq!(config.responses_flavor("xai"), ResponsesFlavor::Xai);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(logs
            .contains("service_tier is configured but withheld on the xai/grok Responses flavor"));
        assert!(logs.contains("xai"));
    }

    #[test]
    fn validate_does_not_warn_when_service_tier_is_configured_for_openai_flavor() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let mut config = Config::default();
        config.providers.get_mut("openai").unwrap().service_tier = Some("priority".to_string());

        tracing::subscriber::with_default(subscriber, || {
            config.validate().unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(!logs.contains("service_tier is configured but withheld"));
    }

    #[test]
    fn validate_does_not_warn_when_service_tier_is_default_sentinel_for_xai_flavor() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let mut config = Config::default();
        config.providers.get_mut("xai").unwrap().service_tier = Some("default".to_string());

        let config = tracing::subscriber::with_default(subscriber, || config.validate().unwrap());
        assert_eq!(config.responses_flavor("xai"), ResponsesFlavor::Xai);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        // "default" is a client-only sentinel stripped unconditionally at wire
        // emission on every flavor, so it is never actually withheld -- warning
        // here would be misleading.
        assert!(!logs.contains("service_tier is configured but withheld"));
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
    fn default_seeds_the_native_antigravity_provider_and_the_deprecated_cli_one() {
        let config = Config::default();

        // `antigravity` is the native HTTP upstream.
        let native = config.provider("antigravity").unwrap();
        assert_eq!(native.kind, ProviderKind::Antigravity);
        assert_eq!(native.auth, AuthMode::AntigravityOauth);
        assert_eq!(native.base_url, "https://daily-cloudcode-pa.googleapis.com");

        // The `agy` subprocess transport kept its behaviour under a new name.
        let cli = config.provider("antigravity-cli").unwrap();
        assert_eq!(cli.kind, ProviderKind::AntigravityCli);
        assert_eq!(cli.auth, AuthMode::None);
        assert_eq!(cli.base_url, "http://localhost");
        assert!(cli.sandbox);

        config.validate().unwrap();
    }

    #[test]
    fn a_legacy_antigravity_block_is_rejected_by_name_rather_than_retargeted() {
        // The whole point of the rename: a config that meant "run the local
        // `agy` binary" must not resolve quietly to an OAuth HTTP upstream with
        // different credentials, egress, and failure modes.
        // The two shapes a legacy config can actually have: the old built-in
        // preset used `none`, and an omitted `auth` defaults to passthrough.
        // (`api_key` is not a legacy shape and is caught earlier by the
        // missing-`api_key_env` guard.)
        for auth in [AuthMode::None, AuthMode::Passthrough] {
            let mut config = Config::default();
            let provider = config.providers.get_mut("antigravity").unwrap();
            provider.auth = auth;
            let error = config.validate().unwrap_err();
            assert!(
                matches!(error, ConfigError::AntigravityKindRequiresOauth { .. }),
                "auth {auth:?} should be refused, got: {error}"
            );
            // The message has to name both ways forward, or the operator has to
            // guess which transport they are on.
            let text = error.to_string();
            assert!(text.contains("antigravity_oauth"), "message: {text}");
            assert!(text.contains("antigravity_cli"), "message: {text}");
        }
    }

    #[test]
    fn a_legacy_antigravity_table_without_auth_is_rejected_not_silently_retargeted() {
        // Shaped like a real pre-#372 config: `kind = "antigravity"` meant "run
        // the local `agy` binary", so it never carried an `auth` key (there was
        // nothing to authenticate over HTTP with) and carried CLI-only knobs
        // like `workspace_roots`/`sandbox`. Figment deep-merges this table over
        // the built-in `antigravity` default, which *does* set
        // `auth = "antigravity_oauth"` -- if the merge fills the missing `auth`
        // key from the default rather than the table being rejected outright,
        // this legacy config would resolve quietly to the new OAuth HTTP
        // upstream instead of erroring by name like the sibling test above.
        //
        // `Config::load` also reads the real process env, and the sibling
        // tests below set `SHUNT_PROVIDERS__ANTIGRAVITY__*` -- a name
        // `Env::prefixed("SHUNT_").split("__")` fixes, so it cannot be given
        // a per-test-unique suffix like the ordinary `CONFIG_ENV_LOCK` tests
        // use. Taking the same lock here, even though this test sets no env
        // var itself, is what keeps it from observing one of those vars left
        // set by a concurrently running sibling.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-legacy-antigravity-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        std::fs::write(
            &path,
            "[providers.antigravity]\n\
             kind = \"antigravity\"\n\
             base_url = \"http://localhost\"\n\
             workspace_roots = [\"/home/user/project\"]\n\
             sandbox = true\n",
        )
        .unwrap();

        let error = Config::load(Some(&path))
            .expect_err("a legacy antigravity table without `auth` must not load");
        assert!(
            matches!(error, ConfigError::AntigravityLegacyTableMissingAuth),
            "expected AntigravityLegacyTableMissingAuth, got: {error}"
        );
        let text = error.to_string();
        assert!(text.contains("antigravity_oauth"), "message: {text}");
        assert!(text.contains("antigravity_cli"), "message: {text}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_env_only_legacy_antigravity_shape_is_rejected() {
        // Same legacy shape as the sibling file-table test above, but
        // assembled entirely through `SHUNT_PROVIDERS__ANTIGRAVITY__*` env
        // vars rather than a `[providers.antigravity]` file table. Before the
        // guard was moved to look at the *effective* (file + env) figment
        // instead of the file layer alone, this shape was invisible to it:
        // there was no file table for the guard to inspect, so it let the
        // config through, and the built-in `antigravity` default's
        // `auth = "antigravity_oauth"` was silently deep-merged in.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-env-legacy-antigravity-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        // No `providers.antigravity` table at all -- the config file has
        // nothing to say about it.
        std::fs::write(&path, "[server]\ndefault_provider = \"anthropic\"\n").unwrap();

        std::env::set_var(
            "SHUNT_PROVIDERS__ANTIGRAVITY__BASE_URL",
            "http://localhost:9999",
        );

        let error = Config::load(Some(&path))
            .expect_err("an env-only legacy antigravity shape must not load");
        assert!(
            matches!(error, ConfigError::AntigravityLegacyTableMissingAuth),
            "expected AntigravityLegacyTableMissingAuth, got: {error}"
        );

        std::env::remove_var("SHUNT_PROVIDERS__ANTIGRAVITY__BASE_URL");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_legacy_antigravity_table_completed_by_env_auth_is_accepted() {
        // The mirror image of the two rejection tests above: a file table
        // shaped like a pre-#372 legacy config (`kind = "antigravity"`, no
        // `auth`) that a `SHUNT_PROVIDERS__ANTIGRAVITY__AUTH` env var
        // legitimately completes. Before the guard was moved to look at the
        // effective (file + env) figment, it ran against the file layer
        // alone and rejected this config before the env layer ever had a
        // chance to supply the missing `auth` key.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-env-completed-antigravity-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        std::fs::write(
            &path,
            "[providers.antigravity]\nkind = \"antigravity\"\nbase_url = \"http://localhost\"\n",
        )
        .unwrap();

        std::env::set_var("SHUNT_PROVIDERS__ANTIGRAVITY__AUTH", "antigravity_oauth");

        let config = Config::load(Some(&path))
            .expect("a legacy table completed by an env-supplied auth must load");
        let antigravity = config.provider("antigravity").unwrap();
        assert_eq!(antigravity.kind, ProviderKind::Antigravity);
        assert_eq!(antigravity.auth, AuthMode::AntigravityOauth);

        std::env::remove_var("SHUNT_PROVIDERS__ANTIGRAVITY__AUTH");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn antigravity_cli_migration_backfills_auth_from_the_cli_default() {
        // The documented migration path off the ambiguous legacy shape
        // above: a `[providers.antigravity]` table whose only content is
        // `kind = "antigravity_cli"`, opting explicitly into the deprecated
        // subprocess transport. Without
        // `backfill_antigravity_cli_migration_auth`, the merge still
        // inherits `auth = antigravity_oauth` from the built-in
        // `antigravity` default for this same name (the identity that
        // `kind` just overrode), and `validate` rejects the result as
        // `AntigravityOauthWrongKind` -- breaking the very migration path
        // this shape is supposed to allow.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-antigravity-cli-migration-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        std::fs::write(
            &path,
            "[providers.antigravity]\nkind = \"antigravity_cli\"\n",
        )
        .unwrap();

        let config = Config::load(Some(&path))
            .expect("the documented antigravity -> antigravity_cli migration must load");
        let antigravity = config.provider("antigravity").unwrap();
        assert_eq!(antigravity.kind, ProviderKind::AntigravityCli);
        assert_eq!(antigravity.auth, AuthMode::None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn antigravity_oauth_requires_the_antigravity_kind() {
        let mut config = Config::default();
        // Carrying the token on an anthropic-kind provider would forward the
        // client's own credential instead of injecting the Antigravity one.
        config.providers.get_mut("antigravity").unwrap().kind = ProviderKind::Anthropic;
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::AntigravityOauthWrongKind { .. }
        ));
    }

    #[test]
    fn antigravity_oauth_rejects_an_off_origin_host() {
        let mut config = Config::default();
        config.providers.get_mut("antigravity").unwrap().base_url =
            "https://evil.example.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::AntigravityOauthNonGoogleHost { .. }
        ));
        assert!(error.to_string().contains("evil.example.com"));
    }

    #[test]
    fn antigravity_oauth_requires_https() {
        let mut config = Config::default();
        config.providers.get_mut("antigravity").unwrap().base_url =
            "http://cloudcode-pa.googleapis.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::AntigravityOauthNotHttps { .. }
        ));
        assert!(error.to_string().contains("plaintext"));
    }

    #[test]
    fn antigravity_oauth_accepts_both_antigravity_backends() {
        // The `daily-` control plane is the default and the only host that
        // serves the Antigravity catalog; production stays valid for operators
        // who still point at it.
        for base_url in [
            "https://daily-cloudcode-pa.googleapis.com",
            "https://cloudcode-pa.googleapis.com",
        ] {
            let mut config = Config::default();
            config.providers.get_mut("antigravity").unwrap().base_url = base_url.to_string();
            config
                .validate()
                .unwrap_or_else(|error| panic!("{base_url} must validate: {error}"));
        }
    }

    #[test]
    fn antigravity_oauth_rejects_other_googleapis_hosts() {
        // Widening the guard to the `daily-` host must not widen it to the
        // whole domain: the sandbox spelling and every other googleapis.com
        // product are different backends the subscription bearer must not
        // reach.
        for host in [
            "daily-cloudcode-pa.sandbox.googleapis.com",
            "generativelanguage.googleapis.com",
            "storage.googleapis.com",
        ] {
            let mut config = Config::default();
            config.providers.get_mut("antigravity").unwrap().base_url = format!("https://{host}");
            let error = config.validate().unwrap_err();
            assert!(matches!(
                error,
                ConfigError::AntigravityOauthNonGoogleHost { .. }
            ));
            assert!(error.to_string().contains(host));
        }
    }

    #[test]
    fn google_oauth_still_rejects_the_antigravity_daily_host() {
        // The wider host predicate belongs to `antigravity_oauth` alone. Code
        // Assist's own guard stays pinned to the production host.
        let mut config = Config::default();
        config.providers.get_mut("gemini").unwrap().base_url =
            "https://daily-cloudcode-pa.googleapis.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::GoogleOauthNonGoogleHost { .. }
        ));
        assert!(error
            .to_string()
            .contains("daily-cloudcode-pa.googleapis.com"));
    }

    #[test]
    fn the_native_antigravity_provider_rides_the_gemini_adapter() {
        // Stage 1 is wire-identical to Code Assist, so it must dispatch to the
        // Gemini adapter rather than the `agy` subprocess one.
        use crate::routing::AdapterKind;
        assert_eq!(
            AdapterKind::from(ProviderKind::Antigravity),
            AdapterKind::Gemini
        );
        assert_eq!(
            AdapterKind::from(ProviderKind::AntigravityCli),
            AdapterKind::AntigravityCli
        );
    }

    #[test]
    fn default_seeds_builtin_gemini_provider() {
        let config = Config::default();
        let gemini = config.provider("gemini").unwrap();
        assert_eq!(gemini.kind, ProviderKind::Gemini);
        assert_eq!(gemini.base_url, "https://cloudcode-pa.googleapis.com");
        assert_eq!(gemini.auth, AuthMode::GoogleOauth);
        // A gemini provider routes through the Gemini adapter.
        assert_eq!(
            crate::routing::AdapterKind::from(gemini.kind),
            crate::routing::AdapterKind::Gemini
        );
        // The built-in gemini provider (googleapis.com over https) validates.
        assert!(config.validate().is_ok());
    }

    #[test]
    fn google_oauth_requires_gemini_kind() {
        let mut config = Config::default();
        config.providers.get_mut("gemini").unwrap().kind = ProviderKind::Anthropic;
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::GoogleOauthWrongKind { .. }));
    }

    #[test]
    fn google_oauth_rejects_non_google_host() {
        // Pointing a google_oauth provider off-origin is refused (bearer-leak guard).
        let mut config = Config::default();
        config.providers.get_mut("gemini").unwrap().base_url =
            "https://evil.example.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::GoogleOauthNonGoogleHost { .. }
        ));
        assert!(error.to_string().contains("evil.example.com"));
    }

    #[test]
    fn google_oauth_rejects_other_googleapis_subdomain() {
        // Non-Code-Assist Google subdomains (e.g. storage) are refused to avoid bearer leakage.
        let mut config = Config::default();
        config.providers.get_mut("gemini").unwrap().base_url =
            "https://storage.googleapis.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(
            error,
            ConfigError::GoogleOauthNonGoogleHost { .. }
        ));
        assert!(error.to_string().contains("storage.googleapis.com"));
    }

    #[test]
    fn google_oauth_requires_https_base_url() {
        let mut config = Config::default();
        config.providers.get_mut("gemini").unwrap().base_url =
            "http://cloudcode-pa.googleapis.com".to_string();
        let error = config.validate().unwrap_err();
        assert!(matches!(error, ConfigError::GoogleOauthNotHttps { .. }));
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
    fn native_tool_search_defaults_on_and_gates_on_flavor_and_model() {
        let config = Config::default();
        // Auto (`tool_search` unset) resolves to native for these two built-in
        // providers because both are known-good hosts — codex is the
        // ChatGPT/Codex backend and openai targets api.openai.com — so no flag
        // needs setting for a supported flavor + model.
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
        // Unsupported flavor keeps the shim (xAI), even though `tool_search`
        // auto-resolves to on for a known host — the flavor gate blocks it
        // regardless.
        assert!(!config.native_tool_search("xai", "gpt-5.6-sol"));
        // Unknown provider is never native.
        assert!(!config.native_tool_search("nope", "gpt-5.6-sol"));
    }

    #[test]
    fn native_tool_search_false_forces_shim_opt_out() {
        let mut config = Config::default();
        // Explicit opt-out reaches even a supported flavor + model.
        config.providers.get_mut("codex").unwrap().tool_search = Some(false);
        config.providers.get_mut("openai").unwrap().tool_search = Some(false);

        assert!(!config.native_tool_search("codex", "gpt-5.6-sol"));
        assert!(!config.native_tool_search("openai", "gpt-5.4"));
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

    /// zstd request compression is on by default, but only on the ChatGPT/Codex
    /// flavor — the only backend verified to accept a compressed request body
    /// (issue #285).
    #[test]
    fn request_compression_gated_on_flag_and_chatgpt_flavor() {
        let config = Config::default();
        assert!(config.responses_request_compression("codex"));

        // Stock OpenAI, xAI, and the Grok CLI proxy stay uncompressed even though
        // the flag defaults on.
        assert!(!config.responses_request_compression("openai"));
        assert!(!config.responses_request_compression("xai"));
        assert!(!config.responses_request_compression("grok"));

        // Unknown provider ⇒ false.
        assert!(!config.responses_request_compression("nope"));

        // Per-provider opt-out.
        let mut config = Config::default();
        config
            .providers
            .get_mut("codex")
            .unwrap()
            .request_compression = false;
        assert!(!config.responses_request_compression("codex"));
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
            dsn: dsn.into(),
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

    /// A destination URL is a *base* endpoint that `signal_url` extends by
    /// string concatenation, so a query, fragment, or embedded credential is
    /// rejected at boot rather than misrouted or leaked into a log.
    #[test]
    fn gateway_telemetry_rejects_query_fragment_and_userinfo_urls() {
        for url in [
            "https://collector.example?tenant=acme",
            "https://collector.example#frag",
            "https://user:secret@collector.example",
        ] {
            let telemetry = GatewayTelemetryConfig {
                forward_to: vec![GatewayTelemetryDestination {
                    url: url.to_string(),
                    headers: None,
                    metrics: true,
                    logs: false,
                    traces: false,
                }],
            };
            assert!(
                matches!(
                    super::validate_gateway_telemetry(Some(&telemetry)),
                    Err(ConfigError::InvalidGatewayTelemetryUrl { index: 0, .. })
                ),
                "{url} must be rejected"
            );
        }
    }

    /// The shapes that stay valid: a bare origin and one with a path prefix.
    #[test]
    fn gateway_telemetry_accepts_base_endpoints_with_and_without_a_path() {
        for url in [
            "https://collector.example",
            "https://collector.example/otlp",
        ] {
            let telemetry = GatewayTelemetryConfig {
                forward_to: vec![GatewayTelemetryDestination {
                    url: url.to_string(),
                    headers: None,
                    metrics: true,
                    logs: false,
                    traces: false,
                }],
            };
            assert!(
                super::validate_gateway_telemetry(Some(&telemetry)).is_ok(),
                "{url} must be accepted"
            );
        }
    }

    /// An invalid header must fail startup rather than be dropped at relay
    /// time — it is usually the collector's auth key, and a runtime skip would
    /// exclude it silently for the life of the process.
    #[test]
    fn gateway_telemetry_rejects_invalid_header_names_and_values() {
        let destination = |name: &str, value: &str| GatewayTelemetryConfig {
            forward_to: vec![GatewayTelemetryDestination {
                url: "https://collector.example".to_string(),
                headers: Some(
                    [(name.to_string(), value.to_string().into())]
                        .into_iter()
                        .collect(),
                ),
                metrics: true,
                logs: false,
                traces: false,
            }],
        };

        // A space is legal in TOML but not in an HTTP header name.
        let bad_name = destination("x collector key", "value");
        assert!(matches!(
            super::validate_gateway_telemetry(Some(&bad_name)),
            Err(ConfigError::InvalidGatewayTelemetryHeader {
                index: 0,
                part: "name",
                ..
            })
        ));

        // A newline in the value would otherwise be a header-injection vector.
        let bad_value = destination("x-collector-key", "line\nbreak");
        assert!(matches!(
            super::validate_gateway_telemetry(Some(&bad_value)),
            Err(ConfigError::InvalidGatewayTelemetryHeader {
                index: 0,
                part: "value",
                ..
            })
        ));

        // The error must not leak the value, which is typically a secret.
        let rendered = super::validate_gateway_telemetry(Some(&destination(
            "x-collector-key",
            "super\u{0}secret",
        )))
        .unwrap_err()
        .to_string();
        assert!(rendered.contains("x-collector-key"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");

        // A well-formed header is still accepted.
        let good = destination("x-collector-key", "collector-secret");
        assert!(super::validate_gateway_telemetry(Some(&good)).is_ok());
    }

    #[test]
    fn gateway_telemetry_signals_default_to_metrics_only() {
        use figment::providers::{Format, Toml};
        let telemetry: super::GatewayTelemetryConfig = figment::Figment::from(Toml::string(
            "[[forward_to]]\nurl = \"https://collector.example\"\n",
        ))
        .extract()
        .unwrap();
        let destination = &telemetry.forward_to[0];
        assert!(destination.metrics);
        assert!(!destination.logs);
        assert!(!destination.traces);
        assert!(destination.headers.is_none());
    }

    #[test]
    fn gateway_telemetry_signals_parse_explicit_values() {
        use figment::providers::{Format, Toml};
        let telemetry: super::GatewayTelemetryConfig = figment::Figment::from(Toml::string(
            "[[forward_to]]\nurl = \"https://collector.example\"\nmetrics = false\nlogs = true\ntraces = true\nheaders = { \"x-api-key\" = \"secret\" }\n",
        ))
        .extract()
        .unwrap();
        let destination = &telemetry.forward_to[0];
        assert!(!destination.metrics);
        assert!(destination.logs);
        assert!(destination.traces);
        assert_eq!(destination.headers.as_ref().unwrap()["x-api-key"], "secret");
    }

    #[test]
    fn load_errors_when_explicit_config_path_is_missing() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    fn auto_include_builtin_models_defaults_to_true() {
        assert!(Config::default().auto_include_builtin_models);
    }

    #[test]
    fn auto_include_builtin_models_parses_false_from_toml() {
        let config: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(figment::providers::Toml::string(
                    "auto_include_builtin_models = false",
                ))
                .extract()
                .unwrap();

        assert!(!config.auto_include_builtin_models);
    }

    #[test]
    fn toml_adds_a_provider_and_merges_builtin_overrides() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
api_key_env = "MOONSHOT_API_KEY"

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
        assert_eq!(kimi.api_key_env.as_deref(), Some("MOONSHOT_API_KEY"));
        // Built-in codex kept its default base_url/auth while gaining effort.
        let codex = config.provider("codex").unwrap();
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(codex.auth, AuthMode::ChatgptOauth);
        assert_eq!(codex.effort.as_deref(), Some("high"));

        let _ = std::fs::remove_dir_all(dir);
    }

    // Issue #345: `${VAR}`/`${file:...}` reference substitution runs on the
    // file layer before figment ever sees it. These tests exercise the pass
    // through the full `Config::load` path (TOML + YAML), confirm `*_env`
    // fields (which name an env var rather than referencing one) are left
    // alone, confirm the shipped example files still load after the
    // parse/walk/re-serialize round trip, and confirm `Secret` fields behave
    // correctly whether fed by a reference or a literal. Unit coverage for
    // the substitution pass itself and the `Secret` type lives in
    // `src/config/secrets.rs`.

    #[test]
    fn toml_resolves_env_var_reference_in_a_normal_field() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-toml-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        let env = format!("SHUNT_CONFIG_TEST_BASE_HOST_{}", std::process::id());
        std::env::set_var(&env, "api.moonshot.ai");
        let reference = format!("${{{env}}}");
        std::fs::write(
            &path,
            format!(
                "[providers.kimi]\nkind = \"anthropic\"\nbase_url = \"https://{reference}/anthropic\"\nauth = \"api_key\"\napi_key_env = \"MOONSHOT_API_KEY\"\n"
            ),
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();
        let kimi = config.provider("kimi").unwrap();
        assert_eq!(kimi.base_url, "https://api.moonshot.ai/anthropic");

        std::env::remove_var(&env);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn yaml_resolves_env_var_reference_in_a_normal_field() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-yaml-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.yaml");
        let env = format!("SHUNT_CONFIG_TEST_YAML_HOST_{}", std::process::id());
        std::env::set_var(&env, "api.moonshot.ai");
        let reference = format!("${{{env}}}");
        std::fs::write(
            &path,
            format!(
                "providers:\n  kimi:\n    kind: anthropic\n    base_url: \"https://{reference}/anthropic\"\n    auth: api_key\n    api_key_env: MOONSHOT_API_KEY\n"
            ),
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();
        let kimi = config.provider("kimi").unwrap();
        assert_eq!(kimi.base_url, "https://api.moonshot.ai/anthropic");

        std::env::remove_var(&env);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tokens_env_field_is_unaffected_by_the_substitution_pass() {
        // `tokens_env` *names* an environment variable; it must never be
        // treated as a `${...}` reference itself, since it contains no `${`.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-tokens-env-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        let env = format!("SHUNT_CONFIG_TEST_TOKENS_{}", std::process::id());
        std::env::set_var(&env, "alice:tok-a");
        std::fs::write(
            &path,
            format!(
                "[server]\ndefault_provider = \"anthropic\"\n\n[server.auth]\ntokens_env = \"{env}\"\n"
            ),
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.server.auth.as_ref().unwrap().tokens_env, env);
        assert!(config.resolve_inbound_auth().unwrap().is_some());

        std::env::remove_var(&env);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn example_config_files_still_load() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-examples-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // `ConfigFormat::from_path` detects format from the file's real
        // extension; the example files' last extension segment is
        // `.example`, not `.toml`/`.yaml`, so copy each to the extension its
        // own header comment tells users to save it as ("copy to
        // ./shunt.yaml and edit") before loading.
        let toml_path = dir.join("shunt.toml");
        std::fs::copy(root.join("shunt.toml.example"), &toml_path).unwrap();
        Config::load(Some(&toml_path))
            .expect("shunt.toml.example loads through the substitution pass");

        let yaml_path = dir.join("shunt.yaml");
        std::fs::copy(root.join("shunt.yaml.example"), &yaml_path).unwrap();
        Config::load(Some(&yaml_path))
            .expect("shunt.yaml.example loads through the substitution pass");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn literal_admin_key_is_rejected_while_reference_fed_keys_load() {
        // The pre-existing `Secret` fields only warn about a literal, because
        // deployments already hold literals there. The admin key arrays are
        // new, so a literal is refused outright — this pins the difference.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-admin-key-literal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens_env = format!(
            "SHUNT_CONFIG_TEST_ADMIN_LITERAL_TOKENS_{}",
            std::process::id()
        );
        std::env::remove_var(&tokens_env);

        // A key written straight into the file is a load error, and the error
        // names the path without echoing the key.
        let toml_path = dir.join("shunt.toml");
        let literal_toml = format!(
            "[server.admin]\ntokens_env = \"{tokens_env}\"\n\n\
             [[server.admin.write_keys]]\nid = \"terraform\"\nkey = \"{ADMIN_KEY_A}\"\n"
        );
        std::fs::write(&toml_path, &literal_toml).unwrap();
        let error =
            Config::load(Some(&toml_path)).expect_err("a literal admin key must be refused");
        assert!(matches!(
            error,
            ConfigError::LiteralAdminKey { ref path }
                if path == "server.admin.write_keys.0.key"
        ));
        assert!(!error.to_string().contains(ADMIN_KEY_A));

        // The same holds for `read_keys`, and for a YAML config file — the
        // literals map is shared by both parsers.
        let yaml_path = dir.join("shunt.yaml");
        std::fs::write(
            &yaml_path,
            format!(
                "server:\n  admin:\n    tokens_env: \"{tokens_env}\"\n    read_keys:\n\
                 \x20     - id: reporting\n        key: \"{ADMIN_KEY_B}\"\n"
            ),
        )
        .unwrap();
        assert!(matches!(
            Config::load(Some(&yaml_path)),
            Err(ConfigError::LiteralAdminKey { ref path })
                if path == "server.admin.read_keys.0.key"
        ));

        // A `${VAR}` reference loads and resolves to the real key.
        let key_env = format!("SHUNT_CONFIG_TEST_ADMIN_KEY_{}", std::process::id());
        std::env::set_var(&key_env, ADMIN_KEY_A);
        std::fs::write(
            &toml_path,
            format!(
                "[server.admin]\ntokens_env = \"{tokens_env}\"\n\n\
                 [[server.admin.write_keys]]\nid = \"terraform\"\nkey = \"${{{key_env}}}\"\n"
            ),
        )
        .unwrap();
        let config = Config::load(Some(&toml_path)).expect("a ${VAR}-fed key loads");
        let admin = config.server.admin.as_ref().unwrap();
        assert_eq!(admin.write_keys[0].key.expose(), ADMIN_KEY_A);
        std::env::remove_var(&key_env);

        // So does a `${file:}` reference.
        let key_file = dir.join("admin-key");
        std::fs::write(&key_file, format!("{ADMIN_KEY_B}\n")).unwrap();
        std::fs::write(
            &toml_path,
            format!(
                "[server.admin]\ntokens_env = \"{tokens_env}\"\n\n\
                 [[server.admin.read_keys]]\nid = \"reporting\"\nkey = \"${{file:{}}}\"\n",
                key_file.display()
            ),
        )
        .unwrap();
        let config = Config::load(Some(&toml_path)).expect("a ${file:}-fed key loads");
        let admin = config.server.admin.as_ref().unwrap();
        assert_eq!(admin.read_keys[0].key.expose(), ADMIN_KEY_B);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_field_fed_by_a_reference_resolves_and_still_redacts() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-sentry-ref-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        let env = format!("SHUNT_CONFIG_TEST_DSN_{}", std::process::id());
        let dsn = "https://public@o0.ingest.sentry.io/1";
        std::env::set_var(&env, dsn);
        let reference = format!("${{{env}}}");
        std::fs::write(&path, format!("[sentry]\ndsn = \"{reference}\"\n")).unwrap();

        let config = Config::load(Some(&path)).unwrap();
        let sentry = config.sentry.as_ref().unwrap();
        assert_eq!(sentry.dsn.expose(), dsn);
        assert_eq!(format!("{:?}", sentry.dsn), "[redacted]");

        std::env::remove_var(&env);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_field_with_a_literal_value_still_loads_successfully() {
        // A literal secret in the config file is allowed; the aggregated
        // warning is advisory only and must never fail the load, and it
        // must name the offending field's path so an operator can act on it
        // (issue #348: attribution used to be by *value*, not field
        // identity, so an unrelated field could be misnamed instead — see
        // the sibling tests below for the specific regressions this guards).
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-sentry-literal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        std::fs::write(
            &path,
            "[sentry]\ndsn = \"https://public@o0.ingest.sentry.io/1\"\n",
        )
        .unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let config = tracing::subscriber::with_default(subscriber, || Config::load(Some(&path)));
        assert!(config.is_ok());
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("[sentry].dsn"), "{logs}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reference_fed_secret_never_warns_even_when_a_plain_field_shares_its_value() {
        // Regression for issue #348's R3: a `Secret` populated from a
        // `${VAR}` reference must never trigger the literal-secret warning,
        // even when an unrelated *non*-Secret field happens to hold that
        // same resolved value written literally. Attribution used to be by
        // value rather than field identity, so this used to wrongly warn
        // about `[otel].endpoint` — a plain `String` field, not a `Secret` —
        // even though there was no literal secret anywhere in the file.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-r3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        let env = format!("SHUNT_CONFIG_TEST_R3_DSN_{}", std::process::id());
        let dsn = "https://public@o0.ingest.sentry.io/1";
        std::env::set_var(&env, dsn);
        let reference = format!("${{{env}}}");
        std::fs::write(
            &path,
            format!("[sentry]\ndsn = \"{reference}\"\n\n[otel]\nendpoint = \"{dsn}\"\n"),
        )
        .unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let config =
            tracing::subscriber::with_default(subscriber, || Config::load(Some(&path)).unwrap());
        assert_eq!(config.sentry.as_ref().unwrap().dsn.expose(), dsn);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        // `Config::load` always emits an unconditional "loaded config" INFO
        // line, so asserting `logs.is_empty()` would be wrong here; what must
        // be absent is the literal-secret WARN.
        assert!(
            !logs.contains("are written literally in the config file"),
            "expected no literal-secret warning, got: {logs}"
        );

        std::env::remove_var(&env);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn literal_secret_warning_names_only_the_secret_field_on_value_collision() {
        // Regression for issue #348's R1/R2: when a literal `Secret` field
        // and a plain (non-Secret) field happen to hold the exact same
        // string, the warning must name only the `Secret`'s path. Value-keyed
        // attribution used to name *every* path bound to that value,
        // regardless of whether the field was actually a `Secret`.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-r1-r2-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        let shared = "https://public@o0.ingest.sentry.io/1";
        std::fs::write(
            &path,
            format!("[sentry]\ndsn = \"{shared}\"\n\n[otel]\nendpoint = \"{shared}\"\n"),
        )
        .unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            Config::load(Some(&path)).unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(logs.contains("[sentry].dsn"), "{logs}");
        assert!(!logs.contains("[otel].endpoint"), "{logs}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn literal_secret_warning_at_two_secret_paths_reports_a_count_without_naming_a_cause() {
        // Regression guard for the wording fixed alongside #348: attribution
        // can fail for two unrelated reasons -- the same literal value
        // sitting at more than one Secret-shaped path (this test's
        // scenario: `sentry.dsn` and `otel.headers.<key>` sharing one
        // string), or a literal value at a path the hand-maintained
        // allowlist hasn't caught up to (see
        // `literal_hit_at_a_path_missing_from_the_secret_field_allowlist_is_unattributed_not_silent`
        // in src/config/secrets.rs). The aggregated warning must report a
        // count either way, and must never claim the more specific "more
        // than one Secret field" cause -- that would be wrong for the
        // drifted-allowlist case.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-unattributed-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        // A clearly-fake literal (the same placeholder DSN the sibling tests
        // above use), never a real-looking credential; must also be a
        // syntactically valid DSN since `sentry.dsn` is validated as one. No
        // `SHUNT_*` env var may hold this same value, or `never_literal`
        // would suppress the warning by design.
        let shared = "https://public@o0.ingest.sentry.io/1";
        std::fs::write(
            &path,
            format!(
                "[server]\ndefault_provider = \"anthropic\"\n\n\
                 [sentry]\ndsn = \"{shared}\"\n\n\
                 [otel]\nendpoint = \"https://otel.example.com\"\nservice_name = \"shunt-test\"\n\n\
                 [otel.headers]\nx-fake-header = \"{shared}\"\n"
            ),
        )
        .unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        let config =
            tracing::subscriber::with_default(subscriber, || Config::load(Some(&path)).unwrap());
        // Advisory only: a literal secret at an ambiguous path must never
        // fail the load, and the value itself still resolves normally.
        assert_eq!(config.sentry.as_ref().unwrap().dsn.expose(), shared);

        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains(
                "2 config value(s) are written literally in the config file but \
                            could not be attributed to a specific field"
            ),
            "{logs}"
        );
        assert!(
            !logs.contains("more than one Secret field"),
            "the message must not assert why attribution failed, only that it did: {logs}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_secret_and_empty_plain_fields_never_warn() {
        // An empty string is the documented "disabled" sentinel (e.g.
        // `SHUNT_SENTRY__DSN=""`), never a credential, so it must never
        // trigger the literal-secret warning — including when other empty
        // string fields are also present in the file.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-secrets-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        std::fs::write(
            &path,
            "[sentry]\ndsn = \"\"\n\n[otel]\nendpoint = \"\"\nservice_name = \"\"\n",
        )
        .unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            Config::load(Some(&path)).unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        // `Config::load` always emits an unconditional "loaded config" INFO
        // line, so asserting `logs.is_empty()` would be wrong here; what must
        // be absent is the literal-secret WARN.
        assert!(
            !logs.contains("are written literally in the config file"),
            "expected no literal-secret warning, got: {logs}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn literal_gateway_session_jwt_secret_is_attributed_in_both_wire_forms() {
        // End-to-end companion to
        // `is_secret_field_path_matches_gateway_session_jwt_secret_scalar_and_array_forms`
        // in `secrets.rs`. That test hand-builds the `LiteralScope` path map,
        // so it is self-consistent with the `is_secret_field_path` arm rather
        // than with the real traversal: were `walk_toml` to emit a different
        // shape for array elements, the arm and the test would be wrong
        // together and stay green while the boot warning silently degraded to
        // an unattributed count. This one drives the actual `Config::load`
        // pass instead. It also pins the exact number of occurrences, so it
        // fails if `jwt_secret`'s `untagged` deserializer ever records a value
        // twice — serde buffers content for untagged variants, and
        // `Secret::deserialize` has the `record_literal_hit` side effect.
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `[server.gateway]` resolves only with an approval source; this value
        // is deliberately unrelated to the secrets under test.
        std::env::set_var("SHUNT_GATEWAY_USERS", "dev@example.com:approval-secret");

        // Clearly-fake literals, each over the 32-byte floor. No `SHUNT_*` env
        // var may hold these same values, or `never_literal` would suppress
        // the warning by design.
        let scalar = "literal-session-secret-0123456789abcdef";
        let rotated_new = "literal-rotated-new-0123456789abcdef";
        let rotated_old = "literal-rotated-old-0123456789abcdef";

        let load_logs = |session_body: &str| -> String {
            let dir = std::env::temp_dir().join(format!(
                "shunt-config-test-session-literal-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("shunt.toml");
            std::fs::write(
                &path,
                format!(
                    "[server]\ndefault_provider = \"anthropic\"\n\n\
                     [server.gateway]\npublic_url = \"https://gateway.example.com\"\n\n\
                     [server.gateway.session]\n{session_body}\n"
                ),
            )
            .unwrap();

            let output = Arc::new(Mutex::new(Vec::new()));
            let writer_output = Arc::clone(&output);
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || BufferWriter {
                    buffer: Arc::clone(&writer_output),
                })
                .with_ansi(false)
                .without_time()
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                Config::load(Some(&path)).unwrap();
            });
            let _ = std::fs::remove_dir_all(dir);
            let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            logs
        };

        let logs = load_logs(&format!("jwt_secret = \"{scalar}\""));
        assert_eq!(
            logs.matches("[server.gateway.session].jwt_secret").count(),
            1,
            "the bare-string form must be attributed by path exactly once: {logs}"
        );
        assert!(
            !logs.contains("could not be attributed"),
            "the bare-string form must not fall back to an unattributed count: {logs}"
        );

        let logs = load_logs(&format!(
            "jwt_secret = [\"{rotated_new}\", \"{rotated_old}\"]"
        ));
        assert_eq!(
            logs.matches("[server.gateway.session.jwt_secret].0")
                .count(),
            1,
            "array element 0 must be attributed by path exactly once: {logs}"
        );
        assert_eq!(
            logs.matches("[server.gateway.session.jwt_secret].1")
                .count(),
            1,
            "array element 1 must be attributed by path exactly once: {logs}"
        );
        assert!(
            !logs.contains("could not be attributed"),
            "the array form must not fall back to an unattributed count: {logs}"
        );

        std::env::remove_var("SHUNT_GATEWAY_USERS");
    }

    // Default-propagation coverage for issue #286/#289: `Config::load` seeds
    // figment with `Serialized::defaults(Self::default())`, so a mismatch
    // between the serde `#[serde(default)]` on `ProviderConfig::tool_search`
    // (an unset `Option<bool>`, "auto") and a stray literal in
    // `Config::default()` would only surface here, not in a unit test that
    // builds `ProviderConfig`/`Config` directly in Rust.
    #[test]
    fn legacy_provider_custom_host_tool_search_defaults_to_shim() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-tool-search-default-{}",
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
[providers.custom]
kind = "responses"
base_url = "https://api.custom-openai.example/v1"
auth = "api_key"
api_key_env = "CUSTOM_API_KEY"
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();

        // No `tool_search` key declared for a brand-new, user-declared
        // provider on a host shunt has never verified — auto must keep the
        // #43 shim rather than silently trusting an unknown Responses
        // backend to implement `tool_search` items (issue #289).
        assert!(!config.native_tool_search("custom", "gpt-5.6-sol"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_provider_custom_host_tool_search_true_opts_into_native() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-tool-search-custom-opt-in-{}",
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
[providers.custom]
kind = "responses"
base_url = "https://api.custom-openai.example/v1"
auth = "api_key"
api_key_env = "CUSTOM_API_KEY"
tool_search = true
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();

        // Explicit opt-in reaches a non-known host too — the operator has
        // verified their own backend implements `tool_search` items.
        assert!(config.native_tool_search("custom", "gpt-5.6-sol"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_provider_on_known_openai_host_defaults_on() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-tool-search-known-host-{}",
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
[providers.custom]
kind = "responses"
base_url = "https://api.openai.com/v1"
auth = "api_key"
api_key_env = "CUSTOM_API_KEY"
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();

        // A user-declared provider name still auto-resolves to native as
        // long as its `base_url` host is exactly `api.openai.com` — the rule
        // reads the host, not the provider name.
        assert!(config.native_tool_search("custom", "gpt-5.6-sol"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_provider_tool_search_false_opts_out() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "shunt-config-test-tool-search-opt-out-{}",
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
[providers.custom]
kind = "responses"
base_url = "https://api.custom-openai.example/v1"
auth = "api_key"
api_key_env = "CUSTOM_API_KEY"
tool_search = false
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();

        // Explicit opt-out reaches a user-declared provider too.
        assert!(!config.native_tool_search("custom", "gpt-5.6-sol"));

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
        let _env_guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    api_key_env: MOONSHOT_API_KEY
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
        assert_eq!(kimi.api_key_env.as_deref(), Some("MOONSHOT_API_KEY"));
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
