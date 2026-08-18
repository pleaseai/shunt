//! The on-disk session for a logged-in shunt gateway.
//!
//! One file, one deployment: `$SHUNT_GATEWAY_SESSION_FILE`, else
//! `$HOME/.shunt/gateway/session.json`, else a working-directory-relative
//! `.shunt/gateway/session.json`. Written born-private (directory `0700`, file
//! `0600`) and atomically, in the nested-camelCase shape the Claude and Kimi
//! account stores use:
//!
//! ```json
//! {"gatewaySession": {"gatewayUrl": "...", "accessToken": "...",
//!                     "refreshToken": "...", "expiresAt": 1750000000000}}
//! ```
//!
//! `expiresAt` is epoch **milliseconds**, matching those stores.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context};
use serde_json::{json, Value};

use crate::auth::shared;

/// Path override for the session file. Deliberately *not* `SHUNT_GATEWAY_TOKEN`:
/// despite its name that variable is the static override for `shunt token`
/// (see [`crate::auth::claude::auth::static_override`]), and reusing it here
/// would silently change that command's behavior.
const SESSION_FILE_ENV: &str = "SHUNT_GATEWAY_SESSION_FILE";

#[derive(Clone, PartialEq, Eq)]
pub struct GatewaySession {
    /// Base URL the session was issued by; discovery is re-resolved from it on
    /// every refresh rather than caching a token endpoint that may move.
    pub gateway_url: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
}

/// Redacting, deliberately: the derived form would print the token pair through
/// any `unwrap`/`assert_eq!` panic message, and this type is compared in tests.
impl std::fmt::Debug for GatewaySession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewaySession")
            .field("gateway_url", &self.gateway_url)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

pub fn session_path() -> PathBuf {
    session_path_from(
        shared::env_path_override(SESSION_FILE_ENV),
        shared::home_dir(),
    )
}

/// The resolution order, split out from the process environment so it can be
/// tested without mutating `HOME` — which is global to the test binary and
/// would race sibling tests that read it.
fn session_path_from(override_path: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    override_path
        .or_else(|| home.map(|home| home.join(".shunt").join("gateway").join("session.json")))
        .unwrap_or_else(|| PathBuf::from(".shunt/gateway/session.json"))
}

/// Read the stored session. `Ok(None)` means "no session file" (not logged in);
/// a file that exists but cannot be parsed is an error, not a silent `None` —
/// otherwise a corrupted session would read as a fresh machine and the caller
/// would report the wrong remedy.
pub fn read_session(path: &Path) -> anyhow::Result<Option<GatewaySession>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    parse_session(&value)
        .map(Some)
        .ok_or_else(|| anyhow!(
            "{} is missing gatewaySession.gatewayUrl / accessToken / refreshToken; run `shunt gateway login <url>` again",
            path.display()
        ))
}

fn parse_session(value: &Value) -> Option<GatewaySession> {
    let session = value.get("gatewaySession")?;
    let field = |name: &str| {
        session
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    Some(GatewaySession {
        gateway_url: field("gatewayUrl")?,
        access_token: field("accessToken")?,
        refresh_token: field("refreshToken")?,
        // A missing expiry reads as "already expired" so the next call
        // refreshes rather than presenting a token of unknown age.
        expires_at_ms: session
            .get("expiresAt")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

pub fn write_session(path: &Path, session: &GatewaySession) -> anyhow::Result<()> {
    let value = json!({
        "gatewaySession": {
            "gatewayUrl": session.gateway_url,
            "accessToken": session.access_token,
            "refreshToken": session.refresh_token,
            "expiresAt": session.expires_at_ms
        }
    });
    shared::write_account_file(path, &value)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Remove the session file. `Ok(false)` means there was nothing to remove, so
/// `shunt gateway logout` is idempotent.
pub fn remove_session(path: &Path) -> anyhow::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Held for the read -> refresh -> write critical section. Dropping it releases
/// the advisory lock.
pub struct SessionLock {
    #[cfg(unix)]
    file: fs::File,
}

#[cfg(unix)]
impl Drop for SessionLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Closing the descriptor would release the lock anyway; unlocking
        // explicitly keeps the release ordered with respect to the writeback
        // that just happened rather than with an implicit close.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Take an exclusive advisory lock over the session for this deployment.
///
/// Claude Code re-runs `apiKeyHelper` on every 401 and caches it per process,
/// so two concurrent sessions can call `shunt gateway token` at the same
/// moment, both read the same refresh token, and both POST it — and because the
/// gateway's refresh tokens are single-use, the loser's replay revokes the
/// whole family and logs the user out everywhere. An in-process mutex cannot
/// see across processes, so the lock has to live on the filesystem.
///
/// The lock is taken on a sibling `.lock` file rather than on the session
/// itself: writeback renames a temp file over the session, so its inode
/// changes and a lock held on the old inode would guard nothing.
pub async fn lock_session(path: &Path) -> anyhow::Result<SessionLock> {
    let lock_path = lock_path(path);
    tokio::task::spawn_blocking(move || lock_blocking(&lock_path))
        .await
        .context("gateway session lock task failed")?
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

#[cfg(unix)]
fn lock_blocking(lock_path: &Path) -> anyhow::Result<SessionLock> {
    use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};

    if let Some(parent) = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        shared::create_private_dir(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    // Blocking acquire: the holder is doing one token refresh, so waiting is
    // both short and exactly what the waiter wants — it re-reads afterwards and
    // usually finds the refreshed token already on disk.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", lock_path.display()));
    }
    Ok(SessionLock { file })
}

/// Documented no-op off Unix: `flock(2)` has no `std` equivalent there, so the
/// concurrent-refresh guard degrades to nothing rather than failing to build.
/// Two simultaneous `shunt gateway token` runs on such a platform can still
/// race each other into a single-use refresh-token replay.
#[cfg(not(unix))]
fn lock_blocking(_lock_path: &Path) -> anyhow::Result<SessionLock> {
    Ok(SessionLock {})
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    std::env::temp_dir().join(format!(
        "shunt-gateway-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(test)]
pub(crate) fn test_session(gateway_url: &str, expires_at_ms: i64) -> GatewaySession {
    GatewaySession {
        gateway_url: gateway_url.to_string(),
        access_token: "access-1".to_string(),
        refresh_token: "refresh-1".to_string(),
        expires_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_path_prefers_the_override_then_home_then_the_cwd() {
        let home = PathBuf::from("/home/dev");
        assert_eq!(
            session_path_from(Some(PathBuf::from("/tmp/custom.json")), Some(home.clone())),
            PathBuf::from("/tmp/custom.json")
        );
        assert_eq!(
            session_path_from(None, Some(home)),
            PathBuf::from("/home/dev/.shunt/gateway/session.json")
        );
        assert_eq!(
            session_path_from(None, None),
            PathBuf::from(".shunt/gateway/session.json")
        );
    }

    #[tokio::test]
    async fn a_blank_session_file_override_is_treated_as_unset() {
        let _guard = TEST_ENV_LOCK.lock().await;
        {
            let _blank = shared::EnvVarGuard::set(SESSION_FILE_ENV, "   ");
            assert_eq!(
                shared::env_path_override(SESSION_FILE_ENV),
                None,
                "a whitespace-only override must not resolve to a cwd-relative path"
            );
        }
        let _set = shared::EnvVarGuard::set(SESSION_FILE_ENV, "/tmp/custom.json");
        assert_eq!(
            shared::env_path_override(SESSION_FILE_ENV),
            Some(PathBuf::from("/tmp/custom.json"))
        );
    }

    #[test]
    fn round_trips_a_session_and_reports_a_missing_file_as_absent() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("session.json");
        assert_eq!(read_session(&path).unwrap(), None);

        let session = test_session("https://gateway.example", 4_000_000_000_000);
        write_session(&path, &session).unwrap();
        assert_eq!(read_session(&path).unwrap(), Some(session));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_present_but_malformed_session_is_an_error_not_absence() {
        let dir = temp_dir("malformed");
        let path = dir.join("session.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, br#"{"gatewaySession":{"gatewayUrl":"https://g"}}"#).unwrap();

        let error = read_session(&path).expect_err("a half-written session must not read as None");
        assert!(
            error.to_string().contains("shunt gateway login"),
            "got: {error}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn session_file_is_0600_inside_a_0700_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("perms");
        let path = dir.join("gateway").join("session.json");
        write_session(&path, &test_session("https://gateway.example", 0)).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_session_is_idempotent() {
        let dir = temp_dir("logout");
        let path = dir.join("session.json");
        write_session(&path, &test_session("https://gateway.example", 0)).unwrap();

        assert!(remove_session(&path).unwrap(), "first removal deletes it");
        assert!(!path.exists());
        assert!(
            !remove_session(&path).unwrap(),
            "removing an absent session must succeed and report nothing removed"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lock_path_is_a_sibling_of_the_session_file() {
        assert_eq!(
            lock_path(Path::new("/tmp/shunt/session.json")),
            PathBuf::from("/tmp/shunt/session.json.lock")
        );
    }
}
