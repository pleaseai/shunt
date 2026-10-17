//! Opening the store: connect, preflight, instance hold, migrate.

use std::sync::Arc;
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use super::config::{StoreBackend, StoreConfig};
use super::db::Db;
use super::error::StoreError;
use super::hold::HoldState;
use super::migrate;

/// How the store is being opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `shunt run`: create a missing SQLite file, migrate, and later hold the
    /// instance.
    Serve,
    /// `shunt check`: read-only. Never creates the database, migrates, or
    /// modifies it; SQLite may still create or remove the `-wal`/`-shm`
    /// sidecars while reading a WAL database.
    Inspect,
}

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(feature = "store-postgres")]
const POSTGRES_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(feature = "store-postgres")]
const DEFAULT_POSTGRES_CONNECTIONS: u32 = 4;

/// An open store: the database plus the sanitized target for messages.
#[derive(Debug, Clone)]
pub struct Store {
    db: Db,
    target: String,
    migrator: &'static Migrator,
    /// Shared across clones: they are one instance, one hold.
    pub(super) hold: Arc<HoldState>,
    /// The config this store opened with, so tests can open a second opener.
    #[cfg(test)]
    pub(crate) config: StoreConfig,
}

impl Store {
    /// Connect, check the schema is not newer than this binary, then (in
    /// `Serve` mode) apply pending migrations before returning.
    pub async fn open(config: &StoreConfig, mode: Mode) -> Result<Store, StoreError> {
        let target = config.sanitized_target();
        let db = connect(config, mode, &target).await?;
        let migrator = migrate::embedded(&db);
        Self::finish(config, db, target, mode, migrator).await
    }

    /// `open` with an explicit migration set, so tests can inject a bad one.
    #[cfg(test)]
    pub(crate) async fn open_with(
        config: &StoreConfig,
        mode: Mode,
        migrator: &'static Migrator,
    ) -> Result<Store, StoreError> {
        let target = config.sanitized_target();
        let db = connect(config, mode, &target).await?;
        Self::finish(config, db, target, mode, migrator).await
    }

    async fn finish(
        config: &StoreConfig,
        db: Db,
        target: String,
        mode: Mode,
        migrator: &'static Migrator,
    ) -> Result<Store, StoreError> {
        let store = Store {
            db,
            target,
            migrator,
            hold: Arc::new(HoldState::new(config.hold_stale_seconds)),
            #[cfg(test)]
            config: config.clone(),
        };
        if mode == Mode::Inspect {
            return Ok(store);
        }
        let opened = store.serve_steps().await;
        if opened.is_err() {
            // The hold may have been acquired before a later step failed; the
            // holder id is random per open, so a corrected restart would
            // otherwise be refused until the hold goes stale.
            if let Err(error) = store.release_hold().await {
                tracing::warn!(%error, "failed to release the store hold after a failed open");
            }
            store.db.close().await;
        }
        opened.map(|()| store)
    }

    /// `preflight` -> `ensure_hold_table` -> acquire hold -> `run`, in that order.
    async fn serve_steps(&self) -> Result<(), StoreError> {
        migrate::preflight(&self.db, self.migrator, &self.target).await?;
        self.before_migrate().await?;
        migrate::run(&self.db, self.migrator, &self.target).await
    }

    /// Runs after the schema-version preflight and before migrations. The
    /// instance hold is acquired here, so a refused opener never migrates a
    /// live holder's schema.
    async fn before_migrate(&self) -> Result<(), StoreError> {
        self.ensure_hold_table().await?;
        self.acquire_hold().await.map(|_| ())
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Backend plus location, never credentials.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The applied schema version (0 for a database with no migrations).
    pub async fn schema_version(&self) -> Result<i64, StoreError> {
        migrate::schema_version(&self.db).await
    }

    /// The newest schema version this binary embeds.
    pub fn supported_version(&self) -> i64 {
        migrate::supported_version(self.migrator)
    }

    /// How many embedded migrations the database has not applied yet.
    pub async fn pending_migrations(&self) -> Result<usize, StoreError> {
        let applied = self.schema_version().await?;
        Ok(self.migrator.iter().filter(|m| m.version > applied).count())
    }

    /// Fail with `SchemaTooNew` when the database is ahead of this binary.
    /// For `Inspect` callers, which do not run the preflight on open.
    pub async fn check_schema(&self) -> Result<i64, StoreError> {
        migrate::preflight(&self.db, self.migrator, &self.target).await
    }

    /// Close the connection(s); a SQLite WAL is checkpointed.
    pub async fn close(&self) {
        self.db.close().await;
    }
}

async fn connect(config: &StoreConfig, mode: Mode, target: &str) -> Result<Db, StoreError> {
    match config.backend {
        StoreBackend::Sqlite => connect_sqlite(config, mode, target).await,
        StoreBackend::Postgres => connect_postgres(config, mode, target).await,
    }
}

fn unreachable(target: &str, reason: impl std::fmt::Display) -> StoreError {
    StoreError::Unreachable {
        target: target.to_string(),
        reason: reason.to_string(),
    }
}

async fn connect_sqlite(config: &StoreConfig, mode: Mode, target: &str) -> Result<Db, StoreError> {
    let path = config.sqlite_file();
    let mut options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(false)
        .busy_timeout(SQLITE_BUSY_TIMEOUT);
    match mode {
        Mode::Serve => {
            precreate_private(&path).map_err(|error| unreachable(target, error))?;
            options = options.journal_mode(SqliteJournalMode::Wal);
        }
        Mode::Inspect => {
            if !path.exists() {
                return Err(StoreError::Absent {
                    target: target.to_string(),
                });
            }
            options = options.read_only(true);
        }
    }
    // One connection, never recycled: it removes read-then-write upgrade
    // SQLITE_BUSY races, and keeps the WAL open for the process lifetime.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .map_err(|error| unreachable(target, error))?;
    Ok(Db::Sqlite(pool))
}

/// Create a missing database file with mode 0600 before SQLite sees it.
/// SQLite gives `-wal` and `-shm` the main file's mode, so the sidecars
/// inherit it. An existing file is left as the operator made it.
fn precreate_private(path: &std::path::Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(feature = "store-postgres")]
async fn connect_postgres(
    config: &StoreConfig,
    mode: Mode,
    target: &str,
) -> Result<Db, StoreError> {
    let mut options = config
        .pg_connect_options()
        .map_err(|reason| unreachable(target, reason))?;
    if mode == Mode::Inspect {
        options = options.options([("default_transaction_read_only", "on")]);
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(
            config
                .max_connections
                .unwrap_or(DEFAULT_POSTGRES_CONNECTIONS),
        )
        .acquire_timeout(POSTGRES_ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
        .map_err(|error| unreachable(target, error))?;
    Ok(Db::Postgres(pool))
}

#[cfg(not(feature = "store-postgres"))]
async fn connect_postgres(
    _config: &StoreConfig,
    _mode: Mode,
    target: &str,
) -> Result<Db, StoreError> {
    Err(unreachable(
        target,
        "this build has no postgres support; rebuild with `--features store-postgres`",
    ))
}

#[cfg(test)]
mod tests;
