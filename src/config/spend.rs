//! `[server.spend]` — spend-limit policy for the admin spend API and (from the
//! next stage) enforcement on inference requests.
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

/// How a limit is chosen when several group limits apply to one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupLimitMode {
    #[default]
    Min,
    Max,
}

/// `[server.spend]`. Stage 1 deserializes the retention fields without range
/// validation and validates `group_limit_mode` against its enum, but does not
/// yet run retention sweeps or resolve group limits.
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

/// `[server.spend.enforcement]` — how the (not yet implemented) enforcement
/// path behaves when the spend meter itself errors.
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
