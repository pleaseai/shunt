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
    time::Duration,
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
///
/// Under the same lock [`super::auth::resolve_token_bounded`] and
/// [`super::login::run`] take, because logout, login, and refresh all contend
/// for the same file: an unlocked unlink can land *between* a refresher's token
/// POST and its writeback, and the writeback then resurrects a session the user
/// just signed out of.
pub async fn remove_session(path: &Path) -> anyhow::Result<bool> {
    let _lock = lock_session(path).await?;
    let removed = match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", path.display()))
        }
    };
    // The lock file is deliberately left behind. It holds no state — it is
    // empty, and only its inode matters — and a stable inode is exactly what
    // keeps logout, login, and refresh serialized against each other: unlinking
    // it would let an in-flight holder keep a lock on the old inode while the
    // next process locks a freshly created one, so the two would no longer
    // exclude each other at all.
    Ok(removed)
}

/// Held for the read -> refresh -> write critical section. Dropping it releases
/// the advisory lock.
///
/// An alias for the shared [`shared::file_lock::FileLock`]: the mechanism moved
/// there when the Antigravity credential store needed the same guard (#384),
/// and this name stays so the gateway's own call sites still read in terms of
/// the session.
pub type SessionLock = shared::file_lock::FileLock;

/// Diagnostics for the gateway session lock. The wording is the gateway's own;
/// only the `flock` mechanics are shared.
static GATEWAY_SESSION_LOCK: shared::file_lock::FileLockKind = shared::file_lock::FileLockKind {
    lock_name: "gateway session lock",
    contention_hint: "Another `shunt gateway token` is holding it — most likely one whose gateway \
                      accepted the connection and never answered. The lock releases when that \
                      process exits, so wait for it and retry. Do not delete the lock file: \
                      removing it while a writer holds it lets the next writer lock a new inode \
                      and serialize against nothing",
    task_context: "gateway session lock task failed",
    unsupported_warning: "Warning: this platform has no advisory file lock, so concurrent `shunt \
                          gateway token` runs are not serialized. If two run at once they can \
                          replay the same single-use refresh token, which signs this machine out \
                          of the gateway; run `shunt gateway login <url>` again if that happens.",
    #[cfg(not(unix))]
    warned: std::sync::Once::new(),
};

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
///
/// Three operations serialize on it — logout, login, and refresh — because all
/// three replace or remove the same file. A login that stored its new session
/// without the lock would be undone by an in-flight refresh's writeback landing
/// after it, exactly as an unlocked logout would be.
///
/// Acquisition is bounded (`LOCK_TIMEOUT`) rather than an unconditional
/// `LOCK_EX`. A holder blocked on a gateway that accepts the connection and
/// never answers would otherwise stall every other `apiKeyHelper` on the
/// machine indefinitely and silently — one unreachable deployment taking down
/// every Claude Code session. Timing out turns that into a reported failure on
/// one session instead of a hang on all of them.
pub async fn lock_session(path: &Path) -> anyhow::Result<SessionLock> {
    lock_session_for(path, LOCK_TIMEOUT).await
}

/// [`lock_session`] with an explicit bound, so tests can drive the expiry path
/// without waiting [`LOCK_TIMEOUT`] out.
pub(crate) async fn lock_session_for(
    path: &Path,
    timeout: Duration,
) -> anyhow::Result<SessionLock> {
    shared::file_lock::lock_file(path, &GATEWAY_SESSION_LOCK, timeout).await
}

/// Only the tests name the lock file directly; production code reaches it
/// through the shared module.
#[cfg(test)]
fn lock_path(path: &Path) -> PathBuf {
    shared::file_lock::lock_path(path)
}

/// Slack the waiter keeps beyond the worst-case legitimate hold, covering the
/// session write and scheduling jitter.
const LOCK_HEADROOM_SECS: u64 = 30;
/// How long a caller waits for the refresh lock before giving up.
///
/// Derived from [`super::auth::NETWORK_TIMEOUT`] rather than chosen
/// independently, so the two cannot drift apart: a legitimate holder makes two
/// separately bounded round-trips (discovery, then the token POST), so its
/// worst case is twice that budget, and the waiter needs headroom *beyond* it.
/// At exactly 2x, a holder using its full budget would expire the waiter at the
/// same instant it succeeded, reporting contention for a refresh that worked.
pub(crate) const LOCK_TIMEOUT: Duration =
    Duration::from_secs(2 * super::auth::NETWORK_TIMEOUT.as_secs() + LOCK_HEADROOM_SECS);

/// How many callers are parked waiting for `path`'s session lock right now.
///
/// Takes the *session* path, not the lock path, so callers cannot disagree with
/// [`lock_path`] about which file the count is keyed by.
#[cfg(test)]
pub(crate) fn waiters_blocked_on(path: &Path) -> usize {
    shared::file_lock::waiters_blocked_on(path)
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

    #[tokio::test]
    async fn remove_session_is_idempotent() {
        let dir = temp_dir("logout");
        let path = dir.join("session.json");
        write_session(&path, &test_session("https://gateway.example", 0)).unwrap();

        assert!(
            remove_session(&path).await.unwrap(),
            "first removal deletes it"
        );
        assert!(!path.exists());
        assert!(
            !remove_session(&path).await.unwrap(),
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

    /// The lock actually excludes, rather than merely being taken. Real time,
    /// not a paused clock: `lock_blocking` blocks a `spawn_blocking` thread in
    /// `flock(2)`, which a virtual clock cannot advance through.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_held_session_lock_blocks_the_next_acquisition_until_it_drops() {
        use std::time::Duration;

        let dir = temp_dir("lock-exclusion");
        let path = dir.join("session.json");
        let held = lock_session(&path).await.expect("first acquisition");

        // A channel rather than a `Barrier`: a panic on either side of a
        // barrier deadlocks teardown, while a dropped sender just ends the
        // receive.
        let (acquired_tx, mut acquired_rx) = tokio::sync::mpsc::channel::<()>(1);
        let waiter_path = path.clone();
        let waiter = tokio::spawn(async move {
            let lock = lock_session(&waiter_path)
                .await
                .expect("second acquisition");
            let _ = acquired_tx.send(()).await;
            lock
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(500), acquired_rx.recv())
                .await
                .is_err(),
            "the second acquisition completed while the first lock was still held, so the lock \
             excludes nothing"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(10), acquired_rx.recv())
            .await
            .expect("dropping the guard must release the lock")
            .expect("the waiter must report its acquisition");
        drop(waiter.await.expect("waiter task"));

        let _ = fs::remove_dir_all(dir);
    }

    /// A wedged holder must be reported, not waited on forever: an unbounded
    /// `LOCK_EX` behind one unreachable gateway hangs every `apiKeyHelper` on
    /// the machine with nothing on stderr to explain it.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_for_a_stuck_holder_times_out_and_names_the_lock() {
        let dir = temp_dir("lock-timeout");
        let path = dir.join("session.json");
        let held = lock_session(&path).await.expect("first acquisition");

        let started = std::time::Instant::now();
        let error = lock_session_for(&path, Duration::from_millis(300))
            .await
            .expect_err("a held lock must time out rather than block forever");
        let message = error.to_string();
        assert!(message.contains("timed out"), "got: {message}");
        assert!(
            message.contains(&lock_path(&path).display().to_string()),
            "the message must name the lock file so it can be found: {message}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the bound must actually bound: waited {:?}",
            started.elapsed()
        );

        drop(held);
        let _ = fs::remove_dir_all(dir);
    }

    /// The lock file outlives the session on purpose: it is the inode logout,
    /// login, and refresh serialize on, so unlinking it would let a holder of
    /// the old inode run alongside a process that locked a new one.
    ///
    /// Unix only: off Unix `lock_blocking` is a documented no-op that creates
    /// no file at all, so there is no inode for these assertions to find.
    #[cfg(unix)]
    #[tokio::test]
    async fn logout_keeps_the_lock_file_so_later_runs_still_serialize() {
        let dir = temp_dir("logout-lock");
        let path = dir.join("session.json");
        write_session(&path, &test_session("https://gateway.example", 0)).unwrap();
        // Taking and releasing the lock is what creates the sibling file.
        drop(lock_session(&path).await.unwrap());
        assert!(lock_path(&path).exists(), "the lock file should exist now");

        assert!(remove_session(&path).await.unwrap());
        assert!(!path.exists());
        assert!(
            lock_path(&path).exists(),
            "logout must leave the lock inode in place, or the next login and an in-flight \
             refresh lock different inodes and stop excluding each other"
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// The race logout used to lose: an unlocked unlink lands while a refresh
    /// holds the lock, and the refresh's writeback then puts the session back
    /// after logout already reported success.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn logout_waits_for_an_in_flight_refresh_instead_of_racing_it() {
        let dir = temp_dir("logout-race");
        let path = dir.join("session.json");
        write_session(&path, &test_session("https://gateway.example", 0)).unwrap();

        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let refresher_path = path.clone();
        let refresher = tokio::spawn(async move {
            let lock = lock_session(&refresher_path)
                .await
                .expect("the refresher takes the lock first");
            held_tx.send(()).expect("signal that the lock is held");
            // Wide enough that a logout which does not take the lock is
            // guaranteed to unlink before this writeback, rather than usually.
            tokio::time::sleep(Duration::from_millis(300)).await;
            write_session(&refresher_path, &test_session("https://gateway.example", 1)).unwrap();
            drop(lock);
        });

        held_rx.await.expect("the refresher must acquire the lock");
        remove_session(&path).await.unwrap();
        refresher.await.expect("refresher task");

        assert!(
            !path.exists(),
            "logout unlinked the session while a refresh held the lock, and the refresh then \
             wrote it straight back"
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// The waiter registry must count a caller that is genuinely parked, and
    /// nothing else. `login_waits_for_an_in_flight_refresh_before_storing_the_session`
    /// reads a zero here as "the login is not blocked", so a registry that
    /// over- or under-counts would quietly change what that test proves.
    ///
    /// Pins all three properties that matter: the uncontended acquire registers
    /// nobody, a contended one registers exactly one waiter, and the count is
    /// scoped to its own lock path rather than shared with a sibling.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_waiter_registry_counts_only_callers_parked_on_that_lock() {
        use std::time::Duration;

        let dir = temp_dir("waiter-registry");
        let path = dir.join("session.json");
        // A second session in the same directory, so a registry keyed by
        // something coarser than the lock path (a global count, or the parent
        // directory) fails this test rather than passing it by luck.
        let sibling = dir.join("other-session.json");

        let held = lock_session(&path).await.expect("first acquisition");
        assert_eq!(
            waiters_blocked_on(&path),
            0,
            "an uncontended acquisition never waited, so it must register no waiter"
        );

        let waiter_path = path.clone();
        let waiter =
            tokio::spawn(
                async move { lock_session_for(&waiter_path, Duration::from_secs(10)).await },
            );

        // Polled rather than slept on: the waiter registers as soon as its
        // first non-blocking acquire fails, and this test should not encode a
        // guess about when that is.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while waiters_blocked_on(&path) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a caller blocked on a held lock was never registered as a waiter"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            waiters_blocked_on(&path),
            1,
            "one caller is blocked, so the count must be exactly one"
        );
        assert_eq!(
            waiters_blocked_on(&sibling),
            0,
            "the waiter is parked on {}, so a different session's lock must show none",
            path.display()
        );

        drop(held);
        waiter
            .await
            .expect("waiter task")
            .expect("the waiter acquires once the holder releases");
        // Deregistration is the half a paired register/unregister would drop:
        // a phantom waiter here would make a later test read a stale block.
        assert_eq!(
            waiters_blocked_on(&path),
            0,
            "the waiter acquired the lock, so it is no longer waiting"
        );

        let _ = fs::remove_dir_all(dir);
    }
}
