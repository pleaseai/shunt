//! Store errors. Every `Display` names the reason and the sanitized target
//! (`sqlite:<path>` / `postgres://host:port/db`), never credentials.

/// Why the store could not be opened or used.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database could not be reached, created, or opened.
    #[error("store {target} is unreachable: {reason}")]
    Unreachable { target: String, reason: String },
    /// A migration failed, or the applied history does not match this binary.
    #[error("store {target}: migration failed: {reason}")]
    Migration { target: String, reason: String },
    /// The database was migrated by a newer binary.
    #[error(
        "store {target} is at schema version {found}, newer than the {supported} this binary supports; upgrade shunt"
    )]
    SchemaTooNew {
        target: String,
        found: i64,
        supported: i64,
    },
    /// Inspect mode only: the SQLite file does not exist (it is created at boot).
    #[error("store {target} does not exist yet; it is created at boot")]
    Absent { target: String },
    /// A statement failed.
    #[error("store statement failed: {reason}")]
    Database { reason: String },
    /// A `u64` did not fit the store's signed 64-bit columns.
    #[error("value {value} does not fit a signed 64-bit store column")]
    OutOfRange { value: u64 },
}

/// `u64` to the store's `BIGINT`. Overflow is an error, never a wrap.
pub fn to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::OutOfRange { value })
}
