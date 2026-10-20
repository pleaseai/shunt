//! In-process single-flight locks for OAuth refreshes, one per credential file.
//!
//! Extracted from the ChatGPT/Codex store, which needed it first, so the xAI
//! store can share it. A provider whose refresh token rotates on use must never
//! run two refreshes of the same file at once: the loser replays a consumed
//! token. But a pooled provider fails over between accounts, so one account's
//! refresh hanging at the token endpoint must not hold up another account's.
//! Keying the lock by credential path gives both.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex as StdMutex, Weak},
};

/// Registry of per-path refresh locks. Stores are constructed per request, so
/// the registry must be shared across independent instances. Keying by path
/// lets refreshes for different accounts (different credential files) proceed
/// in parallel while still serializing concurrent refreshes of the *same*
/// file. Entries are held `Weak`, so a path's lock is dropped once nothing is
/// refreshing it, keeping the registry from growing unbounded over the process
/// lifetime. The refresh task owns the guard through atomic writeback,
/// preventing a cancelled caller from exposing an already-consumed refresh
/// token to another request.
static REFRESH_LOCKS: LazyLock<StdMutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Canonicalize a credential path so two spellings of one file (a symlink, a
/// relative override) share one lock. Falls back to the path as given when it
/// cannot be resolved.
pub(crate) fn normalized_auth_path(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

/// Return the single-flight lock for `path`, creating and registering one if
/// none exists (or the previous one has been dropped). Opportunistically
/// prunes dead entries so paths that are no longer being refreshed don't
/// linger in the registry forever.
pub(crate) fn refresh_lock_for(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = REFRESH_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    locks.retain(|_, weak| weak.strong_count() > 0);
    lock
}
