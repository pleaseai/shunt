//! Embedded migrations and the schema-version reads around them.
//!
//! `preflight` and `schema_version` only read: `Migrator::run` writes, so it
//! cannot be the check that a newer binary already migrated this database
//! (AC-020).

use sqlx::migrate::Migrator;

use super::db::{Db, Value};
use super::error::StoreError;

pub(crate) static SQLITE: Migrator = sqlx::migrate!("./migrations/sqlite");

#[cfg(feature = "store-postgres")]
pub(crate) static POSTGRES: Migrator = sqlx::migrate!("./migrations/postgres");

/// The migration set this binary embeds for `db`'s engine.
pub(crate) fn embedded(db: &Db) -> &'static Migrator {
    match db {
        Db::Sqlite(_) => &SQLITE,
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => &POSTGRES,
    }
}

/// The newest version in `migrator` (0 when empty).
pub(crate) fn supported_version(migrator: &Migrator) -> i64 {
    migrator.iter().map(|m| m.version).max().unwrap_or(0)
}

const TABLE_EXISTS_SQLITE: &str =
    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'";
#[cfg(feature = "store-postgres")]
const TABLE_EXISTS_POSTGRES: &str = "SELECT 1 FROM information_schema.tables \
     WHERE table_schema = current_schema() AND table_name = '_sqlx_migrations'";

/// `MAX(version)` of the applied migrations; a missing table is 0. Read-only.
pub(crate) async fn schema_version(db: &Db) -> Result<i64, StoreError> {
    let mut tx = db.begin().await?;
    let exists_sql = match db {
        Db::Sqlite(_) => TABLE_EXISTS_SQLITE,
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => TABLE_EXISTS_POSTGRES,
    };
    let version = if tx.fetch_optional(exists_sql, &[]).await?.is_none() {
        0
    } else {
        let params: [Value; 0] = [];
        match tx
            .fetch_optional("SELECT MAX(version) FROM _sqlx_migrations", &params)
            .await?
        {
            Some(row) => row.opt_int(0)?.unwrap_or(0),
            None => 0,
        }
    };
    tx.rollback().await?;
    Ok(version)
}

/// Fail with [`StoreError::SchemaTooNew`] when the database is ahead of
/// `migrator`. Returns the database's version otherwise.
pub(crate) async fn preflight(
    db: &Db,
    migrator: &Migrator,
    target: &str,
) -> Result<i64, StoreError> {
    let found = schema_version(db).await.map_err(|error| match error {
        StoreError::Database { reason } => StoreError::Unreachable {
            target: target.to_string(),
            reason,
        },
        other => other,
    })?;
    let supported = supported_version(migrator);
    if found > supported {
        return Err(StoreError::SchemaTooNew {
            target: target.to_string(),
            found,
            supported,
        });
    }
    Ok(found)
}

/// Apply every pending migration in `migrator`.
pub(crate) async fn run(db: &Db, migrator: &Migrator, target: &str) -> Result<(), StoreError> {
    let result = match db {
        Db::Sqlite(pool) => migrator.run(pool).await,
        #[cfg(feature = "store-postgres")]
        Db::Postgres(pool) => migrator.run(pool).await,
    };
    result.map_err(|error| StoreError::Migration {
        target: target.to_string(),
        reason: error.to_string(),
    })
}
