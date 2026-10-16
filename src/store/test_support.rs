//! Shared test helpers for the store modules.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::config::{StoreBackend, StoreConfig, DEFAULT_HOLD_STALE_SECONDS};

/// A self-deleting directory under the system temp dir.
pub(crate) struct TempDir(pub PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-store-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A SQLite `[store]` pointing at `path`.
pub(crate) fn sqlite_config(path: &std::path::Path) -> StoreConfig {
    StoreConfig {
        backend: StoreBackend::Sqlite,
        url: path.to_string_lossy().as_ref().into(),
        max_connections: None,
        password_env: None,
        allow_insecure_remote_tls: false,
        hold_stale_seconds: DEFAULT_HOLD_STALE_SECONDS,
    }
}
