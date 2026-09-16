//! Benchmark-only entry points into the stage router (issue #554).
//!
//! `benches/` compile as separate crates against the library, so they reach
//! only `pub` items — and the whole stage-router path is `pub(crate)`:
//! [`crate::routing::resolve_request_chain_value`], `StageContext`,
//! `StageRouterStore`, and `signals::extract`. The two public routing entry
//! points ([`crate::routing::resolve`] and [`crate::routing::resolve_model`])
//! both pass `stage: None`, so no benchmark could reach a live routing decision
//! at all. That is why the planned `stage_router_resolve` benchmark never
//! shipped.
//!
//! This module is a **facade, not a widening**. It exposes benchmark-shaped
//! functions rather than the private types behind them, so `StageContext`'s
//! `Cell` write/read protocol, `PendingPin`, and `StageDecision` stay
//! crate-private in every build — including this one. It is gated on
//! `--features bench`, so a normal build's public API is byte-for-byte what it
//! was before.
//!
//! Nothing here may add work the production path does not do: a benchmark that
//! measures its own harness measures nothing. Each function below is the
//! shortest path from a caller's inputs to the crate-private call it exists to
//! expose.

use std::cell::Cell;
use std::time::Instant;

use serde_json::Value;

pub use switchyard_libsy::ToolSignals;

use crate::config::{Config, StageRouterConfig};
use crate::error::ShuntError;
use crate::routing::stage::store::MAX_TRACKED_SESSIONS as STORE_CAP;
use crate::routing::stage::{self, signals, StageContext, StageRouterStore};
use crate::routing::{self, Route};

/// The session cap `StageRouterStore` evicts against — the point past which
/// eviction stops being O(1) (issue #552).
pub const MAX_TRACKED_SESSIONS: usize = STORE_CAP;

/// Extract tool-activity signals from a request's `messages` array.
///
/// The subject of issue #553: pass 2 walks the whole history on every turn, so
/// a session's total extraction cost is quadratic in its turn count even though
/// each individual call is linear.
pub fn extract_signals(messages: &Value, recent_turn_window: usize) -> Option<ToolSignals> {
    signals::extract(messages, recent_turn_window)
}

/// Per-session tier pins, as they live on `AppState`.
#[derive(Default)]
pub struct StageStore(StageRouterStore);

impl StageStore {
    pub fn new() -> Self {
        Self(StageRouterStore::new())
    }

    /// Drive one turn's hysteresis with no conversation to score: `apply`
    /// followed by the `commit` that admission triggers.
    ///
    /// Scoring is skipped deliberately — the estimate is the picker default,
    /// which [`stage::decide`] reaches without reading any `messages` — so what
    /// this measures is the mutex-held get/resolve/insert/evict of issue #552
    /// and nothing else. Use [`resolve_chain`] for the whole request path.
    pub fn turn(
        &self,
        model: &str,
        session_id: &str,
        router: &StageRouterConfig,
        now: Instant,
    ) -> bool {
        let estimate = stage::decide(router, None);
        let applied = self
            .0
            .apply(model, Some(session_id), router, estimate, false, now);
        applied
            .pin
            .is_some_and(|pin| self.0.commit(pin, now).is_some())
    }
}

/// Resolve one live request through the stage router, exactly as
/// `src/proxy/failover.rs` does: score the conversation, apply hysteresis, then
/// commit the pin once the request is admitted.
///
/// This is the only path that reaches `resolve_request_chain_value` with a
/// `StageContext`, and so the only one that measures what a router-backed
/// request actually pays.
pub fn resolve_chain(
    config: &Config,
    store: &StageStore,
    request: &Value,
    session_id: Option<&str>,
    read_only: bool,
    now: Instant,
) -> Result<Vec<Route>, ShuntError> {
    let stage = StageContext {
        store: &store.0,
        request,
        session_id,
        read_only,
        now,
        pending: Cell::new(None),
        decided: Cell::new(None),
    };
    let (routes, _model) = routing::resolve_request_chain_value(config, request, Some(&stage))?;
    stage.commit();
    Ok(routes)
}
