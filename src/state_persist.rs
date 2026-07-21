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

use std::{fs, io, path::Path, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    accounts::{AccountKey, QuotaState},
    server::AppState,
};

/// Version 2 replaces the provider-name key with the physical-account key.
const STATE_VERSION: u32 = 2;

/// How often the background task flushes dirty quota to disk. A restart loses at
/// most this much of the newest quota, which the next response re-derives anyway.
const FLUSH_INTERVAL: Duration = Duration::from_secs(15);

/// On-disk envelope: a version tag plus one entry per observed account.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedPool {
    version: u32,
    accounts: Vec<PersistedAccount>,
}

/// One physical account's persisted quota.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedAccount {
    key: AccountKey,
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
pub async fn restore(state: &AppState) {
    let Some(path) = state_path(state).map(Path::to_path_buf) else {
        return;
    };
    let load_path = path.clone();
    let result = tokio::task::spawn_blocking(move || load(&load_path)).await;
    match result {
        Ok(Ok(Some(persisted))) => {
            let count = persisted.accounts.len();
            state.accounts.import_quotas(
                persisted
                    .accounts
                    .into_iter()
                    .map(|account| (account.key, account.quota)),
            );
            tracing::info!(
                path = %path.display(),
                accounts = count,
                "restored pool quota state from disk"
            );
        }
        // Absent file or version/parse mismatch: nothing to restore, cold start.
        Ok(Ok(None)) => {}
        Ok(Err(error)) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to read pool state file; starting cold"
        ),
        Err(error) => tracing::warn!(%error, "pool state restore task panicked"),
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

/// Write the pool's quota to disk if it changed since the last flush. Atomically
/// claims the current dirty state so an idle interval writes nothing. A failed
/// write marks the pool dirty again for the next timer tick; mutations that land
/// during the blocking save independently leave the flag set.
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
                .map(|(key, quota)| PersistedAccount { key, quota })
                .collect(),
        };
        save(&path, &persisted)
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            state.accounts.mark_dirty();
            tracing::warn!(%error, "failed to persist pool state");
        }
        Err(error) => {
            state.accounts.mark_dirty();
            tracing::warn!(%error, "pool state persister task panicked");
        }
    }
}

/// Read and validate the state file. `Ok(None)` covers every recoverable case
/// (absent file, invalid JSON, version mismatch) so the caller can cold-start;
/// `Err` is reserved for unexpected I/O errors worth surfacing.
fn load(path: &Path) -> io::Result<Option<PersistedPool>> {
    let bytes = match fs::read(path) {
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

/// Write the state atomically via [`crate::atomic_file::write_private_atomic`]:
/// a private sibling temp file renamed over the target, so a crash mid-write
/// never leaves a truncated file where a reader would find it.
fn save(path: &Path, pool: &PersistedPool) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(pool).map_err(io::Error::other)?;
    crate::atomic_file::write_private_atomic(path, &json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::QuotaState,
        config::{AccountConfig, Config, PoolConfig},
    };
    use std::path::PathBuf;

    fn sample_pool() -> PersistedPool {
        PersistedPool {
            version: STATE_VERSION,
            accounts: vec![PersistedAccount {
                key: crate::accounts::account_key("anthropic", &account("acct-a")),
                quota: QuotaState {
                    utilization_5h: Some(0.42),
                    reset_5h: Some(9_999_999_999),
                    status: Some("allowed".to_string()),
                    ..Default::default()
                },
            }],
        }
    }

    fn temp_file(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shunt-state-persist-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create test directory");
        dir.join("state.json")
    }

    fn remove_test_dir(path: &Path) {
        fs::remove_dir_all(path.parent().expect("test path has parent")).ok();
    }

    fn state_with_path(path: PathBuf) -> AppState {
        let mut config = Config::default();
        config.server.pool = Some(PoolConfig {
            state_path: Some(path),
            ..Default::default()
        });
        AppState::new(config, reqwest::Client::new()).expect("valid test config")
    }

    fn account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn save_then_load_round_trips_quota() {
        let path = temp_file("roundtrip");
        save(&path, &sample_pool()).expect("save succeeds");

        let loaded = load(&path).expect("load succeeds").expect("file present");
        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.accounts.len(), 1);
        let persisted_account = &loaded.accounts[0];
        assert_eq!(
            persisted_account.key,
            crate::accounts::account_key("anthropic", &account("acct-a"))
        );
        assert_eq!(persisted_account.quota.utilization_5h, Some(0.42));
        assert_eq!(persisted_account.quota.reset_5h, Some(9_999_999_999));
        assert_eq!(persisted_account.quota.status.as_deref(), Some("allowed"));

        remove_test_dir(&path);
    }

    #[test]
    fn save_atomically_replaces_existing_target() {
        let path = temp_file("overwrite");
        save(&path, &sample_pool()).expect("initial save succeeds");
        let replacement = PersistedPool {
            version: STATE_VERSION,
            accounts: vec![PersistedAccount {
                key: crate::accounts::account_key("codex", &account("acct-b")),
                quota: QuotaState {
                    status: Some("weekly".to_string()),
                    ..Default::default()
                },
            }],
        };
        save(&path, &replacement).expect("replacement save succeeds");

        let loaded = load(&path).expect("load succeeds").expect("file present");
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(
            loaded.accounts[0].key,
            crate::accounts::account_key("codex", &account("acct-b"))
        );
        assert_eq!(
            loaded.accounts[0].key.identity,
            crate::accounts::account_key("codex", &account("acct-b")).identity
        );
        assert_eq!(loaded.accounts[0].quota.status.as_deref(), Some("weekly"));
        remove_test_dir(&path);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = temp_file("no-temp");
        save(&path, &sample_pool()).expect("save succeeds");
        let entries = fs::read_dir(path.parent().unwrap())
            .expect("read test directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read entries");
        assert_eq!(entries.len(), 1, "only the target file should remain");
        assert_eq!(entries[0].path(), path);
        remove_test_dir(&path);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_file("permissions");
        save(&path, &sample_pool()).expect("save succeeds");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn failed_flush_keeps_pool_dirty_for_retry() {
        let path = temp_file("flush-failure");
        fs::create_dir(&path).expect("target directory makes rename fail");
        let state = state_with_path(path.clone());
        state.accounts.import_quotas([(
            crate::accounts::account_key("anthropic", &account("acct-a")),
            sample_pool().accounts.remove(0).quota,
        )]);
        state.accounts.mark_dirty();

        flush(&state).await;

        assert!(state.accounts.take_dirty(), "failed save must be retried");
        let entries = fs::read_dir(path.parent().unwrap())
            .expect("read test directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read entries");
        assert_eq!(entries.len(), 1, "failed save must clean up its temp file");
        assert_eq!(entries[0].path(), path);
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn restore_warm_starts_pool_snapshot() {
        let path = temp_file("restore");
        save(&path, &sample_pool()).expect("save succeeds");
        let state = state_with_path(path.clone());

        restore(&state).await;

        let snapshots = state
            .accounts
            .snapshot("anthropic", &[account("acct-a")], None, None);
        assert!(snapshots[0].has_state);
        assert_eq!(snapshots[0].utilization_5h, Some(0.42));
        assert_eq!(snapshots[0].reset_5h, Some(9_999_999_999));
        assert_eq!(snapshots[0].status.as_deref(), Some("allowed"));
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn restore_missing_corrupt_or_version_mismatched_file_starts_cold() {
        for (label, contents) in [
            ("missing", None),
            ("corrupt", Some(b"{ this is not json".to_vec())),
            (
                "old-version",
                Some(b"{\"version\":1,\"accounts\":[]}".to_vec()),
            ),
            (
                "future-version",
                Some(format!("{{\"version\":{},\"accounts\":[]}}", STATE_VERSION + 1).into_bytes()),
            ),
        ] {
            let path = temp_file(label);
            if let Some(contents) = contents {
                fs::write(&path, contents).expect("write invalid state file");
            }
            let state = state_with_path(path.clone());

            restore(&state).await;

            let snapshots = state
                .accounts
                .snapshot("anthropic", &[account("acct-a")], None, None);
            assert!(!snapshots[0].has_state, "{label} file should start cold");
            remove_test_dir(&path);
        }
    }

    #[tokio::test]
    async fn first_snapshot_expires_stale_restored_quota() {
        let path = temp_file("expired");
        let expired = PersistedPool {
            version: STATE_VERSION,
            accounts: vec![PersistedAccount {
                key: crate::accounts::account_key("anthropic", &account("acct-a")),
                quota: QuotaState {
                    utilization_5h: Some(1.0),
                    reset_5h: Some(1),
                    status: Some("rejected".to_string()),
                    ..Default::default()
                },
            }],
        };
        save(&path, &expired).expect("save succeeds");
        let state = state_with_path(path.clone());
        restore(&state).await;

        let snapshots = state
            .accounts
            .snapshot("anthropic", &[account("acct-a")], None, None);

        assert!(snapshots[0].has_state);
        assert!(snapshots[0].available, "stale quota must not avoid account");
        assert!(!snapshots[0].near_quota);
        assert_eq!(snapshots[0].utilization_5h, None);
        assert_eq!(snapshots[0].reset_5h, None);
        assert_eq!(snapshots[0].status, None);
        remove_test_dir(&path);
    }
}
