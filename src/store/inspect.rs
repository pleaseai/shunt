//! `shunt check` store inspection (FR-008, AC-021): connect read-only, report
//! the schema version, never migrate, write, or create a missing file.

use std::time::Duration;

use super::config::StoreConfig;
use super::error::StoreError;
use super::open::{Mode, Store};

/// Longer than the 10 s PostgreSQL acquire timeout, so an unreachable host
/// reports its own error instead of this bound.
pub const INSPECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The one line `shunt check` prints for `config`'s store, or why the store
/// cannot be used. Never contains credentials: every name in it comes from
/// [`StoreConfig::sanitized_target`].
pub async fn inspect(config: &StoreConfig) -> Result<String, StoreError> {
    let head = format!(
        "store ok: {} {}",
        config.backend_label(),
        config.sanitized_target()
    );
    let store = match Store::open(config, Mode::Inspect).await {
        Ok(store) => store,
        Err(StoreError::Absent { .. }) => {
            return Ok(format!(
                "{head}, file does not exist yet; it is created with mode 0600 at boot"
            ));
        }
        Err(error) => return Err(error),
    };
    let report = report(&store, &head).await;
    store.close().await;
    report
}

async fn report(store: &Store, head: &str) -> Result<String, StoreError> {
    let found = store.check_schema().await?;
    let supported = store.supported_version();
    let mut line = format!("{head}, schema v{found} (binary supports v{supported})");
    match store.pending_migrations().await? {
        0 => {}
        1 => line.push_str("; 1 pending migration applies at boot"),
        pending => line.push_str(&format!("; {pending} pending migrations apply at boot")),
    }
    Ok(line)
}
