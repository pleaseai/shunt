//! `[models.router] type = "prefill_router"` — the driven lane (ADR-0005 §6).
//!
//! Upstream's learned router is not a scorer shunt can call synchronously the
//! way `stage` calls `pick_tier`: it is a whole libsy [`Algorithm`] that runs
//! encoder inference on a blocking worker and keeps its own user-turn affinity.
//! So the decision is made *before* resolution, in `proxy::failover`, and
//! parked on the [`StageContext`](crate::routing::stage::StageContext) for the
//! synchronous `resolve_chain` to read. That split is why
//! [`crate::routing::outcome::PrefillDecision`] exists at all.
//!
//! The surface below is identical in both builds, so no call site carries a
//! `cfg`. Without the `prefill-router` feature [`PrefillRouters`] holds
//! nothing, [`PrefillRouters::build`] cannot fail (validation already refused
//! every such entry by name), and [`decide`] is a constant `None` — the
//! `resolve_chain` arm then answers from the config's first target, exactly as
//! a body-less resolution does.
//!
//! The feature-on internals live in [`driven`], which is the only module that
//! names an upstream type, so this file compiles identically either way.

#[cfg(feature = "prefill-router")]
mod driven;

#[cfg(all(test, feature = "prefill-router"))]
mod tests;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::config::{Config, ConfigError};
use crate::routing::outcome::PrefillDecision;

/// The built upstream algorithms, one per `[[models]]` entry with
/// `type = "prefill_router"`, keyed by the entry's model id.
///
/// Empty (and free) in a build without the feature: the struct has no fields
/// at all there, so a deployment that never opted in carries no map, no
/// allocation, and no lookup.
pub struct PrefillRouters {
    #[cfg(feature = "prefill-router")]
    by_model: std::collections::HashMap<String, driven::Entry>,
}

impl PrefillRouters {
    /// Build every prefill entry in `config`.
    ///
    /// Loading a checkpoint is the expensive, fallible part, and it happens
    /// here — once per `RuntimeState`, not per request. Without the feature
    /// this is a no-op that cannot fail: `Config::validate` has already
    /// rejected any such entry with
    /// [`ConfigError::PrefillRouterFeatureDisabled`], so reaching this with one
    /// configured is not possible.
    pub fn build(config: &Config) -> Result<Self, ConfigError> {
        #[cfg(feature = "prefill-router")]
        {
            Ok(Self {
                by_model: driven::build(config)?,
            })
        }
        #[cfg(not(feature = "prefill-router"))]
        {
            let _ = config;
            Ok(Self {})
        }
    }

    /// Whether any entry was built — always true of a build without the
    /// feature.
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "prefill-router")]
        {
            self.by_model.is_empty()
        }
        #[cfg(not(feature = "prefill-router"))]
        {
            true
        }
    }

    /// Build a store around an already-constructed algorithm, so a test can
    /// drive [`decide`] against a stub rather than a checkpoint — the whole
    /// point being that everything below the algorithm boundary (the request
    /// shape, the metadata, the fail-open) is shunt's and must be testable
    /// without Python on the machine.
    #[cfg(all(test, feature = "prefill-router"))]
    pub(crate) fn from_algorithm(
        model: &str,
        targets: &[&str],
        algorithm: std::sync::Arc<dyn switchyard_libsy::Algorithm>,
    ) -> Self {
        Self {
            by_model: std::collections::HashMap::from([(
                model.to_string(),
                driven::Entry::new(algorithm, targets),
            )]),
        }
    }
}

/// Drive the request through libsy when its `model` names a prefill entry;
/// `None` otherwise — and always `None` without the feature.
///
/// Every request is driven, `count_tokens` probes included and with no
/// `read_only` special case. Upstream's affinity is what makes that cheap: a
/// tool continuation replays the decision the turn's user message already
/// earned instead of re-running inference, and a probe sent for a turn is part
/// of that same turn. Skipping the probe would instead route it by the
/// default target while the turn it measures went somewhere else.
pub(crate) async fn decide(
    routers: &PrefillRouters,
    body: &Value,
    headers: &HeaderMap,
) -> Option<PrefillDecision> {
    #[cfg(feature = "prefill-router")]
    {
        driven::decide(&routers.by_model, body, headers).await
    }
    #[cfg(not(feature = "prefill-router"))]
    {
        let _ = (routers, body, headers);
        None
    }
}

// Re-exported for the two consumers that reach it by name: this module's unit
// tests and, through `bench_support`, `benches/stage_router.rs`. Gated on those
// two builds rather than on the feature alone — the production lane calls
// `driven::messages_from_body` inside `driven`, so a plain
// `--features prefill-router` build has no consumer for the re-export and
// `-D warnings` turns the resulting `unused import` into a build failure.
#[cfg(all(feature = "prefill-router", any(test, feature = "bench")))]
pub use driven::messages_from_body;
