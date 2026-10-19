//! The instance hold (ADR-0006 Phase 1): one shunt process per store.
//!
//! A single `instance_hold` row names the holder. Acquiring it is one
//! conditional `UPDATE` that is identical on both engines; a holder renews
//! `heartbeat_at` every `hold_stale_seconds / 3`, and a row whose heartbeat is
//! older than `hold_stale_seconds` can be taken over.
//!
//! A takeover can land while the old holder is stalled, so a heartbeat alone
//! is not a fence. Every durable write must start with
//! [`Store::begin_durable`], which re-checks `holder_id = $me` inside its own
//! transaction.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;

use super::db::{Tx, Value};
use super::error::StoreError;
use super::open::Store;

/// Same DDL as migration 0001 (which also uses `IF NOT EXISTS`).
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS instance_hold (\
    id BIGINT PRIMARY KEY CHECK (id = 1), \
    holder_id TEXT NULL, \
    holder_label TEXT NULL, \
    started_at BIGINT NULL, \
    heartbeat_at BIGINT NULL)";
const SEED_ROW: &str = "INSERT INTO instance_hold (id) VALUES (1) ON CONFLICT DO NOTHING";
const READ_ROW: &str =
    "SELECT holder_id, holder_label, heartbeat_at FROM instance_hold WHERE id = 1";
/// [`READ_ROW`] taking the row lock, for [`Store::acquire_hold`] on PostgreSQL.
#[cfg(feature = "store-postgres")]
const READ_ROW_FOR_UPDATE: &str =
    "SELECT holder_id, holder_label, heartbeat_at FROM instance_hold WHERE id = 1 FOR UPDATE";

/// How long [`Store::begin_settled`] and [`Store::acquire_hold`] wait for the
/// hold row lock on PostgreSQL. A durable transaction holds that lock only for
/// its own short write. One
/// that holds it longer is usually a backend whose client is gone (the
/// connection broke during COMMIT) and that the server has not yet reaped, and
/// waiting for it would keep the caller's lock held for just as long.
const SETTLED_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const ACQUIRE: &str = "UPDATE instance_hold SET holder_id = $1, holder_label = $2, \
    started_at = $3, heartbeat_at = $3 \
    WHERE id = 1 AND (holder_id IS NULL OR holder_id = $1 OR heartbeat_at < $4)";
const RENEW: &str = "UPDATE instance_hold SET heartbeat_at = $1 WHERE id = 1 AND holder_id = $2";
/// Scoped to the transaction (`is_local = true`), so the pooled connection
/// goes back without it.
#[cfg(feature = "store-postgres")]
const SET_LOCK_TIMEOUT: &str = "SELECT set_config('lock_timeout', $1, true)";
const RELEASE: &str = "UPDATE instance_hold SET holder_id = NULL, holder_label = NULL, \
    started_at = NULL, heartbeat_at = NULL WHERE id = 1 AND holder_id = $1";

/// Hold state shared by every clone of one [`Store`].
#[derive(Debug)]
pub(crate) struct HoldState {
    holder_id: String,
    label: String,
    stale_seconds: u64,
    fenced: AtomicBool,
    /// Read only by the PostgreSQL arms of [`Store::begin_settled`] and
    /// [`Store::acquire_hold`].
    #[cfg_attr(not(feature = "store-postgres"), allow(dead_code))]
    settled_lock_timeout_ms: AtomicU64,
}

impl HoldState {
    pub(crate) fn new(stale_seconds: u64) -> Self {
        Self {
            holder_id: uuid::Uuid::new_v4().simple().to_string(),
            label: holder_label(),
            stale_seconds,
            fenced: AtomicBool::new(false),
            settled_lock_timeout_ms: AtomicU64::new(millis(SETTLED_LOCK_TIMEOUT)),
        }
    }
}

/// The previous holder that an acquisition displaced (AC-027).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Takeover {
    pub(crate) label: String,
    pub(crate) age_secs: u64,
}

/// A running heartbeat task; it stops on drop or via [`HeartbeatHandle::stop`].
#[derive(Debug)]
pub struct HeartbeatHandle {
    task: JoinHandle<()>,
}

impl HeartbeatHandle {
    /// Stop the task and wait until it has ended.
    pub async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for HeartbeatHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Display-only `{host}:{pid}`.
fn holder_label() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(os_hostname)
        .unwrap_or_else(|| "unknown-host".to_string());
    format!("{host}:{}", std::process::id())
}

#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = vec![0u8; 256];
    // SAFETY: `buf` is writable for `buf.len()` bytes for the whole call;
    // `gethostname` writes at most that many (NUL-terminated) and returns
    // non-zero on failure without relying on the contents.
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if ret != 0 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).into_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}

impl Store {
    /// This process's hold id (random per `Store::open`).
    pub fn holder_id(&self) -> &str {
        &self.hold.holder_id
    }

    /// Display-only `{host}:{pid}` written to the hold row.
    pub fn holder_label(&self) -> &str {
        &self.hold.label
    }

    /// True once this process learned it no longer holds the store.
    pub fn is_fenced(&self) -> bool {
        self.hold.fenced.load(Ordering::SeqCst)
    }

    fn hold_lost(&self) -> StoreError {
        StoreError::HoldLost {
            target: self.target().to_string(),
        }
    }

    /// Create `instance_hold` and its seed row if missing. Idempotent. Two
    /// openers racing the CREATE on PostgreSQL can fail with a duplicate
    /// object error, so one failure is retried before it is reported.
    pub(crate) async fn ensure_hold_table(&self) -> Result<(), StoreError> {
        match self.create_hold_table().await {
            Ok(()) => Ok(()),
            Err(_) => self.create_hold_table().await,
        }
    }

    async fn create_hold_table(&self) -> Result<(), StoreError> {
        let mut tx = self.db().begin_immediate().await?;
        tx.execute(CREATE_TABLE, &[]).await?;
        tx.execute(SEED_ROW, &[]).await?;
        tx.commit().await
    }

    /// Acquire the hold with one conditional UPDATE. `Ok(Some(_))` names the
    /// stale holder this acquisition displaced; `HoldHeld` means a live holder
    /// has it.
    pub(crate) async fn acquire_hold(&self) -> Result<Option<Takeover>, StoreError> {
        let mut tx = self.db().begin_immediate().await?;
        let before = self.read_row_locked(&mut tx).await?;
        // Sampled once the row lock is held: taking it can wait out another
        // writer, and the stale cutoff must reflect when the row is read.
        let now = now_secs();
        let stale = i64::try_from(self.hold.stale_seconds).unwrap_or(i64::MAX);
        let changed = tx
            .execute(
                ACQUIRE,
                &[
                    self.hold.holder_id.as_str().into(),
                    self.hold.label.as_str().into(),
                    now.into(),
                    now.saturating_sub(stale).into(),
                ],
            )
            .await?;
        if changed == 1 {
            tx.commit().await?;
            self.hold.fenced.store(false, Ordering::SeqCst);
            let takeover = before
                .filter(|r| {
                    r.holder_id
                        .as_deref()
                        .is_some_and(|h| h != self.hold.holder_id)
                })
                .map(|r| Takeover {
                    label: r.label(),
                    age_secs: r.age(now),
                });
            if let Some(previous) = &takeover {
                tracing::warn!(
                    target: "shunt::store",
                    previous_holder = %previous.label,
                    age_secs = previous.age_secs,
                    "took over a stale store hold; the previous holder stopped renewing"
                );
            }
            return Ok(takeover);
        }
        let current = read_row(&mut tx).await?;
        tx.rollback().await?;
        match current {
            Some(row) => Err(StoreError::HoldHeld {
                target: self.target().to_string(),
                label: row.label(),
                age_secs: row.age(now),
            }),
            None => Err(StoreError::Database {
                reason: "the instance_hold row is missing".to_string(),
            }),
        }
    }

    /// Begin a transaction for a durable write. It first renews
    /// `heartbeat_at` guarded by `holder_id = $me`; zero rows means another
    /// process took the hold, so the transaction is rolled back, the store is
    /// fenced, and [`StoreError::HoldLost`] is returned with nothing written.
    ///
    /// Every durable write (pool flush, spend, gateway tokens, import) must
    /// start here rather than with `Db::begin`: a holder stalled past the
    /// stale threshold and taken over would otherwise keep writing until its
    /// next heartbeat. Once fenced this returns `HoldLost` without touching
    /// the database.
    pub async fn begin_durable(&self) -> Result<Tx, StoreError> {
        if self.is_fenced() {
            return Err(self.hold_lost());
        }
        let tx = self.db().begin().await?;
        self.renew_in(tx).await
    }

    /// Run `RENEW` as the first write of `tx`, rolling back and fencing when
    /// the row is no longer ours.
    async fn renew_in(&self, mut tx: Tx) -> Result<Tx, StoreError> {
        let renewed = tx
            .execute(
                RENEW,
                &[now_secs().into(), self.hold.holder_id.as_str().into()],
            )
            .await?;
        if renewed == 0 {
            tx.rollback().await?;
            self.hold.fenced.store(true, Ordering::SeqCst);
            return Err(self.hold_lost());
        }
        Ok(tx)
    }

    /// Begin a transaction for reading back a write whose commit outcome is
    /// unknown ([`StoreError::CommitUncertain`]). It starts like
    /// [`Self::begin_durable`]: the `RENEW` takes the instance-hold row lock,
    /// which every durable transaction holds from its own `RENEW` until it
    /// commits or rolls back. So this waits for a durable transaction still in
    /// flight, and a read made in it sees that transaction's outcome. A plain
    /// read can run while the server is still committing and return the state
    /// from before it.
    ///
    /// On PostgreSQL the wait is bounded by [`SETTLED_LOCK_TIMEOUT`]: past it
    /// the `RENEW` fails with a [`StoreError::Database`], which callers treat
    /// as a read-back that failed. SQLite has one connection, so this waits
    /// for the pool instead, bounded by its acquire timeout.
    pub async fn begin_settled(&self) -> Result<Tx, StoreError> {
        if self.is_fenced() {
            return Err(self.hold_lost());
        }
        let mut tx = self.db().begin().await?;
        self.bound_lock_wait(&mut tx).await?;
        self.renew_in(tx).await
    }

    /// Read the hold row with its lock held for the rest of `tx`. SQLite's
    /// `BEGIN IMMEDIATE` already holds the write lock, so a plain read does.
    /// PostgreSQL takes the row lock here (`FOR UPDATE`), bounded like
    /// [`Self::begin_settled`]; without it the `ACQUIRE` update would wait for
    /// the lock with a stale cutoff computed before the wait, and the row read
    /// could name an earlier holder than the one the update displaces.
    async fn read_row_locked(&self, tx: &mut Tx) -> Result<Option<HoldRow>, StoreError> {
        match tx {
            Tx::Sqlite(_) => read_row(tx).await,
            #[cfg(feature = "store-postgres")]
            Tx::Postgres(_) => {
                self.bound_lock_wait(tx).await?;
                read_row_with(tx, READ_ROW_FOR_UPDATE).await
            }
        }
    }

    /// Set the settled lock timeout for the rest of `tx`. A no-op on SQLite.
    async fn bound_lock_wait(&self, tx: &mut Tx) -> Result<(), StoreError> {
        match tx {
            Tx::Sqlite(_) => Ok(()),
            #[cfg(feature = "store-postgres")]
            Tx::Postgres(_) => {
                let timeout = self.hold.settled_lock_timeout_ms.load(Ordering::SeqCst);
                tx.execute(SET_LOCK_TIMEOUT, &[format!("{timeout}ms").into()])
                    .await
                    .map(|_| ())
            }
        }
    }

    /// Shorten [`SETTLED_LOCK_TIMEOUT`] so a test does not wait for it.
    #[cfg(test)]
    pub(crate) fn set_settled_lock_timeout(&self, timeout: Duration) {
        self.hold
            .settled_lock_timeout_ms
            .store(millis(timeout), Ordering::SeqCst);
    }

    /// Renew the hold once, outside any caller transaction. `Ok(false)` means
    /// the row was taken; the store is then fenced. Database errors do not
    /// fence.
    pub(crate) async fn heartbeat_once(&self) -> Result<bool, StoreError> {
        let mut tx = self.db().begin().await?;
        let renewed = tx
            .execute(
                RENEW,
                &[now_secs().into(), self.hold.holder_id.as_str().into()],
            )
            .await?;
        tx.commit().await?;
        if renewed == 0 {
            self.hold.fenced.store(true, Ordering::SeqCst);
        }
        Ok(renewed == 1)
    }

    /// Start the heartbeat task: every `hold_stale_seconds / 3` (at least one
    /// second). A taken hold fences the store and logs ERROR once, then the
    /// task ends; a transient database error logs WARN and keeps trying.
    pub fn spawn_heartbeat(&self) -> HeartbeatHandle {
        let store = self.clone();
        let period = Duration::from_secs((self.hold.stale_seconds / 3).max(1));
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match store.heartbeat_once().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::error!(
                            target: "shunt::store",
                            holder = %store.hold.label,
                            "the store hold was taken over by another shunt process; durable writes are refused from now on"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "shunt::store",
                            %error,
                            "store hold heartbeat failed; will retry"
                        );
                    }
                }
            }
        });
        HeartbeatHandle { task }
    }

    /// Release the hold on graceful shutdown. A no-op if it is no longer ours.
    pub async fn release_hold(&self) -> Result<(), StoreError> {
        let mut tx = self.db().begin().await?;
        tx.execute(RELEASE, &[self.hold.holder_id.as_str().into()])
            .await?;
        tx.commit().await
    }
}

struct HoldRow {
    holder_id: Option<String>,
    label: Option<String>,
    heartbeat_at: Option<i64>,
}

impl HoldRow {
    fn label(&self) -> String {
        self.label.clone().unwrap_or_else(|| "unknown".to_string())
    }

    fn age(&self, now: i64) -> u64 {
        self.heartbeat_at
            .map_or(0, |at| u64::try_from(now.saturating_sub(at)).unwrap_or(0))
    }
}

async fn read_row(tx: &mut Tx) -> Result<Option<HoldRow>, StoreError> {
    read_row_with(tx, READ_ROW).await
}

async fn read_row_with(tx: &mut Tx, sql: &str) -> Result<Option<HoldRow>, StoreError> {
    let params: [Value; 0] = [];
    match tx.fetch_optional(sql, &params).await? {
        Some(row) => Ok(Some(HoldRow {
            holder_id: row.opt_text(0)?,
            label: row.opt_text(1)?,
            heartbeat_at: row.opt_int(2)?,
        })),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
