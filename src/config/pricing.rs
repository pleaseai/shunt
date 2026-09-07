//! `[server.spend.pricing]` — the USD list-price adjustments the spend meter
//! will price requests with.
//!
//! Policy only, like the rest of `[server.spend]`: a multiplier applied to
//! every priced request plus optional per-upstream/model override rates for
//! deployments whose real price differs from Anthropic's published list (a
//! resold Bedrock capacity pool, a negotiated discount). The rates live here;
//! `crate::gateway::spend::pricing` turns them into a lookup table.

use serde::{Deserialize, Serialize};

/// `[server.spend.pricing]`. Absent means list prices at multiplier 1.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PricingConfig {
    /// Scales every resolved rate, list price and override alike. Must be in
    /// `(0, 1]` — the block exists to model a discount, not a markup.
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<PricingOverride>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            multiplier: default_multiplier(),
            overrides: Vec::new(),
        }
    }
}

/// One `[[server.spend.pricing.overrides]]` row: the four USD-per-million-token
/// rates that replace the list price for `model` on `upstream`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PricingOverride {
    pub upstream: String,
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

fn default_multiplier() -> f64 {
    1.0
}
