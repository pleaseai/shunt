//! Boot and graceful shutdown of the optional store (ADR-0006).
//!
//! [`start`] runs once in `serve`, after the router is built and before it
//! serves: open (preflight, hold, migrate) → start the hold heartbeat → import
//! the JSON snapshots once → attach the store to the gateway bundle → restore
//! pool, gateway sessions and spend from it → confirm the hold is still ours →
//! spawn the pool store persister. With no `[store]` it returns `None` and
//! `serve` keeps its file-based path.
//!
//! [`StoreRuntime::shutdown`] runs after the HTTP drain: stop the background
//! tasks → write pending state once → release the hold → close.

use std::sync::Arc;
use std::time::Duration;

use super::import::{import_all, ImportReport, ImportSources, Source};
use super::{HeartbeatHandle, Mode, Store, StoreError};
use crate::config::Config;
use crate::server::AppState;
use crate::state_persist::{self, PoolPersister};

/// The running store: its handle plus the two background tasks it owns.
pub struct StoreRuntime {
    store: Arc<Store>,
    heartbeat: HeartbeatHandle,
    persister: PoolPersister,
}

/// Open the store and make it the source of every store-backed subsystem.
/// `None` when `config` has no `[store]`. On any failure after the open, the
/// hold is released and the store closed before the error is returned, so a
/// corrected restart does not wait out the stale window.
pub async fn start(config: &Config, state: &AppState) -> Result<Option<StoreRuntime>, StoreError> {
    let Some(store_config) = config.store.as_deref() else {
        return Ok(None);
    };
    let store = Arc::new(
        Store::open(store_config, Mode::Serve)
            .await
            .map_err(|error| error.located(&store_config.sanitized_target()))?,
    );
    // Renew the hold through the imports and restores: one that outlasts
    // `hold_stale_seconds` must not let another process take the store while
    // this one is still starting.
    let heartbeat = store.spawn_heartbeat();
    let prepared = match prepare(config, state, &store).await {
        Ok(report) => confirm_hold(&store).await.map(|()| report),
        Err(error) => Err(error),
    };
    let report = match prepared {
        Ok(report) => report,
        Err(error) => {
            heartbeat.stop().await;
            release_and_close(&store).await;
            return Err(error.located(store.target()));
        }
    };
    tracing::info!(
        target: "shunt::store",
        backend = ?store_config.backend,
        store = %store.target(),
        holder = %store.holder_label(),
        pool = ?report.pool,
        gateway = ?report.gateway,
        spend = ?report.spend,
        "store opened; state is served from it"
    );
    let persister = state_persist::spawn_state_persister(state.clone(), Some(store.clone()));
    Ok(Some(StoreRuntime {
        store,
        heartbeat,
        persister,
    }))
}

/// Import, attach, restore. Gateway and spend restore only when they are
/// store-backed, by the same rule the import uses.
async fn prepare(
    config: &Config,
    state: &AppState,
    store: &Arc<Store>,
) -> Result<ImportReport, StoreError> {
    let sources = ImportSources::from_config(config);
    let report = import_all(store, &sources).await?;
    let attached = state.gateway_stores.attach_store(store.clone());
    debug_assert!(attached, "the store is attached once, at boot");
    state_persist::restore_from_store(state, store).await?;
    if matches!(sources.gateway, Source::On { .. }) {
        crate::gateway::durable::restore_from_store(state).await?;
    }
    if matches!(sources.spend, Source::On { .. }) {
        crate::gateway::spend::persist::restore(state)
            .await
            .map_err(|error| StoreError::Database {
                reason: format!("restoring gateway spend-limit state: {error}"),
            })?;
    }
    Ok(report)
}

/// Renew the hold once more before serving: a takeover the heartbeat has not
/// seen yet would otherwise surface only as `HoldLost` on the first write.
async fn confirm_hold(store: &Store) -> Result<(), StoreError> {
    if store.is_fenced() || !store.heartbeat_once().await? {
        return Err(StoreError::HoldLost {
            target: store.target().to_string(),
        });
    }
    Ok(())
}

async fn release_and_close(store: &Store) {
    if let Err(error) = store.release_hold().await {
        tracing::warn!(target: "shunt::store", %error, "failed to release the store hold");
    }
    store.close().await;
}

impl StoreRuntime {
    /// Stop the heartbeat and persister, write pending pool state and gateway
    /// revocations once, release the hold, and close, all within `timeout`.
    /// A failed final write is logged and the hold is still released. Past
    /// the deadline the remaining steps are abandoned (logged); the hold then
    /// goes stale on its own.
    pub async fn shutdown(self, timeout: Duration) {
        if tokio::time::timeout(timeout, self.shutdown_in_order())
            .await
            .is_err()
        {
            tracing::warn!(
                target: "shunt::store",
                timeout_seconds = timeout.as_secs(),
                "store shutdown did not finish in time; the hold expires on its own"
            );
        }
    }

    async fn shutdown_in_order(self) {
        self.heartbeat.stop().await;
        if let Err(error) = self.persister.stop_and_flush().await {
            tracing::warn!(target: "shunt::store", %error, "final store flush failed at shutdown");
        }
        release_and_close(&self.store).await;
        tracing::info!(target: "shunt::store", "store hold released and store closed");
    }
}

#[cfg(test)]
mod tests;
