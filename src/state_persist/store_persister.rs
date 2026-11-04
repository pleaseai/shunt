//! The store-backed half of pool persistence (write-behind class).
//!
//! Same protocol as the file persister: a timer claims the pool's dirty flag
//! and writes the *current* export, so a failed write is retried with whatever
//! the pool holds on the next tick rather than with the failed snapshot. Nothing
//! on the proxy path awaits any of this; the pool serves from memory throughout.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::task::JoinHandle;

use super::{flush, restore_bindings};
use crate::{
    accounts::{AccountKey, AccountPool, BindingRow, QuotaState},
    server::AppState,
    store::{pool_repo, pool_session_repo, Store, StoreError},
};

/// Restore pool quota from the store at boot. Fail-loud: a store that cannot be
/// read returns its error and the caller decides whether to boot without it.
pub async fn restore_from_store(state: &AppState, store: &Store) -> Result<(), StoreError> {
    let quotas = pool_repo::load_pool(store).await?;
    let count = quotas.len();
    if state.accounts.import_quotas(quotas) {
        state.accounts.mark_dirty();
    }
    // Same filter as the file path: expired, unconfigured and malformed rows
    // are dropped (and the pool marked dirty so the next flush deletes them),
    // the cap holds, and `any_bindings` is raised.
    let (rows, dropped) = pool_session_repo::load_bindings(store).await?;
    if dropped {
        state.accounts.mark_dirty();
    }
    let bindings = restore_bindings(state, rows).await;
    tracing::info!(
        accounts = count,
        bindings,
        "restored pool quota state from the store"
    );
    Ok(())
}

/// The store a persister writes to, plus what it needs to flush safely.
struct StoreSink {
    store: Arc<Store>,
    /// Serializes flushes so an older export can never land after a newer one,
    /// and holds the binding rows last written (`None` until the first flush,
    /// which reads them from the store), so a flush writes only the bindings
    /// that changed.
    gate: tokio::sync::Mutex<Option<pool_session_repo::Snapshot>>,
    /// Set once the instance hold is lost; flushing stops for good.
    halted: AtomicBool,
}

impl StoreSink {
    fn halted(&self) -> bool {
        self.halted.load(Ordering::SeqCst)
    }

    /// Write the pool if it changed since the last flush. A failure counts on
    /// `shunt.store.write_failures{subsystem="pool"}` and re-marks the pool
    /// dirty, except `HoldLost`, which halts flushing instead of spinning.
    async fn flush(&self, accounts: &AccountPool) -> Result<(), StoreError> {
        let mut written = self.gate.lock().await;
        if self.halted() {
            return Err(StoreError::HoldLost {
                target: self.store.target().to_string(),
            });
        }
        if !accounts.take_dirty() {
            return Ok(());
        }
        let exported = accounts.export_quotas();
        let bindings = accounts.export_bindings();
        let Err(error) = self.write(&exported, &bindings, &mut written).await else {
            return Ok(());
        };
        crate::metrics::record_store_write_failure("pool");
        if matches!(error, StoreError::HoldLost { .. }) {
            self.halted.store(true, Ordering::SeqCst);
            tracing::error!(%error, "pool state persistence stopped: this instance lost the store hold");
        } else {
            accounts.mark_dirty();
            tracing::warn!(%error, "failed to persist pool state to the store; will retry");
        }
        Err(error)
    }
}

impl StoreSink {
    /// Write the quota export and the binding delta in one durable
    /// transaction. `written` becomes the new binding snapshot only once the
    /// transaction commits; on a failure it keeps what the store still holds.
    async fn write(
        &self,
        quotas: &[(AccountKey, QuotaState)],
        bindings: &[BindingRow],
        written: &mut Option<pool_session_repo::Snapshot>,
    ) -> Result<(), StoreError> {
        let current = pool_session_repo::snapshot_of(bindings)?;
        let mut tx = self.store.begin_durable().await?;
        let previous = match written.take() {
            Some(previous) => Ok(previous),
            None => pool_session_repo::read_snapshot(&mut tx).await,
        };
        let result = match previous {
            Ok(previous) => {
                let diff = pool_session_repo::diff(&previous, &current);
                *written = Some(previous);
                match pool_repo::write_pool(&mut tx, quotas).await {
                    Ok(()) => pool_session_repo::apply_diff(&mut tx, &diff).await,
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                tx.commit().await?;
                *written = Some(current);
                Ok(())
            }
            Err(error) => {
                let _ = tx.rollback().await;
                Err(error)
            }
        }
    }
}

/// Handle to the pool's background persister. Dropping it leaves the task
/// running (the default deployment never stops it); [`PoolPersister::stop`]
/// ends it.
pub struct PoolPersister {
    state: AppState,
    sink: Option<Arc<StoreSink>>,
    task: Option<JoinHandle<()>>,
}

impl PoolPersister {
    /// A persister that does nothing (no file path, no store).
    pub(super) fn idle(state: AppState) -> Self {
        Self {
            state,
            sink: None,
            task: None,
        }
    }

    pub(super) fn file(state: AppState, task: JoinHandle<()>) -> Self {
        Self {
            state,
            sink: None,
            task: Some(task),
        }
    }

    pub(super) fn store(state: AppState, store: Arc<Store>, interval: Duration) -> Self {
        let sink = Arc::new(StoreSink {
            store,
            gate: tokio::sync::Mutex::new(None),
            halted: AtomicBool::new(false),
        });
        let task = tokio::spawn(store_loop(sink.clone(), state.clone(), interval));
        Self {
            state,
            sink: Some(sink),
            task: Some(task),
        }
    }

    /// Write any pending quota now, for graceful shutdown. With a store this
    /// also writes any pending gateway-session diff (a revocation a failed
    /// token write left in memory) and returns the first error; the file
    /// persister logs its own failures and returns `Ok`.
    pub async fn flush_now(&self) -> Result<(), StoreError> {
        match &self.sink {
            Some(sink) => {
                let pool = sink.flush(&self.state.accounts).await;
                let sessions = crate::gateway::durable::flush_pending(&self.state).await;
                pool.and(sessions)
            }
            None => {
                flush(&self.state).await;
                Ok(())
            }
        }
    }

    /// Stop the background task and wait for it to end. In-flight writes finish
    /// or roll back with their transaction; call [`Self::flush_now`] first to
    /// write pending state.
    pub async fn stop(mut self) {
        self.stop_task().await;
    }

    /// Graceful shutdown in the order the store needs: end the timer first so
    /// no tick races the final write, then [`Self::flush_now`] once.
    pub async fn stop_and_flush(mut self) -> Result<(), StoreError> {
        self.stop_task().await;
        // The abort may have landed inside a flush that had already claimed the
        // dirty flag; re-arm it so the final write is not skipped.
        self.state.accounts.mark_dirty();
        self.flush_now().await
    }

    async fn stop_task(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn store_loop(sink: Arc<StoreSink>, state: AppState, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; there is nothing new to write at t=0.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let _ = sink.flush(&state.accounts).await;
        // Pending gateway-session writes ride the same tick (15 s): a
        // revocation kept in memory through a store outage reaches the store
        // without waiting for another token request. A no-op when no gateway
        // store is attached or nothing differs.
        let sessions = crate::gateway::durable::flush_pending(&state).await;
        if matches!(sessions, Err(StoreError::HoldLost { .. })) && !sink.halted() {
            // A gateway-only change leaves the pool clean, so the pool flush
            // above never sees the lost hold; halt here instead of retrying a
            // fenced write every tick.
            sink.halted.store(true, Ordering::SeqCst);
            tracing::error!("store persistence stopped: this instance lost the store hold");
        }
        if sink.halted() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod binding_tests;
