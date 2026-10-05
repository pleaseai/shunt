//! `[server.spend]` — spend-limit policy for the admin spend API and for
//! enforcement and metering on `/v1/messages`.
//!
//! Policy only: the section holds no credentials. Spend endpoints authenticate
//! with the `[server.admin]` credential, so enabling spend limits no longer
//! drags in gateway login (`[server.gateway]` requires a signing secret plus
//! static users or OIDC before it will start). Everything under
//! `[server.gateway]` is downstream of a login session; spend enforcement
//! applies to `/v1/messages` for any caller, however it authenticated, which is
//! why this is a top-level section instead.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::PricingConfig;

/// How a limit is chosen when several group limits apply to one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupLimitMode {
    #[default]
    Min,
    Max,
}

/// `[server.spend]`. `blocked_message`, `enforcement.fail_closed_on_error` and
/// `spend_retention_months` are live: the refusals, the fail-closed switch and
/// the meter-counter pruning read them. The audit and identity retention days
/// are deserialized without range validation and no sweep runs yet, and
/// `group_limit_mode` is validated against its enum but no group limit is
/// resolved.
///
/// Unlike the `[server.gateway.admin]` block it replaces, this struct retains
/// no key material, so a derived `Debug` cannot leak a credential.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpendConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_message: Option<String>,
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u64,
    #[serde(default = "default_spend_retention_months")]
    pub spend_retention_months: u64,
    #[serde(default = "default_identity_retention_days")]
    pub identity_retention_days: u64,
    #[serde(default)]
    pub group_limit_mode: GroupLimitMode,
    #[serde(default = "default_spend_state_path")]
    pub state_path: Option<PathBuf>,
    #[serde(default)]
    pub enforcement: SpendEnforcementConfig,
    /// `[server.spend.pricing]`. Absent means the built-in list prices at
    /// multiplier 1; `crate::gateway::spend::pricing::PriceTable` reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<PricingConfig>,
}

impl Default for SpendConfig {
    fn default() -> Self {
        Self {
            blocked_message: None,
            audit_retention_days: default_audit_retention_days(),
            spend_retention_months: default_spend_retention_months(),
            identity_retention_days: default_identity_retention_days(),
            group_limit_mode: GroupLimitMode::default(),
            state_path: default_spend_state_path(),
            enforcement: SpendEnforcementConfig::default(),
            pricing: None,
        }
    }
}

impl SpendConfig {
    /// The configured persistence path, or `None` for memory-only state — an
    /// explicit `state_path = ""` opts out the same way the gateway session
    /// store does.
    pub fn state_path(&self) -> Option<&Path> {
        self.state_path
            .as_deref()
            .filter(|path| !path.as_os_str().is_empty())
    }
}

/// `[server.spend.enforcement]` — how `/v1/messages` admission behaves when
/// the spend meter cannot be trusted for a principal (`false` forwards the
/// request with a warning, `true` refuses it with `429` when the principal has
/// a cap; a principal with no cap is forwarded either way).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpendEnforcementConfig {
    #[serde(default)]
    pub fail_closed_on_error: bool,
}

fn default_audit_retention_days() -> u64 {
    365
}

fn default_spend_retention_months() -> u64 {
    13
}

fn default_identity_retention_days() -> u64 {
    90
}

/// `~/.shunt/gateway-spend.json` (`HOME`, falling back to `USERPROFILE` on
/// Windows), or `None` — memory-only — when neither is set. Like the gateway
/// session store this never falls back to a working-directory-relative path.
fn default_spend_state_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
        .map(|home| home.join(".shunt").join("gateway-spend.json"))
}
