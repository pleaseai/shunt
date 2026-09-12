//! `[models.stage_router]` — opt-in, content-aware tier selection for one
//! advertised model id.
//!
//! A `[[models]]` entry normally names a fixed destination. An entry carrying
//! this table instead names *two* destinations — a capable tier and an efficient
//! one — and lets the request's recent tool-result history decide which serves
//! the turn. The chosen target is itself a public model id, so it resolves
//! through the ordinary routing ladder and inherits failover chains, account
//! pools, adapter selection, `effort`, and `service_tier` unchanged.
//!
//! Policy only: the table holds no credentials, so a derived `Debug` cannot leak
//! one. Absent this table a `[[models]]` entry behaves exactly as before.

use serde::{Deserialize, Serialize};

/// Which tier serves a turn whose signals are too weak to decide.
///
/// The scorer reports low confidence far more often than it reports a wrong
/// answer, so this default — not the score — governs most early turns in a
/// session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageRouterPicker {
    /// Start efficient and escalate only when signals support capable.
    #[default]
    EfficientFirst,
    /// Start capable and de-escalate only when signals support efficient.
    CapableFirst,
}

/// `[models.stage_router]`.
///
/// Thresholds are deliberately asymmetric. Escalating costs one forfeited
/// prompt-cache prefix; staying on the efficient tier through a turn it cannot
/// handle costs a wasted turn *and* the escalation afterwards. So
/// `confidence_threshold` gates the way up and the stricter
/// `deescalate_threshold` gates the way down.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StageRouterConfig {
    /// Model id serving hard reasoning, investigation, and error recovery.
    pub capable_target: String,
    /// Model id serving routine work once the plan is settled.
    pub efficient_target: String,
    #[serde(default)]
    pub picker: StageRouterPicker,
    /// Minimum scorer confidence to act on a signal-driven decision.
    #[serde(default = "default_confidence_threshold")]
    pub confidence_threshold: f64,
    /// How many recent turns of tool results feed the scorer.
    #[serde(default = "default_recent_turn_window")]
    pub recent_turn_window: usize,
    /// Turns the capable tier is held before a de-escalation may fire.
    #[serde(default = "default_min_dwell_turns")]
    pub min_dwell_turns: u32,
    /// Confidence required to move *down* to the efficient tier. Defaults to
    /// [`DEFAULT_DEESCALATE_THRESHOLD`], which is stricter than the escalation
    /// threshold; read it through [`StageRouterConfig::deescalate_threshold`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deescalate_threshold: Option<f64>,
    /// How long a session's pinned tier survives without traffic.
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
}

/// Matches the stage router's published calibration: one saturated signal scores
/// `tanh(0.5) ≈ 0.46` and so falls short on its own, while two corroborating
/// signals reach `tanh(1.0) ≈ 0.76` and clear this bar.
pub const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.5;
/// Strictly above [`DEFAULT_CONFIDENCE_THRESHOLD`]: a tier flip forfeits the
/// warm prompt-cache prefix and any `previous_response_id` continuation, so
/// coming back down must clear a higher bar than going up.
pub const DEFAULT_DEESCALATE_THRESHOLD: f64 = 0.75;

fn default_confidence_threshold() -> f64 {
    DEFAULT_CONFIDENCE_THRESHOLD
}

fn default_recent_turn_window() -> usize {
    3
}

fn default_min_dwell_turns() -> u32 {
    3
}

fn default_session_ttl_seconds() -> u64 {
    3600
}

impl StageRouterConfig {
    /// The effective de-escalation threshold, applying the default when the
    /// operator left the key out.
    pub fn deescalate_threshold(&self) -> f64 {
        self.deescalate_threshold
            .unwrap_or(DEFAULT_DEESCALATE_THRESHOLD)
    }

    /// Both configured targets, in the order they are declared.
    pub fn targets(&self) -> [&str; 2] {
        [&self.capable_target, &self.efficient_target]
    }
}
