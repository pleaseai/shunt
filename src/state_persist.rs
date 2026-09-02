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
//! windows whose reset has already passed are dropped during import, before
//! the first `select_order`/`snapshot`, exactly as live ones are. A legacy
//! aggregate status attached to such a window is dropped at the same import
//! boundary when it has no independent observation timestamp. Version-2
//! aggregate status without an independent timestamp captures the earliest
//! persisted window reset as an immutable deadline through the existing
//! aggregate cap; a synthesized future timestamp is clamped to boot time. A
//! version-2 aggregate without a reset uses boot time as its cap. The
//! synthesized stamp can remain in the v3 rewrite, and a later v3 restore
//! preserves it while still running normal metadata normalization, expiry,
//! missing-signal boot stamping, and future-timestamp clamping. A window that
//! never carried a reset instant is bounded instead by its persisted
//! utilization observation time and independent per-window status observation
//! time: each reset-less signal expires one window length after its own
//! timestamp, whether restored from disk or recorded live, so polling one
//! signal cannot keep the other alive indefinitely.
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
/// Version 3 separates utilization and per-window status freshness.
const LEGACY_STATE_VERSION: u32 = 2;
const STATE_VERSION: u32 = 3;

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
/// to a cold start, never a boot error. Version-2 files take the explicit
/// legacy import path, which synthesizes an unstamped aggregate's earliest
/// captured reset deadline through the existing v3 cap before rewriting the
/// file. A second restore treats that synthesized value as an existing v3
/// stamp while still applying normal normalization, expiry, and future-time
/// clamping. Call once before serving requests so the first request already
/// sees the restored quota.
pub async fn restore(state: &AppState) {
    let Some(path) = state_path(state).map(Path::to_path_buf) else {
        return;
    };
    let load_path = path.clone();
    let result = tokio::task::spawn_blocking(move || load(&load_path)).await;
    match result {
        Ok(Ok(Some(persisted))) => {
            let count = persisted.accounts.len();
            let legacy = persisted.version == LEGACY_STATE_VERSION;
            let corrected = if legacy {
                state.accounts.import_quotas_legacy(
                    persisted
                        .accounts
                        .into_iter()
                        .map(|account| (account.key, account.quota)),
                )
            } else {
                state.accounts.import_quotas(
                    persisted
                        .accounts
                        .into_iter()
                        .map(|account| (account.key, account.quota)),
                )
            };
            if legacy || corrected {
                state.accounts.mark_dirty();
            }
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
    if persisted.version != STATE_VERSION && persisted.version != LEGACY_STATE_VERSION {
        tracing::warn!(
            path = %path.display(),
            found = persisted.version,
            expected = format!("{STATE_VERSION} or {LEGACY_STATE_VERSION}"),
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
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::path::PathBuf;

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs()
    }

    fn sample_pool() -> PersistedPool {
        PersistedPool {
            version: STATE_VERSION,
            accounts: vec![PersistedAccount {
                key: crate::accounts::account_key("anthropic", &account("acct-a")),
                quota: QuotaState {
                    utilization_5h: Some(0.42),
                    reset_5h: Some(unix_now() + 3_600),
                    status: Some("allowed".to_string()),
                    observed_at_5h: Some(unix_now()),
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

    async fn restore_v2_then_v3(
        path: &Path,
        quota: QuotaState,
    ) -> (QuotaState, Option<QuotaState>, PersistedPool) {
        let key = crate::accounts::account_key("anthropic", &account("acct-matrix"));
        save(
            path,
            &PersistedPool {
                version: LEGACY_STATE_VERSION,
                accounts: vec![PersistedAccount {
                    key: key.clone(),
                    quota,
                }],
            },
        )
        .expect("save v2 succeeds");

        let first_state = state_with_path(path.to_path_buf());
        restore(&first_state).await;
        let first = first_state
            .accounts
            .raw_quota_for_test(&key)
            .expect("v2 account is restored")
            .1;
        flush(&first_state).await;

        let rewritten = load(path)
            .expect("load rewritten state succeeds")
            .expect("rewritten v3 state exists");
        let second_state = state_with_path(path.to_path_buf());
        restore(&second_state).await;
        let second = second_state
            .accounts
            .raw_quota_for_test(&key)
            .map(|(_, quota)| quota);
        (first, second, rewritten)
    }

    #[test]
    fn save_then_load_round_trips_quota() {
        let path = temp_file("roundtrip");
        let pool = sample_pool();
        save(&path, &pool).expect("save succeeds");

        let loaded = load(&path).expect("load succeeds").expect("file present");
        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.accounts.len(), 1);
        let persisted_account = &loaded.accounts[0];
        assert_eq!(
            persisted_account.key,
            crate::accounts::account_key("anthropic", &account("acct-a"))
        );
        assert_eq!(persisted_account.quota.utilization_5h, Some(0.42));
        assert_eq!(
            persisted_account.quota.reset_5h,
            pool.accounts[0].quota.reset_5h
        );
        assert_eq!(persisted_account.quota.status.as_deref(), Some("allowed"));
        assert_eq!(
            persisted_account.quota.observed_at_5h, pool.accounts[0].quota.observed_at_5h,
            "the observation time round-trips through disk like any other quota field"
        );

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
        let pool = sample_pool();
        save(&path, &pool).expect("save succeeds");
        let state = state_with_path(path.clone());

        restore(&state).await;

        let snapshots = state
            .accounts
            .snapshot("anthropic", &[account("acct-a")], None, None);
        assert!(snapshots[0].has_state);
        assert_eq!(snapshots[0].utilization_5h, Some(0.42));
        assert_eq!(snapshots[0].reset_5h, pool.accounts[0].quota.reset_5h);
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
    async fn restore_migrates_v2_combined_status_timestamp_and_rewrites_v3() {
        let path = temp_file("migrate-v2");
        let account = account("acct-a");
        let observed = unix_now().saturating_sub(60);
        let reset = unix_now() + 3_600;
        save(
            &path,
            &PersistedPool {
                version: LEGACY_STATE_VERSION,
                accounts: vec![PersistedAccount {
                    key: crate::accounts::account_key("anthropic", &account),
                    quota: QuotaState {
                        status_5h: Some("rejected".to_string()),
                        observed_at_5h: Some(observed),
                        reset_5h: Some(reset),
                        ..Default::default()
                    },
                }],
            },
        )
        .expect("save succeeds");
        let state = state_with_path(path.clone());

        restore(&state).await;

        let key = crate::accounts::account_key("anthropic", &account);
        let (_, quota) = state.accounts.raw_quota_for_test(&key).unwrap();
        assert_eq!(quota.observed_at_5h, None);
        assert_eq!(quota.observed_at_status_5h, Some(observed));
        assert_eq!(quota.reset_at_status_5h, Some(reset));
        assert!(state.accounts.take_dirty(), "v2 import schedules a rewrite");
        state.accounts.mark_dirty();
        flush(&state).await;

        let rewritten = load(&path)
            .expect("load succeeds")
            .expect("rewritten file exists");
        assert_eq!(rewritten.version, STATE_VERSION);
        assert_eq!(
            rewritten.accounts[0].quota.observed_at_status_5h,
            Some(observed)
        );
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn restore_migrates_v2_aggregate_status_to_earliest_reset_deadline() {
        const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;
        let path = temp_file("migrate-v2-aggregate");
        let account = account("acct-aggregate");
        let before = unix_now();
        let earliest = before + 3_600;
        let quota = QuotaState {
            status: Some("rejected".to_string()),
            reset_5h: Some(before + 7_200),
            reset_7d: Some(earliest),
            reset_7d_oi: Some(before + 10_800),
            ..Default::default()
        };
        save(
            &path,
            &PersistedPool {
                version: LEGACY_STATE_VERSION,
                accounts: vec![PersistedAccount {
                    key: crate::accounts::account_key("anthropic", &account),
                    quota,
                }],
            },
        )
        .expect("save succeeds");
        let state = state_with_path(path.clone());

        restore(&state).await;

        let key = crate::accounts::account_key("anthropic", &account);
        let (_, quota) = state.accounts.raw_quota_for_test(&key).unwrap();
        assert_eq!(
            quota.observed_at_status,
            Some(earliest.saturating_sub(WINDOW_7D_SECS)),
            "v2 aggregate status keeps the earliest captured reset as its cap"
        );
        assert!(state.accounts.take_dirty(), "v2 import schedules a rewrite");
        state.accounts.mark_dirty();
        flush(&state).await;

        let rewritten = load(&path)
            .expect("load succeeds")
            .expect("rewritten file exists");
        assert_eq!(rewritten.version, STATE_VERSION);
        assert_eq!(
            rewritten.accounts[0].quota.observed_at_status,
            Some(earliest.saturating_sub(WINDOW_7D_SECS))
        );
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn restore_v2_aggregate_boundaries_survive_the_v3_restore_pipeline() {
        const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;
        enum Expectation {
            Removed,
            Deadline {
                earliest: u64,
                resets: [Option<u64>; 3],
            },
            BootCap,
        }

        let before = unix_now();
        let valid = before + 3_600;
        let later = before + 7_200;
        let cases = [
            (
                "matrix-past",
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(before.saturating_sub(1)),
                    ..Default::default()
                },
                Expectation::Removed,
            ),
            (
                "matrix-reset-only",
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(valid),
                    ..Default::default()
                },
                Expectation::Deadline {
                    earliest: valid,
                    resets: [Some(valid), None, None],
                },
            ),
            (
                "matrix-no-reset",
                QuotaState {
                    status: Some("rejected".to_string()),
                    ..Default::default()
                },
                Expectation::BootCap,
            ),
            (
                "matrix-far-future",
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(before + WINDOW_7D_SECS + 3_600),
                    ..Default::default()
                },
                Expectation::BootCap,
            ),
            (
                "matrix-max",
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_7d: Some(u64::MAX),
                    ..Default::default()
                },
                Expectation::BootCap,
            ),
            (
                "matrix-epoch-near",
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_7d_oi: Some(WINDOW_7D_SECS - 1),
                    ..Default::default()
                },
                Expectation::Removed,
            ),
            (
                "matrix-multiple-reset",
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(later),
                    reset_7d: Some(valid),
                    reset_7d_oi: Some(later + 3_600),
                    ..Default::default()
                },
                Expectation::Deadline {
                    earliest: valid,
                    resets: [Some(later), Some(valid), Some(later + 3_600)],
                },
            ),
        ];

        for (label, input, expectation) in cases {
            let path = temp_file(label);
            let (first, second, rewritten) = restore_v2_then_v3(&path, input).await;
            assert_eq!(rewritten.version, STATE_VERSION, "{label} rewrites as v3");
            match expectation {
                Expectation::Removed => {
                    assert_eq!(first, QuotaState::default(), "{label} clears on import");
                    assert!(second.is_none(), "{label} stays absent after v3 restore");
                    assert!(rewritten.accounts.is_empty(), "{label} is removed on flush");
                }
                Expectation::Deadline { earliest, resets } => {
                    assert_eq!(first.status.as_deref(), Some("rejected"), "{label}");
                    assert_eq!(
                        first.observed_at_status,
                        Some(earliest - WINDOW_7D_SECS),
                        "{label} captures the earliest reset deadline"
                    );
                    assert_eq!([first.reset_5h, first.reset_7d, first.reset_7d_oi], resets);
                    assert_eq!(second.as_ref(), Some(&first), "{label} survives v3 restore");
                    assert_eq!(rewritten.accounts.len(), 1, "{label} remains persisted");
                    assert_eq!(
                        rewritten.accounts[0].quota, first,
                        "{label} rewrites equivalently"
                    );
                }
                Expectation::BootCap => {
                    assert_eq!(first.status.as_deref(), Some("rejected"), "{label}");
                    let stamp = first
                        .observed_at_status
                        .expect("{label} has a boot-time aggregate stamp");
                    let after = unix_now();
                    assert!(
                        stamp >= before && stamp <= after,
                        "{label} uses boot-time cap"
                    );
                    assert_eq!(second.as_ref(), Some(&first), "{label} survives v3 restore");
                    assert_eq!(rewritten.accounts.len(), 1, "{label} remains persisted");
                    assert_eq!(
                        rewritten.accounts[0].quota, first,
                        "{label} rewrites equivalently"
                    );
                }
            }
            remove_test_dir(&path);
        }
    }

    #[tokio::test]
    async fn restore_v2_existing_aggregate_stamps_are_stable_or_removed() {
        const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;
        enum Expectation {
            SurvivesInBootRange,
            Removed,
            SurvivesWithPastWindowResetCleared,
        }

        let before = unix_now();
        let cases = [
            (
                "stamp-future",
                QuotaState {
                    status: Some("rejected".to_string()),
                    observed_at_status: Some(before + 86_400),
                    ..Default::default()
                },
                Expectation::SurvivesInBootRange,
            ),
            (
                "stamp-expired",
                QuotaState {
                    status: Some("rejected".to_string()),
                    observed_at_status: Some(before.saturating_sub(WINDOW_7D_SECS)),
                    ..Default::default()
                },
                Expectation::Removed,
            ),
            (
                "stamp-orphan",
                QuotaState {
                    observed_at_status: Some(before + 86_400),
                    ..Default::default()
                },
                Expectation::Removed,
            ),
            (
                "stamp-past-window-reset",
                QuotaState {
                    status: Some("rejected".to_string()),
                    observed_at_status: Some(before.saturating_sub(60)),
                    reset_5h: Some(before.saturating_sub(1)),
                    ..Default::default()
                },
                Expectation::SurvivesWithPastWindowResetCleared,
            ),
        ];

        for (label, input, expectation) in cases {
            let path = temp_file(label);
            let (first, second, rewritten) = restore_v2_then_v3(&path, input).await;
            assert_eq!(rewritten.version, STATE_VERSION, "{label} rewrites as v3");
            match expectation {
                Expectation::SurvivesInBootRange => {
                    let stamp = first.observed_at_status.expect("future v2 stamp survives");
                    let after = unix_now();
                    assert!(
                        stamp >= before && stamp <= after,
                        "future stamp clamps to boot"
                    );
                    assert_eq!(first.status.as_deref(), Some("rejected"));
                    assert_eq!(second.as_ref(), Some(&first));
                    assert_eq!(rewritten.accounts[0].quota, first);
                }
                Expectation::Removed => {
                    assert_eq!(first, QuotaState::default(), "{label} is normalized away");
                    assert!(second.is_none(), "{label} stays removed after v3 restore");
                    assert!(
                        rewritten.accounts.is_empty(),
                        "{label} is absent from v3 state"
                    );
                }
                Expectation::SurvivesWithPastWindowResetCleared => {
                    assert_eq!(first.status.as_deref(), Some("rejected"));
                    assert_eq!(first.observed_at_status, Some(before.saturating_sub(60)));
                    assert_eq!(first.reset_5h, None);
                    assert_eq!(second.as_ref(), Some(&first));
                    assert_eq!(rewritten.accounts[0].quota, first);
                }
            }
            remove_test_dir(&path);
        }
    }

    #[tokio::test]
    async fn restore_v3_resetless_status_stays_resetless_after_reset_only_roundtrip() {
        let path = temp_file("v3-resetless-status");
        let account = account("acct-a");
        let observed = unix_now();
        save(
            &path,
            &PersistedPool {
                version: STATE_VERSION,
                accounts: vec![PersistedAccount {
                    key: crate::accounts::account_key("anthropic", &account),
                    quota: QuotaState {
                        status_5h: Some("rejected".to_string()),
                        observed_at_status_5h: Some(observed),
                        ..Default::default()
                    },
                }],
            },
        )
        .expect("save succeeds");
        let state = state_with_path(path.clone());
        restore(&state).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            HeaderValue::from_str(&(observed + 3_600).to_string()).unwrap(),
        );
        state.accounts.note_quota("anthropic", &account, &headers);
        let key = crate::accounts::account_key("anthropic", &account);
        let (_, quota) = state.accounts.raw_quota_for_test(&key).unwrap();
        assert_eq!(quota.observed_at_status_5h, Some(observed));
        assert_eq!(quota.reset_at_status_5h, None);
        state.accounts.mark_dirty();
        flush(&state).await;

        let restored = state_with_path(path.clone());
        restore(&restored).await;
        let (_, quota) = restored.accounts.raw_quota_for_test(&key).unwrap();
        assert_eq!(quota.observed_at_status_5h, Some(observed));
        assert_eq!(quota.reset_at_status_5h, None);
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn restore_expires_stale_legacy_quota_before_first_snapshot() {
        let path = temp_file("expired");
        let expired = PersistedPool {
            version: STATE_VERSION,
            accounts: vec![PersistedAccount {
                key: crate::accounts::account_key("anthropic", &account("acct-a")),
                quota: QuotaState {
                    utilization_5h: Some(1.0),
                    reset_5h: Some(unix_now().saturating_sub(1)),
                    status: Some("rejected".to_string()),
                    ..Default::default()
                },
            }],
        };
        save(&path, &expired).expect("save succeeds");
        let state = state_with_path(path.clone());
        restore(&state).await;

        // Import must remove the past-reset window and its unstamped legacy
        // aggregate before any selection, snapshot, or export sweep runs.
        let key = crate::accounts::account_key("anthropic", &account("acct-a"));
        let (observed, raw_quota) = state
            .accounts
            .raw_quota_for_test(&key)
            .expect("restored account entry exists");
        assert!(observed, "restored account is marked observed");
        assert_eq!(raw_quota, QuotaState::default());
        assert!(
            state.accounts.take_dirty(),
            "import correction marks persistence dirty"
        );
        // The assertion above consumes the flag so this test can prove the
        // flush path with the same dirty marker the restore normally leaves.
        state.accounts.mark_dirty();
        assert!(
            state.accounts.export_quotas().is_empty(),
            "expired quota is absent immediately after restore"
        );
        let snapshots = state
            .accounts
            .snapshot("anthropic", &[account("acct-a")], None, None);

        assert!(snapshots[0].has_state);
        assert!(snapshots[0].available, "stale quota must not avoid account");
        assert!(!snapshots[0].near_quota);
        assert_eq!(snapshots[0].utilization_5h, None);
        assert_eq!(snapshots[0].reset_5h, None);
        assert_eq!(snapshots[0].status, None);

        // The import correction marks persistence dirty. The next flush must
        // rewrite the file without the stale account, and a fresh restore must
        // observe the cleaned disk state.
        flush(&state).await;
        let persisted = load(&path)
            .expect("load succeeds")
            .expect("corrected state remains loadable");
        assert!(
            persisted.accounts.is_empty(),
            "dirty flush removes the expired quota record"
        );

        let restored_again = state_with_path(path.clone());
        restore(&restored_again).await;
        assert!(restored_again.accounts.export_quotas().is_empty());
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn restore_bounds_reset_less_quota() {
        // A state file written before observed_at_* existed carries a
        // reset-less window with no observation timestamp at all. Restoring
        // it must not leave that window unstamped: without a bound, it would
        // read as "never observed," and for a reset-less mark
        // `expire_stale_quota` treats that as expired immediately, defeating
        // the warm start this persistence feature exists for.
        let path = temp_file("legacy-reset-less");
        let legacy_key = crate::accounts::account_key("anthropic", &account("acct-a"));
        let future_key = crate::accounts::account_key("anthropic", &account("acct-b"));
        let before = unix_now();
        let future = before + 86_400;
        let legacy = PersistedPool {
            version: STATE_VERSION,
            accounts: vec![
                PersistedAccount {
                    key: legacy_key.clone(),
                    quota: QuotaState {
                        utilization_7d: Some(0.9),
                        ..Default::default()
                    },
                },
                PersistedAccount {
                    key: future_key.clone(),
                    quota: QuotaState {
                        utilization_5h: Some(0.1),
                        utilization_7d: Some(0.2),
                        utilization_7d_oi: Some(0.3),
                        status_5h: Some("allowed".to_string()),
                        status_7d: Some("allowed".to_string()),
                        status_7d_oi: Some("allowed".to_string()),
                        status: Some("allowed".to_string()),
                        observed_at_5h: Some(future),
                        observed_at_7d: Some(future),
                        observed_at_7d_oi: Some(future),
                        observed_at_status_5h: Some(future),
                        observed_at_status_7d: Some(future),
                        observed_at_status_7d_oi: Some(future),
                        observed_at_status: Some(future),
                        ..Default::default()
                    },
                },
            ],
        };
        save(&path, &legacy).expect("save succeeds");
        let state = state_with_path(path.clone());

        restore(&state).await;
        // Flush immediately: snapshot/select_order can expire a reset-less
        // fixture and erase its observation stamp, causing a spurious failure.
        flush(&state).await;

        let persisted = load(&path)
            .expect("load succeeds")
            .expect("corrected state remains loadable");
        let legacy_quota = &persisted
            .accounts
            .iter()
            .find(|account| account.key == legacy_key)
            .expect("legacy account persisted")
            .quota;
        let migration_time = legacy_quota
            .observed_at_7d
            .expect("legacy observation time persisted");
        assert!(
            migration_time >= before && migration_time <= unix_now(),
            "a restored legacy window is backdated to boot time, not left unstamped"
        );
        let future_quota = &persisted
            .accounts
            .iter()
            .find(|account| account.key == future_key)
            .expect("future-stamped account persisted")
            .quota;
        for observed_at in [
            future_quota.observed_at_5h,
            future_quota.observed_at_7d,
            future_quota.observed_at_7d_oi,
            future_quota.observed_at_status_5h,
            future_quota.observed_at_status_7d,
            future_quota.observed_at_status_7d_oi,
            future_quota.observed_at_status,
        ] {
            assert_eq!(observed_at, Some(migration_time));
        }

        let restored_again = state_with_path(path.clone());
        restore(&restored_again).await;
        let exported = restored_again.accounts.export_quotas();
        let legacy_quota = &exported
            .iter()
            .find(|(key, _)| key == &legacy_key)
            .expect("legacy account restored again")
            .1;
        assert_eq!(legacy_quota.observed_at_7d, Some(migration_time));
        assert!(
            !restored_again.accounts.take_dirty(),
            "a second restore must keep the original migration time"
        );
        remove_test_dir(&path);
    }
}
