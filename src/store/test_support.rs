//! Shared test helpers for the store modules.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::config::{StoreBackend, StoreConfig, DEFAULT_HOLD_STALE_SECONDS};

/// Disambiguates directories made in the same clock tick (macOS clocks tick
/// in microseconds, so parallel tests can otherwise share a name).
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A self-deleting directory under the system temp dir.
pub(crate) struct TempDir(pub PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-store-{tag}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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

/// Run `run` under a throwaway tracing subscriber and return everything it
/// logged (mirrors the capture helper in `reload.rs`).
pub(crate) fn capture_logs(run: impl FnOnce()) -> String {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for BufferWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let output = Arc::new(Mutex::new(Vec::new()));
    let writer_output = Arc::clone(&output);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || BufferWriter(Arc::clone(&writer_output)))
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    let bytes = output.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}
