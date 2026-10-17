//! Store errors. Every `Display` names the reason, never credentials. A
//! variant about the store as a whole names the sanitized target
//! (`sqlite:<path>` / `postgres://host:port/db`); the statement-level ones
//! ([`StoreError::Database`], [`StoreError::CommitUncertain`],
//! [`StoreError::OutOfRange`]) are raised below the layer that knows it, so
//! boot and `shunt check` wrap them with [`StoreError::located`] before an
//! operator sees them.

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
    /// Another live shunt process holds the store (AC-026).
    #[error(
        "store {target} is held by another shunt instance ({label}), last renewed {age_secs}s ago; \
         only one shunt process can use a store at a time"
    )]
    HoldHeld {
        target: String,
        label: String,
        age_secs: u64,
    },
    /// This process no longer holds the store: another instance took it over.
    #[error("store {target}: the instance hold was taken over by another shunt process")]
    HoldLost { target: String },
    /// A statement failed.
    #[error("store statement failed: {reason}")]
    Database { reason: String },
    /// `COMMIT` failed, so the write may or may not have landed (the
    /// connection can drop after the server committed but before the ack).
    #[error("store commit outcome unknown: {reason}")]
    CommitUncertain { reason: String },
    /// Importing a JSON snapshot failed; nothing was imported for that
    /// subsystem and no import record was written (AC-029/AC-030).
    #[error(
        "importing the {subsystem} state snapshot {path} into the store failed: {reason}; \
         nothing was imported for it"
    )]
    Import {
        subsystem: &'static str,
        path: String,
        reason: String,
    },
    /// A `u64` did not fit the store's signed 64-bit columns.
    #[error("value {value} does not fit a signed 64-bit store column")]
    OutOfRange { value: u64 },
    /// A statement-level error named with the store it came from; see
    /// [`StoreError::located`].
    #[error("store {target}: {error}")]
    Located {
        target: String,
        error: Box<StoreError>,
    },
}

impl StoreError {
    /// Name `target` on a statement-level error, which has no target of its
    /// own. Every other variant already names it (or, for `Import`, the
    /// snapshot) and is returned unchanged.
    pub fn located(self, target: &str) -> Self {
        match self {
            error @ (Self::Database { .. }
            | Self::CommitUncertain { .. }
            | Self::OutOfRange { .. }) => Self::Located {
                target: target.to_string(),
                error: Box::new(error),
            },
            other => other,
        }
    }
}

/// `u64` to the store's `BIGINT`. Overflow is an error, never a wrap.
pub fn to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::OutOfRange { value })
}

#[cfg(test)]
mod tests {
    use super::StoreError;

    #[test]
    fn located_names_the_target_on_statement_errors_only() {
        let located = StoreError::Database {
            reason: "disk I/O error".to_string(),
        }
        .located("sqlite:/data/store.db");
        assert_eq!(
            located.to_string(),
            "store sqlite:/data/store.db: store statement failed: disk I/O error"
        );

        let held = StoreError::HoldLost {
            target: "sqlite:/data/store.db".to_string(),
        };
        let shown = held.to_string();
        let held = held.located("sqlite:/data/store.db");
        assert!(matches!(held, StoreError::HoldLost { .. }), "{held:?}");
        assert_eq!(
            held.to_string(),
            shown,
            "a targeted variant is not wrapped twice"
        );
    }
}
