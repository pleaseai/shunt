//! Atomic, owner-only (0600) file writes shared by the on-disk state caches
//! ([`crate::state_persist`], [`crate::gateway::persist`]).
//!
//! Writes go to a unique sibling temp file first, then rename over the target.
//! Rename is atomic on the same filesystem, so a crash mid-write never leaves a
//! truncated file where a reader would find it. Exclusive creation of the temp
//! file prevents a local process from redirecting the write through a symlink.

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
    let tmp = tmp_path(path);
    if let Err(error) = write_private(&tmp, bytes).and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
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
