//! Opt-in on-disk persistence of the account pool's quota state.
//!
//! When `[server.pool] state_path` is set, shunt writes each account's quota
//! (per-window utilization + reset) to that file and restores it at the next
//! boot, so a restart warm-starts from the last observed utilization instead of
//! an empty pool. Without warm-start every account looks unseen until its first
//! post-restart response, which defeats burn-rate avoidance (issue #135) and
//! leaves `GET /usage` blank until traffic re-populates the pool.
//!
//! The file is a best-effort cache, not a source of truth: quota is re-derived
//! from upstream responses (and the usage API) regardless, so a missing, stale,
//! or corrupt file only costs a cold start — never a boot failure. Restored
//! windows whose reset has already passed are dropped lazily by the next
//! `select_order`/`snapshot`, exactly as live ones are.
//!
//! Only quota is persisted. Cooldowns are a monotonic [`std::time::Instant`]
//! (not portable across a restart) and short-lived, so they are intentionally
//! left to lapse on boot.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{accounts::QuotaState, server::AppState};

/// Bump when the on-disk shape changes incompatibly; a file whose version does
/// not match is ignored (cold start) rather than mis-parsed.
const STATE_VERSION: u32 = 1;

/// How often the background task flushes dirty quota to disk. A restart loses at
/// most this much of the newest quota, which the next response re-derives anyway.
const FLUSH_INTERVAL: Duration = Duration::from_secs(15);

/// On-disk envelope: a version tag plus one entry per observed account.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedPool {
    version: u32,
    accounts: Vec<PersistedAccount>,
}

/// One account's persisted quota, keyed by the same `(provider, identity)` the
/// pool uses internally ([`crate::accounts::account_identity`]).
#[derive(Debug, Serialize, Deserialize)]
struct PersistedAccount {
    provider: String,
    identity: String,
    quota: QuotaState,
}

/// The configured state file, or `None` when persistence is disabled.
fn state_path(state: &AppState) -> Option<&Path> {
    state.config.server.pool.as_ref()?.state_path.as_deref()
}

/// Restore pool quota from disk at boot. A no-op when `state_path` is unset or
/// the file is absent/unreadable/incompatible — every failure mode falls back
/// to a cold start, never a boot error. Call once before serving requests so
/// the first request already sees the restored quota.
pub fn restore(state: &AppState) {
    let Some(path) = state_path(state) else {
        return;
    };
    match load(path) {
        Ok(Some(persisted)) => {
            let count = persisted.accounts.len();
            state.accounts.import_quotas(
                persisted
                    .accounts
                    .into_iter()
                    .map(|account| (account.provider, account.identity, account.quota)),
            );
            tracing::info!(
                path = %path.display(),
                accounts = count,
                "restored pool quota state from disk"
            );
        }
        // Absent file or version/parse mismatch: nothing to restore, cold start.
        Ok(None) => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to read pool state file; starting cold"
        ),
    }
}

/// Spawn the background flush loop if `state_path` is configured. A no-op
/// otherwise, so the default deployment adds no background work. Whether the
/// task exists is decided once from the boot config (like the usage poller); a
/// reload does not start or stop it.
pub fn spawn_state_persister(state: AppState) {
    if state_path(&state).is_none() {
        return;
    }
    tracing::info!(
        interval_secs = FLUSH_INTERVAL.as_secs(),
        "pool state persistence enabled"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; consume it so the first real flush
        // waits a full interval (there is nothing new to write at t=0).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            flush(&state).await;
        }
    });
}

/// Write the pool's quota to disk if it changed since the last flush. Reads and
/// clears the dirty flag first, so an idle interval writes nothing.
async fn flush(state: &AppState) {
    let Some(path) = state_path(state).map(Path::to_path_buf) else {
        return;
    };
    if !state.accounts.take_dirty() {
        return;
    }
    let accounts = state.accounts.clone();
    // Serialization + the filesystem write are blocking; keep them off the async
    // worker. The quota snapshot itself briefly locks the pool inside the task.
    let result = tokio::task::spawn_blocking(move || {
        let persisted = PersistedPool {
            version: STATE_VERSION,
            accounts: accounts
                .export_quotas()
                .into_iter()
                .map(|(provider, identity, quota)| PersistedAccount {
                    provider,
                    identity,
                    quota,
                })
                .collect(),
        };
        save(&path, &persisted)
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "failed to persist pool state"),
        Err(error) => tracing::warn!(%error, "pool state persister task panicked"),
    }
}

/// Read and validate the state file. `Ok(None)` covers every recoverable case
/// (absent file, invalid JSON, version mismatch) so the caller can cold-start;
/// `Err` is reserved for unexpected I/O errors worth surfacing.
fn load(path: &Path) -> io::Result<Option<PersistedPool>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let persisted: PersistedPool = match serde_json::from_slice(&bytes) {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "pool state file is not valid json; ignoring"
            );
            return Ok(None);
        }
    };
    if persisted.version != STATE_VERSION {
        tracing::warn!(
            path = %path.display(),
            found = persisted.version,
            expected = STATE_VERSION,
            "pool state file version mismatch; ignoring"
        );
        return Ok(None);
    }
    Ok(Some(persisted))
}

/// Write the state atomically: serialize to a sibling temp file, then rename
/// over the target. Rename is atomic on the same filesystem, so a crash
/// mid-write never leaves a truncated file where a reader would find it.
fn save(path: &Path, pool: &PersistedPool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_vec_pretty(pool).map_err(io::Error::other)?;
    let tmp = tmp_path(path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Sibling `<name>.tmp` path used as the atomic-write staging file.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::QuotaState;

    fn sample_pool() -> PersistedPool {
        PersistedPool {
            version: STATE_VERSION,
            accounts: vec![PersistedAccount {
                provider: "anthropic".to_string(),
                identity: "acct-a".to_string(),
                quota: QuotaState {
                    utilization_5h: Some(0.42),
                    reset_5h: Some(9_999_999_999),
                    status: Some("allowed".to_string()),
                    ..Default::default()
                },
            }],
        }
    }

    /// A unique temp path per test so parallel `cargo test` runs never collide.
    fn temp_file(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-state-persist-{}-{}-{label}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn save_then_load_round_trips_quota() {
        let path = temp_file("roundtrip");
        let pool = sample_pool();
        save(&path, &pool).expect("save succeeds");

        let loaded = load(&path).expect("load succeeds").expect("file present");
        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.accounts.len(), 1);
        let account = &loaded.accounts[0];
        assert_eq!(account.provider, "anthropic");
        assert_eq!(account.identity, "acct-a");
        assert_eq!(account.quota.utilization_5h, Some(0.42));
        assert_eq!(account.quota.reset_5h, Some(9_999_999_999));
        assert_eq!(account.quota.status.as_deref(), Some("allowed"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = temp_file("no-temp");
        save(&path, &sample_pool()).expect("save succeeds");
        assert!(!tmp_path(&path).exists(), "temp file must be renamed away");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_missing_file_is_cold_start_not_error() {
        let path = temp_file("missing");
        assert!(load(&path).expect("absent file is not an error").is_none());
    }

    #[test]
    fn load_corrupt_json_is_ignored() {
        let path = temp_file("corrupt");
        std::fs::write(&path, b"{ this is not json").expect("write corrupt file");
        assert!(load(&path).expect("corrupt file is recoverable").is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_version_mismatch_is_ignored() {
        let path = temp_file("version");
        let stale = format!("{{\"version\":{},\"accounts\":[]}}", STATE_VERSION + 1);
        std::fs::write(&path, stale).expect("write stale-version file");
        assert!(load(&path)
            .expect("version mismatch is recoverable")
            .is_none());
        std::fs::remove_file(&path).ok();
    }
}
