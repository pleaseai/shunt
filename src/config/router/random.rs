//! `[models.router] type = "random"` — a weighted split across public model ids.
//!
//! Upstream's `random` algorithm draws a fresh arm per request. shunt adds one
//! key, `affinity`, because the client this gateway serves is a conversation:
//! a Claude Code session that lands on a different arm mid-session forfeits its
//! prompt-cache prefix, its pooled socket, and its sticky account slot on every
//! turn. Under the default `session` affinity the arm is a pure hash of
//! `(seed, model id, session id)`, so it survives a restart and is the same on
//! every replica loading the same config — and the session's `count_tokens`
//! probes land where the turn does.
//!
//! Policy only: no credentials live here, so a derived `Debug` cannot leak one.

use serde::{Deserialize, Serialize};

/// Whether an arm is pinned for a session or drawn afresh per request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RandomAffinity {
    /// One arm per `x-claude-code-session-id`, by hash. A request carrying no
    /// non-blank session id still takes a fresh weighted draw — the blank id is
    /// never hashed as a shared key, which would collapse every sessionless
    /// caller onto one arm and turn a 90/10 split into 100/0 for them.
    #[default]
    Session,
    /// Upstream's meaning: every request draws.
    Request,
}

/// `[models.router]` with `type = "random"`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RandomRouterConfig {
    /// The public model ids this entry splits across, in declared order.
    pub targets: Vec<String>,
    /// Relative weights, one per target in the same order. Absent means equal
    /// weights. A zero disables that target without renumbering the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<Vec<f64>>,
    /// Upstream's `random` seed. Under `affinity = "request"` it seeds the
    /// per-router RNG so the selection sequence reproduces; under
    /// `affinity = "session"` it is the hash salt, and `0` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default)]
    pub affinity: RandomAffinity,
}

impl RandomRouterConfig {
    /// The weight of the target at `index`, defaulting to `1.0` when the
    /// operator declared none. Out-of-range indices weigh nothing, which cannot
    /// happen after validation but keeps this total.
    pub fn weight(&self, index: usize) -> f64 {
        match self.weights.as_ref() {
            Some(weights) => weights.get(index).copied().unwrap_or(0.0),
            None if index < self.targets.len() => 1.0,
            None => 0.0,
        }
    }

    /// Targets with a positive weight, in declared order. A zero-weight target
    /// is configured but disabled, so it is never drawn and never the
    /// deterministic body-less answer.
    pub fn enabled(&self) -> impl Iterator<Item = (usize, &str)> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(index, _)| self.weight(*index) > 0.0)
            .map(|(index, target)| (index, target.as_str()))
    }
}
