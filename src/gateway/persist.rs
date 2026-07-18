//! Opt-in on-disk persistence of gateway-login refresh sessions (issue #194).
//!
//! When `[server.gateway] state_path` is set, shunt writes the refresh-token
//! store — active sessions and replay tombstones, tokens as SHA-256 hashes —
//! to that file after every mutation of the store, and restores it at the next
//! boot. A restart then keeps managed logins alive: Claude Code silently
//! refreshes into a rotated token instead of forcing every user back through
//! browser sign-in once their access JWT expires.
//!
//! The file never holds a usable credential (only token hashes plus the
//! identity metadata needed to re-mint access JWTs), and is written atomically
//! with owner-only permissions. Like the pool quota cache, reading it is
//! best-effort: a missing, corrupt, or version-mismatched file costs the old
//! memory-only behavior (users sign in again), never a boot failure.
//!
//! Device grants and rate-limit counters stay memory-only by design — they are
//! short-lived, so a restart only costs an in-flight login attempt. Sharing
//! this file between concurrent gateway processes is not supported;
//! multi-instance session/replay coordination remains a follow-up.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::server::AppState;

use super::{approval::Identity, refresh::RefreshRecord};

/// Bump when the on-disk shape changes incompatibly; a file whose version does
/// not match is ignored (sessions reset) rather than mis-parsed.
const STATE_VERSION: u32 = 1;

/// On-disk envelope: a version tag plus one entry per refresh-token record.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedSessions {
    version: u32,
    refresh_tokens: Vec<PersistedRefreshToken>,
}

/// One refresh-token record. `token_sha256` is the store key — the opaque
/// token itself is never written.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedRefreshToken {
    token_sha256: String,
    identity: Identity,
    family: String,
    #[serde(default)]
    inactive_since: Option<u64>,
    issued_at: u64,
}

impl From<RefreshRecord> for PersistedRefreshToken {
    fn from(record: RefreshRecord) -> Self {
        Self {
            token_sha256: record.token_sha256,
            identity: record.identity,
            family: record.family,
            inactive_since: record.inactive_since,
            issued_at: record.issued_at,
        }
    }
}

impl From<PersistedRefreshToken> for RefreshRecord {
    fn from(persisted: PersistedRefreshToken) -> Self {
        Self {
            token_sha256: persisted.token_sha256,
            identity: persisted.identity,
            family: persisted.family,
            inactive_since: persisted.inactive_since,
            issued_at: persisted.issued_at,
        }
    }
}

/// The configured state file, or `None` when persistence is disabled
/// (`state_path = ""`, or no resolvable home directory for the default).
fn state_path(state: &AppState) -> Option<&Path> {
    state.config.server.gateway.as_ref()?.session_state_path()
}

/// Restore refresh sessions from disk at boot. A no-op when `state_path` is
/// unset or the file is absent/unreadable/incompatible — every failure mode
/// falls back to the memory-only behavior, never a boot error. Call once
/// before serving requests so the first refresh grant already sees the
/// restored sessions.
pub async fn restore(state: &AppState) {
    let Some(path) = state_path(state).map(Path::to_path_buf) else {
        return;
    };
    let load_path = path.clone();
    let result = tokio::task::spawn_blocking(move || load(&load_path)).await;
    match result {
        Ok(Ok(Some(persisted))) => {
            let count = persisted.refresh_tokens.len();
            state.gateway_stores.refresh_tokens.import(
                persisted
                    .refresh_tokens
                    .into_iter()
                    .map(RefreshRecord::from),
            );
            tracing::info!(
                path = %path.display(),
                refresh_tokens = count,
                "restored gateway login sessions from disk"
            );
        }
        // Absent file or version/parse mismatch: nothing to restore.
        Ok(Ok(None)) => {}
        Ok(Err(error)) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to read gateway session state file; starting without sessions"
        ),
        Err(error) => tracing::warn!(%error, "gateway session restore task panicked"),
    }
}

/// Write the refresh-token store to disk if it changed. Called by the token
/// endpoint after grants, so a successful login or rotation is on disk before
/// the response is sent — a crash immediately after still finds the session.
/// A failed write re-marks the store dirty and the next mutation retries; the
/// file is stale until then, which at worst re-runs one browser sign-in.
pub async fn save_if_dirty(state: &AppState) {
    let Some(path) = state_path(state).map(Path::to_path_buf) else {
        return;
    };
    if !state.gateway_stores.refresh_tokens.take_dirty() {
        return;
    }
    let stores = state.gateway_stores.clone();
    // Serialization + the filesystem write are blocking; keep them off the
    // async worker. The snapshot itself briefly locks the store in the task.
    let result = tokio::task::spawn_blocking(move || {
        // The gate spans export through the completed write: `take_dirty`
        // above only claims the flag, so without it two concurrent grants
        // could export in one order and rename in the other, persisting a
        // pre-revocation snapshot over a newer one.
        let _gate = stores.refresh_tokens.persist_gate();
        let persisted = PersistedSessions {
            version: STATE_VERSION,
            refresh_tokens: stores
                .refresh_tokens
                .export()
                .into_iter()
                .map(PersistedRefreshToken::from)
                .collect(),
        };
        save(&path, &persisted)
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            state.gateway_stores.refresh_tokens.mark_dirty();
            tracing::warn!(%error, "failed to persist gateway login sessions");
        }
        Err(error) => {
            state.gateway_stores.refresh_tokens.mark_dirty();
            tracing::warn!(%error, "gateway session persister task panicked");
        }
    }
}

/// Read and validate the state file. `Ok(None)` covers every recoverable case
/// (absent file, invalid JSON, version mismatch) so the caller can start
/// without sessions; `Err` is reserved for unexpected I/O errors worth
/// surfacing.
fn load(path: &Path) -> io::Result<Option<PersistedSessions>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let persisted: PersistedSessions = match serde_json::from_slice(&bytes) {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "gateway session state file is not valid json; ignoring"
            );
            return Ok(None);
        }
    };
    if persisted.version != STATE_VERSION {
        tracing::warn!(
            path = %path.display(),
            found = persisted.version,
            expected = STATE_VERSION,
            "gateway session state file version mismatch; ignoring"
        );
        return Ok(None);
    }
    Ok(Some(persisted))
}

/// Write the state atomically via [`crate::atomic_file::write_private_atomic`].
fn save(path: &Path, sessions: &PersistedSessions) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(sessions).map_err(io::Error::other)?;
    crate::atomic_file::write_private_atomic(path, &json)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn identity() -> Identity {
        Identity {
            sub: "dev@example.com".into(),
            email: "dev@example.com".into(),
            name: "dev".into(),
        }
    }

    fn sample_sessions() -> PersistedSessions {
        PersistedSessions {
            version: STATE_VERSION,
            refresh_tokens: vec![
                PersistedRefreshToken {
                    token_sha256: "a".repeat(64),
                    identity: identity(),
                    family: "family-a".into(),
                    inactive_since: None,
                    issued_at: 1_000_000,
                },
                // A replay tombstone in the same family: superseded by rotation,
                // but retained so a later replay of it is still caught.
                PersistedRefreshToken {
                    token_sha256: "b".repeat(64),
                    identity: identity(),
                    family: "family-a".into(),
                    inactive_since: Some(1_000_500),
                    issued_at: 1_000_000,
                },
            ],
        }
    }

    fn temp_file(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shunt-gateway-persist-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create test directory");
        dir.join("sessions.json")
    }

    fn remove_test_dir(path: &Path) {
        fs::remove_dir_all(path.parent().expect("test path has parent")).ok();
    }

    #[test]
    fn save_then_load_round_trips_sessions() {
        let path = temp_file("roundtrip");
        save(&path, &sample_sessions()).expect("save succeeds");

        let loaded = load(&path).expect("load succeeds").expect("file present");
        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.refresh_tokens.len(), 2);
        let record = &loaded.refresh_tokens[0];
        assert_eq!(record.token_sha256, "a".repeat(64));
        assert_eq!(record.identity, identity());
        assert_eq!(record.family, "family-a");
        assert_eq!(record.inactive_since, None);
        assert_eq!(record.issued_at, 1_000_000);
        let tombstone = &loaded.refresh_tokens[1];
        assert_eq!(tombstone.token_sha256, "b".repeat(64));
        assert_eq!(tombstone.identity, identity());
        assert_eq!(tombstone.family, "family-a");
        assert_eq!(
            tombstone.inactive_since,
            Some(1_000_500),
            "a replay tombstone's inactive_since must round-trip through JSON"
        );
        assert_eq!(tombstone.issued_at, 1_000_000);
        remove_test_dir(&path);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_file("permissions");
        save(&path, &sample_sessions()).expect("save succeeds");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        remove_test_dir(&path);
    }

    #[tokio::test]
    async fn failed_save_keeps_sessions_dirty_for_retry() {
        let path = temp_file("save-failure");
        fs::create_dir(&path).expect("target directory makes rename fail");

        let secret_env = format!("SHUNT_GATEWAY_TEST_PERSIST_SECRET_{}", std::process::id());
        let users_env = format!("SHUNT_GATEWAY_TEST_PERSIST_USERS_{}", std::process::id());
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&users_env, "dev@example.com:password");

        let mut config = crate::config::Config::default();
        config.server.gateway = Some(crate::config::GatewayConfig {
            public_url: "https://gateway.example".into(),
            jwt_secret_env: secret_env.clone(),
            users_env: users_env.clone(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: Some(path.clone()),
        });
        let state = AppState::new(config, reqwest::Client::new()).expect("gateway config resolves");

        state.gateway_stores.refresh_tokens.issue(identity());

        save_if_dirty(&state).await;

        assert!(
            state.gateway_stores.refresh_tokens.take_dirty(),
            "failed save must be retried"
        );
        let entries = fs::read_dir(path.parent().unwrap())
            .expect("read test directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read entries");
        assert_eq!(entries.len(), 1, "failed save must clean up its temp file");
        assert_eq!(entries[0].path(), path);

        std::env::remove_var(&secret_env);
        std::env::remove_var(&users_env);
        remove_test_dir(&path);
    }

    #[test]
    fn load_missing_corrupt_or_version_mismatched_file_yields_none() {
        for (label, contents) in [
            ("missing", None),
            ("corrupt", Some(b"{ this is not json".to_vec())),
            (
                "version",
                Some(
                    format!(
                        "{{\"version\":{},\"refresh_tokens\":[]}}",
                        STATE_VERSION + 1
                    )
                    .into_bytes(),
                ),
            ),
        ] {
            let path = temp_file(label);
            if let Some(contents) = contents {
                fs::write(&path, contents).expect("write invalid state file");
            }
            assert!(
                load(&path).expect("load succeeds").is_none(),
                "{label} file must be ignored"
            );
            remove_test_dir(&path);
        }
    }
}
