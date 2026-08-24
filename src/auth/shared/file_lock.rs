//! Advisory `flock(2)` over a credential/session file, on a sibling `.lock`.
//!
//! Extracted verbatim from the gateway session store, which needed it first
//! (two `shunt gateway token` runs replaying one single-use refresh token), and
//! generalized because the Antigravity credential file has the same shape of
//! race: `shunt login antigravity` rewrites it from a **separate process**
//! while the gateway is serving requests, so no in-process mutex can exclude
//! it.
//!
//! The lock is taken on a sibling `<file>.lock` rather than on the file itself:
//! every writeback here renames a temp file over the target, so its inode
//! changes and a lock held on the old inode would guard nothing. That also
//! means the `.lock` file must never be unlinked — a stable inode is the whole
//! mechanism.
//!
//! Acquisition is always bounded rather than an unconditional `LOCK_EX`: a
//! wedged holder must be reported on one caller instead of hanging every other
//! one silently. The bound is supplied by the caller, because the worst-case
//! legitimate hold differs per site (the gateway holds it across two network
//! round trips; Antigravity's project-id merge holds it across two file
//! operations and nothing else).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

// Only the Unix arm touches the filesystem directly; off Unix the lock is a
// documented no-op that opens nothing.
#[cfg(unix)]
use std::{fs, io};

use anyhow::Context;

/// The human-readable identity of one lock, so a shared implementation can
/// still produce the site-specific diagnostics each caller needs.
///
/// Carries its own `Once` rather than sharing a single global: the non-Unix
/// warning is "once per process **per lock kind**", and a shared latch would
/// let whichever kind warned first silence the other one forever.
pub struct FileLockKind {
    /// Names the lock in the timeout message ("gateway session lock").
    pub lock_name: &'static str,
    /// Sentence appended after the timeout message names the lock file: who is
    /// most likely holding it and what to do about it.
    pub contention_hint: &'static str,
    /// Context for a `spawn_blocking` join failure.
    pub task_context: &'static str,
    /// Printed once per process off Unix, where the lock is a documented no-op.
    pub unsupported_warning: &'static str,
    #[cfg(not(unix))]
    pub warned: std::sync::Once,
}

/// Held for the duration of a read -> modify -> write critical section.
/// Dropping it releases the advisory lock.
///
/// `Debug` is derived rather than redacted: this holds a descriptor on an empty
/// lock file, never any credential material.
#[derive(Debug)]
pub struct FileLock {
    #[cfg(unix)]
    file: fs::File,
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Closing the descriptor would release the lock anyway; unlocking
        // explicitly keeps the release ordered with respect to the writeback
        // that just happened rather than with an implicit close.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Take the exclusive advisory lock for `path`, off the async runtime.
///
/// Returns a guard that is `Send`, so it can be held across `.await` points —
/// which is what a read/modify/write critical section spanning two
/// `spawn_blocking` file operations needs.
pub async fn lock_file(
    path: &Path,
    kind: &'static FileLockKind,
    timeout: Duration,
) -> anyhow::Result<FileLock> {
    let lock_path = lock_path(path);
    tokio::task::spawn_blocking(move || lock_blocking_at(&lock_path, kind, timeout))
        .await
        .context(kind.task_context)?
}

/// [`lock_file`] for a caller that is already inside a `spawn_blocking` (or is
/// plain synchronous code). Blocks the calling thread.
pub fn lock_file_blocking(
    path: &Path,
    kind: &'static FileLockKind,
    timeout: Duration,
) -> anyhow::Result<FileLock> {
    lock_blocking_at(&lock_path(path), kind, timeout)
}

/// The sibling `.lock` file guarding `path`.
pub fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Re-try cadence for the non-blocking acquire. Short relative to any
/// legitimate hold, so the waiter picks the lock up promptly once the holder
/// is done.
#[cfg(unix)]
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(unix)]
fn lock_blocking_at(
    lock_path: &Path,
    kind: &'static FileLockKind,
    timeout: Duration,
) -> anyhow::Result<FileLock> {
    use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};

    if let Some(parent) = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        super::create_private_dir(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    // Non-blocking acquire on a deadline rather than a bare `LOCK_EX`. Waiting
    // is what the waiter wants — the holder is doing one short critical
    // section, and the waiter re-reads afterwards and usually finds the result
    // already on disk — but waiting *without bound* means a wedged holder
    // hangs every other caller with no output to explain it.
    let deadline = std::time::Instant::now() + timeout;
    // Test-only, and only ever `Some` once the lock is genuinely contended:
    // registering on the fast path would report a waiter that never waited.
    // Held to the end of this function, so `Drop` is what deregisters — the
    // acquire, the hard-error return, and the deadline bail all exit through
    // it without any of them naming it.
    #[cfg(test)]
    let mut blocked: Option<BlockedWaiter> = None;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(FileLock { file });
        }
        let error = io::Error::last_os_error();
        // Only contention is retried; a real failure (bad descriptor, a
        // filesystem that cannot lock) is reported immediately rather than
        // being retried until the deadline and then misreported as contention.
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error).with_context(|| format!("failed to lock {}", lock_path.display()));
        }
        // Past the `WouldBlock` check, so this caller is about to wait for a
        // holder rather than to fail.
        #[cfg(test)]
        blocked.get_or_insert_with(|| BlockedWaiter::register(lock_path));
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out after {:?} waiting for the {} at {}. {}",
                timeout,
                kind.lock_name,
                lock_path.display(),
                kind.contention_hint
            );
        }
        // Clamped to what is left of the budget, not to the whole of it:
        // `timeout` never shrinks, so retrying on it would let the last sleep
        // run past `deadline` and report the timeout up to one retry interval
        // late.
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(remaining.min(LOCK_RETRY_INTERVAL));
    }
}

/// Documented no-op off Unix: `flock(2)` has no `std` equivalent there, so the
/// concurrency guard degrades to nothing rather than failing to build.
///
/// It says so once per process rather than degrading silently: the symptom is
/// a lost credential much later, which is impossible to trace back to an
/// absent lock with nothing in the output to connect them.
#[cfg(not(unix))]
fn lock_blocking_at(
    _lock_path: &Path,
    kind: &'static FileLockKind,
    _timeout: Duration,
) -> anyhow::Result<FileLock> {
    kind.warned.call_once(|| {
        eprintln!("{}", kind.unsupported_warning);
    });
    Ok(FileLock {})
}

/// Test-only registry of callers currently parked on a file lock.
///
/// Exists because "blocked on `flock`" is otherwise invisible from outside the
/// process, which forces a test that cares about it to guess with a timeout —
/// and any finite guess loses to a slow enough runner. Counting the waiters
/// turns that guess into an observation.
///
/// Keyed by lock path, never a single global count: sibling tests take locks
/// of their own, in parallel, and a global counter would let one of them
/// satisfy another's wait.
#[cfg(test)]
fn blocked_waiters() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, usize>> {
    static WAITERS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, usize>>,
    > = std::sync::OnceLock::new();
    WAITERS.get_or_init(Default::default)
}

/// RAII registration in [`blocked_waiters`]. A guard rather than paired
/// register/deregister calls because [`lock_blocking_at`] leaves by four
/// different paths, and one of them forgetting to deregister would leave a
/// phantom waiter that makes a later test observe a block that already ended.
#[cfg(test)]
struct BlockedWaiter(PathBuf);

#[cfg(test)]
impl BlockedWaiter {
    fn register(lock_path: &Path) -> Self {
        *blocked_waiters()
            .lock()
            .expect("waiter registry")
            .entry(lock_path.to_path_buf())
            .or_insert(0) += 1;
        Self(lock_path.to_path_buf())
    }
}

#[cfg(test)]
impl Drop for BlockedWaiter {
    fn drop(&mut self) {
        let mut waiters = blocked_waiters().lock().expect("waiter registry");
        // Removed at zero rather than left as a 0 entry, so `waiters_blocked_on`
        // and a map lookup agree on "nobody is waiting".
        if let Some(count) = waiters.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                waiters.remove(&self.0);
            }
        }
    }
}

/// How many callers are parked waiting for `path`'s lock right now.
///
/// Takes the *guarded* path, not the lock path, so callers cannot disagree
/// with [`lock_path`] about which file the count is keyed by.
#[cfg(test)]
pub(crate) fn waiters_blocked_on(path: &Path) -> usize {
    blocked_waiters()
        .lock()
        .expect("waiter registry")
        .get(&lock_path(path))
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: FileLockKind = FileLockKind {
        lock_name: "test file lock",
        contention_hint: "Another test is holding it.",
        task_context: "test file lock task failed",
        unsupported_warning: "Warning: no advisory file lock on this platform.",
        #[cfg(not(unix))]
        warned: std::sync::Once::new(),
    };

    fn temp_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        std::env::temp_dir().join(format!(
            "shunt-file-lock-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn lock_path_is_a_sibling_of_the_guarded_file() {
        assert_eq!(
            lock_path(Path::new("/tmp/shunt/creds.json")),
            PathBuf::from("/tmp/shunt/creds.json.lock")
        );
    }

    /// The async and blocking entry points must contend with each other, not
    /// just each with itself: `write_stored` uses the blocking one from inside
    /// `spawn_blocking` while the project-id merge holds the async one.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_blocking_entry_point_waits_for_an_async_holder() {
        let dir = temp_dir("cross-entry");
        let path = dir.join("creds.json");
        let held = lock_file(&path, &TEST_LOCK, Duration::from_secs(10))
            .await
            .expect("async acquisition");

        let error = tokio::task::spawn_blocking({
            let path = path.clone();
            move || lock_file_blocking(&path, &TEST_LOCK, Duration::from_millis(200))
        })
        .await
        .expect("join")
        .expect_err("a held lock must block the blocking entry point too");
        assert!(error.to_string().contains("test file lock"), "got: {error}");

        drop(held);
        tokio::task::spawn_blocking({
            let path = path.clone();
            move || lock_file_blocking(&path, &TEST_LOCK, Duration::from_secs(10))
        })
        .await
        .expect("join")
        .expect("the lock is free once the async guard drops");

        let _ = fs::remove_dir_all(dir);
    }

    /// The timeout message must name the kind and the lock file, or an
    /// operator has nothing to act on.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timeout_names_the_kind_and_the_lock_file() {
        let dir = temp_dir("timeout-text");
        let path = dir.join("creds.json");
        let held = lock_file(&path, &TEST_LOCK, Duration::from_secs(10))
            .await
            .expect("first acquisition");

        let error = lock_file(&path, &TEST_LOCK, Duration::from_millis(200))
            .await
            .expect_err("a held lock must time out rather than block forever");
        let message = error.to_string();
        assert!(message.contains("test file lock"), "got: {message}");
        assert!(
            message.contains(&lock_path(&path).display().to_string()),
            "got: {message}"
        );
        assert!(
            message.contains("Another test is holding it."),
            "got: {message}"
        );

        drop(held);
        let _ = fs::remove_dir_all(dir);
    }
}
