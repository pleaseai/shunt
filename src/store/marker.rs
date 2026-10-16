//! The `<state_path>.imported` sidecar (AC-031).
//!
//! After a snapshot is imported into a store, a small JSON file next to it
//! records which snapshot bytes were imported (`source_sha256`), when, and into
//! which backend. The snapshot itself is never touched (AC-006). A store-less
//! boot that loads a snapshot whose sha256 still equals the recorded one is
//! about to run from import-time state, so it warns; once the file changes
//! (a store-less run rewrites it) the sidecar no longer matches and the warning
//! stops, so an operator who accepted the rollback is not nagged forever.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What the sidecar holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImportMarker {
    pub backend: String,
    pub imported_at: u64,
    pub source_sha256: String,
}

/// `<snapshot>.imported`, beside the snapshot.
pub(crate) fn sidecar_path(snapshot: &Path) -> PathBuf {
    let mut name: OsString = snapshot.as_os_str().to_owned();
    name.push(".imported");
    PathBuf::from(name)
}

/// Lower-case hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        })
}

/// Write the sidecar for `snapshot`, atomically and owner-only.
pub(crate) fn write(snapshot: &Path, marker: &ImportMarker) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(marker).map_err(io::Error::other)?;
    crate::atomic_file::write_private_atomic(&sidecar_path(snapshot), &json)
}

/// The marker in `sidecar` when it describes exactly these `snapshot` bytes,
/// `None` when the sidecar is unreadable or the snapshot has changed since.
pub(crate) fn current_marker(sidecar: &[u8], snapshot: &[u8]) -> Option<ImportMarker> {
    let marker: ImportMarker = serde_json::from_slice(sidecar).ok()?;
    (marker.source_sha256 == sha256_hex(snapshot)).then_some(marker)
}

/// Warn (AC-031) when a store-less boot is about to load `snapshot` bytes that
/// were imported into a store. Silent without a sidecar or once the snapshot's
/// sha256 differs from the recorded one.
pub(crate) fn warn_if_imported(subsystem: &str, path: &Path, snapshot: &[u8]) {
    let Ok(sidecar) = std::fs::read(sidecar_path(path)) else {
        return;
    };
    let Some(marker) = current_marker(&sidecar, snapshot) else {
        return;
    };
    tracing::warn!(
        subsystem,
        path = %path.display(),
        backend = %marker.backend,
        imported_at = marker.imported_at,
        "this snapshot was imported into a store and is unchanged since; running without \
         [store] rolls back to that import-time state, which may include sessions revoked and \
         spend limits changed in the store since then"
    );
}

#[cfg(test)]
mod tests;
