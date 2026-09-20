//! The six per-call bounds a driven `[models.router]` runs its internal model
//! calls under (ADR-0005 §3, issue #594).
//!
//! What this module guards is that a judge call can never become unbounded work
//! charged to a client turn. A driven router spends the *gateway's* credential
//! and the *target's* pool quota on a call the caller never asked for, so every
//! one of them is fenced: `judge_timeout_ms` end to end (headers **and** body —
//! a `200` that then stalls must still fail open), `judge_max_response_bytes`
//! on what is collected, three `gated_*` bounds on a retained turn (PR 6), and
//! `max_judge_calls` on how many calls one session may make at all.
//!
//! The bounds live on the wire as six flat `u64`/`usize` keys on
//! [`super::StageRouterConfig`] rather than a nested table, because serde
//! forbids `flatten` next to `deny_unknown_fields` and the stage table needs
//! the latter. [`CallBounds`] is the read side: one `Copy` value, resolved
//! once, carrying `Duration`s instead of raw milliseconds so no caller has to
//! remember the unit.

use std::time::Duration;

use crate::config::ConfigError;

use super::StageRouterConfig;

/// End-to-end deadline on one judge call, headers and body.
pub const DEFAULT_JUDGE_TIMEOUT_MS: u64 = 30_000;
/// Cap on the bytes collected from a judge reply.
pub const DEFAULT_JUDGE_MAX_RESPONSE_BYTES: usize = 65_536;
/// Cap on the bytes retained from a gated turn (PR 6).
pub const DEFAULT_GATED_MAX_BYTES: usize = 8_388_608;
/// Idle gap allowed between two body chunks of a gated turn (PR 6).
pub const DEFAULT_GATED_IDLE_MS: u64 = 60_000;
/// Wall-clock ceiling on one gated turn (PR 6).
pub const DEFAULT_GATED_MAX_DURATION_MS: u64 = 600_000;
/// Judge calls one session may make under one pin.
pub const DEFAULT_MAX_JUDGE_CALLS: u32 = 8;

/// The six bounds, resolved from config into the units the call path uses.
///
/// `Copy` and field-private-free on purpose: it is read on the request path by
/// three modules (`routing::judge`, `routing::serve`, `proxy::failover`) and
/// carries no allocation, so passing it by value costs nothing and no caller
/// needs a borrow of the config to hold it across an `.await`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallBounds {
    /// End-to-end deadline on one judge call. Applied around the whole
    /// dispatch-and-collect future, never as a transport `.send()` timeout —
    /// that one stops at headers and would let a `200` stall forever.
    pub judge_timeout: Duration,
    /// Bytes collected from a judge reply before it is refused as oversized.
    pub judge_max_response_bytes: usize,
    /// Bytes retained from a gated turn (PR 6).
    pub gated_max_bytes: usize,
    /// Gap allowed between two body chunks of a gated turn (PR 6). SSE ping
    /// frames do not reset it.
    pub gated_idle: Duration,
    /// Wall-clock ceiling on one gated turn (PR 6).
    pub gated_max_duration: Duration,
    /// Judge calls one session may make under one pin.
    pub max_judge_calls: u32,
}

impl CallBounds {
    /// Reject any bound written as `0`.
    ///
    /// Zero is refused rather than read as "unlimited" for all six: a zero
    /// timeout or idle gap would fail every call before it started, and a zero
    /// byte cap or call budget would make the feature inert while looking
    /// configured. `keys` is the `(key, value)` pairs as the operator wrote
    /// them, so the rejection quotes the key rather than a resolved field name;
    /// the caller builds it from the config struct, which keeps this function
    /// free of any unit conversion of its own.
    pub fn validate(model_id: &str, keys: [(&'static str, u64); 6]) -> Result<(), ConfigError> {
        for (key, value) in keys {
            if value == 0 {
                return Err(ConfigError::ZeroCallBound {
                    model: model_id.to_string(),
                    key,
                });
            }
        }
        Ok(())
    }
}

impl StageRouterConfig {
    /// The six bounds this table's internal calls run under.
    pub fn bounds(&self) -> CallBounds {
        CallBounds {
            judge_timeout: Duration::from_millis(self.judge_timeout_ms),
            judge_max_response_bytes: self.judge_max_response_bytes,
            gated_max_bytes: self.gated_max_bytes,
            gated_idle: Duration::from_millis(self.gated_idle_ms),
            gated_max_duration: Duration::from_millis(self.gated_max_duration_ms),
            max_judge_calls: self.max_judge_calls,
        }
    }

    /// The same six, paired with the config keys that wrote them, for
    /// [`CallBounds::validate`].
    pub fn bound_keys(&self) -> [(&'static str, u64); 6] {
        [
            ("judge_timeout_ms", self.judge_timeout_ms),
            (
                "judge_max_response_bytes",
                self.judge_max_response_bytes as u64,
            ),
            ("gated_max_bytes", self.gated_max_bytes as u64),
            ("gated_idle_ms", self.gated_idle_ms),
            ("gated_max_duration_ms", self.gated_max_duration_ms),
            ("max_judge_calls", u64::from(self.max_judge_calls)),
        ]
    }
}
