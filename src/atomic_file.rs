//! Atomic, owner-only (0600) file writes shared by the on-disk state caches
//! ([`crate::state_persist`], [`crate::gateway::persist`]) and credential
//! writeback ([`crate::auth::shared`]).
//!
//! Writes go to a unique sibling temp file first, then rename over the target,
//! then sync the containing directory. Rename is atomic on the same
//! filesystem, so a crash mid-write never leaves a truncated file where a
//! reader would find it, and the directory sync makes the rename itself
//! survive power loss. Exclusive creation of the temp file prevents a local
//! process from redirecting the write through a symlink.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Write `bytes` to `path` atomically with owner-only permissions, creating
/// parent directories as needed. On failure the temp file is removed and the
/// previous target content (if any) is left untouched.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    write_private_atomic_in_existing_dir(path, bytes)
}

/// [`write_private_atomic`] minus parent-directory creation. Credential
/// writeback uses this entry point so a missing parent stays an error instead
/// of being silently created with umask-default permissions.
pub(crate) fn write_private_atomic_in_existing_dir(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    if let Err(error) = write_private(&tmp, bytes).and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    sync_parent_dir(path)
}

/// Flush the directory entry after a rename: without it a power loss can lose
/// the rename even though the write already returned success, silently
/// dropping a just-granted session or credential.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::File::open(parent)?.sync_all()
}

/// Windows cannot fsync a directory through `std`; rename durability is left
/// to the filesystem there.
#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Unique sibling staging path for an atomic write. Exclusive creation below
/// prevents a local process from redirecting the write through a symlink.
fn tmp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shunt-state"),
        std::process::id(),
        counter
    ))
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    // The temp file must be born private: `mode(0o600)` only applies when the
    // file is created, so a stale or pre-created temp at this predictable path
    // would keep its old mode. Remove any leftover, then require exclusive
    // creation: if something recreates the path in between, fail instead of
    // writing secrets into a file someone else owns.
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
