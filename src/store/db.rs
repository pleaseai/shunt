//! The portable execution layer (ADR-0006).
//!
//! `Db` is a concrete enum, not `AnyPool`: `AnyConnectOptions` is URL-only and
//! loses `create_if_missing(false)`, `read_only(true)`, WAL and busy-timeout
//! tuning, and the pre-created 0600 file. SQL is written once with `$N`
//! placeholders in a portable subset (TEXT/BIGINT, `ON CONFLICT ... DO UPDATE`,
//! no RETURNING, no JSONB); the SQLite arm rewrites `$N` to `?N`.

use std::borrow::Cow;

use sqlx::{AssertSqlSafe, Row as _, SqlitePool};

use super::error::StoreError;

/// A bound parameter. `u64` values go through [`super::error::to_i64`] first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    OptInt(Option<i64>),
    Text(String),
    OptText(Option<String>),
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<Option<i64>> for Value {
    fn from(value: Option<i64>) -> Self {
        Self::OptInt(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Option<String>> for Value {
    fn from(value: Option<String>) -> Self {
        Self::OptText(value)
    }
}

/// The backing database. Cheap to clone (the pools are reference-counted).
#[derive(Debug, Clone)]
pub enum Db {
    Sqlite(SqlitePool),
    #[cfg(feature = "store-postgres")]
    Postgres(sqlx::PgPool),
}

/// One result row; columns are decoded by index on access.
pub enum Row {
    Sqlite(sqlx::sqlite::SqliteRow),
    #[cfg(feature = "store-postgres")]
    Postgres(sqlx::postgres::PgRow),
}

macro_rules! column {
    ($row:expr, $ty:ty, $index:expr) => {
        match $row {
            Row::Sqlite(row) => row.try_get::<$ty, _>($index),
            #[cfg(feature = "store-postgres")]
            Row::Postgres(row) => row.try_get::<$ty, _>($index),
        }
        .map_err(|error| decode_error($index, &error))
    };
}

impl Row {
    pub fn int(&self, index: usize) -> Result<i64, StoreError> {
        column!(self, i64, index)
    }

    pub fn opt_int(&self, index: usize) -> Result<Option<i64>, StoreError> {
        column!(self, Option<i64>, index)
    }

    pub fn text(&self, index: usize) -> Result<String, StoreError> {
        column!(self, String, index)
    }

    pub fn opt_text(&self, index: usize) -> Result<Option<String>, StoreError> {
        column!(self, Option<String>, index)
    }
}

fn decode_error(index: usize, error: &sqlx::Error) -> StoreError {
    StoreError::Database {
        reason: format!("column {index} did not decode: {error}"),
    }
}

/// An open transaction. Dropping it without [`Tx::commit`] rolls it back.
pub enum Tx {
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    #[cfg(feature = "store-postgres")]
    Postgres(PgTx),
}

/// A PostgreSQL transaction plus whether one of its round trips timed out. The
/// cancelled request leaves the connection mid-protocol, so a later ROLLBACK or
/// COMMIT would only wait out another bound on it.
#[cfg(feature = "store-postgres")]
pub struct PgTx {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    timed_out: bool,
}

macro_rules! bind_values {
    ($query:expr, $params:expr) => {{
        let mut query = $query;
        for param in $params {
            query = match param {
                Value::Int(v) => query.bind(*v),
                Value::OptInt(v) => query.bind(*v),
                Value::Text(v) => query.bind(v.as_str()),
                Value::OptText(v) => query.bind(v.as_deref()),
            };
        }
        query
    }};
}

fn database_error(error: &sqlx::Error) -> StoreError {
    StoreError::Database {
        reason: error.to_string(),
    }
}

/// The longest one PostgreSQL round trip (BEGIN, a statement, COMMIT or
/// ROLLBACK) may take. Durable writes run under locks that every token
/// rotation and spend mutation queue behind, and on a half-open connection the
/// await would otherwise stay pending until the OS gives up on the socket. It
/// is longer than the settled read's 5 s `lock_timeout`, so that wait still
/// ends with the server's own error. After a timeout the transaction skips its
/// remaining COMMIT or ROLLBACK, and the pool closes the connection.
#[cfg(feature = "store-postgres")]
const POSTGRES_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The longest the pool's release-time drain may take before the connection is
/// closed instead of returned. It drains what a cancelled request left on the
/// connection; sqlx's own release ping has no bound of its own.
#[cfg(feature = "store-postgres")]
pub(crate) fn release_timeout() -> std::time::Duration {
    #[cfg(test)]
    if let Some(timeout) = tests::release_timeout_override() {
        return timeout;
    }
    POSTGRES_IO_TIMEOUT
}

#[cfg(feature = "store-postgres")]
fn io_timeout() -> std::time::Duration {
    #[cfg(test)]
    if let Some(timeout) = tests::IO_TIMEOUT.with(std::cell::Cell::get) {
        return timeout;
    }
    POSTGRES_IO_TIMEOUT
}

/// Replace [`POSTGRES_IO_TIMEOUT`] on the current test thread (`None` restores
/// it). Each test runs on its own thread, so this never reaches another test.
#[cfg(all(test, feature = "store-postgres"))]
pub(crate) fn set_io_timeout(timeout: Option<std::time::Duration>) {
    tests::IO_TIMEOUT.with(|cell| cell.set(timeout));
}

/// Await one PostgreSQL round trip, giving up after [`POSTGRES_IO_TIMEOUT`]
/// with a [`StoreError::Database`]. The gateway token and spend writers report
/// a COMMIT that times out this way as [`StoreError::CommitUncertain`] and read
/// the outcome back.
#[cfg(feature = "store-postgres")]
async fn bounded<T>(
    what: &str,
    io: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, StoreError> {
    bounded_on(&mut false, what, io).await
}

/// [`bounded`] that records a timeout in `timed_out`.
#[cfg(feature = "store-postgres")]
async fn bounded_on<T>(
    timed_out: &mut bool,
    what: &str,
    io: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, StoreError> {
    let timeout = io_timeout();
    match tokio::time::timeout(timeout, io).await {
        Ok(result) => result.map_err(|e| database_error(&e)),
        Err(_) => {
            *timed_out = true;
            Err(StoreError::Database {
                reason: format!("postgres {what} did not complete within {timeout:?}"),
            })
        }
    }
}

#[cfg(feature = "store-postgres")]
fn abandoned(what: &str) -> StoreError {
    StoreError::Database {
        reason: format!(
            "postgres {what} skipped: an earlier round trip on this transaction timed out"
        ),
    }
}

impl Db {
    /// Start a transaction. SQLite has one connection, so a second concurrent
    /// `begin` waits for the first to finish.
    pub async fn begin(&self) -> Result<Tx, StoreError> {
        match self {
            Self::Sqlite(pool) => Ok(Tx::Sqlite(
                pool.begin().await.map_err(|e| database_error(&e))?,
            )),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(pool) => Ok(Tx::Postgres(PgTx {
                tx: bounded("BEGIN", pool.begin()).await?,
                timed_out: false,
            })),
        }
    }

    /// Close the pool, waiting for the connection to be released. For SQLite
    /// this checkpoints the WAL when the last connection closes.
    pub async fn close(&self) {
        match self {
            Self::Sqlite(pool) => pool.close().await,
            #[cfg(feature = "store-postgres")]
            Self::Postgres(pool) => pool.close().await,
        }
    }
}

impl Tx {
    /// Run a statement; returns the rows affected. `sql` is code-owned text
    /// with `$N` placeholders, never operator input.
    pub async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64, StoreError> {
        match self {
            Self::Sqlite(tx) => {
                let sql = rewrite_placeholders(sql).into_owned();
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql)), params);
                let done = query
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| database_error(&e))?;
                Ok(done.rows_affected())
            }
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => {
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let done =
                    bounded_on(&mut tx.timed_out, "statement", query.execute(&mut *tx.tx)).await?;
                Ok(done.rows_affected())
            }
        }
    }

    /// Run a query and return every row.
    pub async fn fetch_all(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, StoreError> {
        match self {
            Self::Sqlite(tx) => {
                let sql = rewrite_placeholders(sql).into_owned();
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql)), params);
                let rows = query
                    .fetch_all(&mut **tx)
                    .await
                    .map_err(|e| database_error(&e))?;
                Ok(rows.into_iter().map(Row::Sqlite).collect())
            }
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => {
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let rows = bounded_on(&mut tx.timed_out, "statement", query.fetch_all(&mut *tx.tx))
                    .await?;
                Ok(rows.into_iter().map(Row::Postgres).collect())
            }
        }
    }

    /// Run a query and return the first row, if any, without collecting the
    /// rest of the result set.
    pub async fn fetch_optional(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<Row>, StoreError> {
        match self {
            Self::Sqlite(tx) => {
                let sql = rewrite_placeholders(sql).into_owned();
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql)), params);
                let row = query
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(|e| database_error(&e))?;
                Ok(row.map(Row::Sqlite))
            }
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => {
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let row = bounded_on(
                    &mut tx.timed_out,
                    "statement",
                    query.fetch_optional(&mut *tx.tx),
                )
                .await?;
                Ok(row.map(Row::Postgres))
            }
        }
    }

    pub async fn commit(self) -> Result<(), StoreError> {
        match self {
            Self::Sqlite(tx) => tx.commit().await.map_err(|e| database_error(&e)),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) if tx.timed_out => Err(abandoned("COMMIT")),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => bounded("COMMIT", tx.tx.commit()).await,
        }
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        match self {
            Self::Sqlite(tx) => tx.rollback().await.map_err(|e| database_error(&e)),
            #[cfg(feature = "store-postgres")]
            // Dropping queues the rollback; the pool's release hook bounds it.
            Self::Postgres(tx) if tx.timed_out => Ok(()),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => bounded("ROLLBACK", tx.tx.rollback()).await,
        }
    }
}

/// Rewrite `$N` placeholders to SQLite's `?N`, leaving `$` inside
/// single-quoted string literals (`''` is an escaped quote) untouched.
pub fn rewrite_placeholders(sql: &str) -> Cow<'_, str> {
    if !sql.contains('$') {
        return Cow::Borrowed(sql);
    }
    let mut out = String::with_capacity(sql.len());
    let mut in_literal = false;
    let mut in_identifier = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_identifier => {
                in_literal = !in_literal;
                out.push(c);
            }
            '"' if !in_literal => {
                in_identifier = !in_identifier;
                out.push(c);
            }
            '$' if !in_literal
                && !in_identifier
                && chars.peek().is_some_and(char::is_ascii_digit) =>
            {
                out.push('?');
            }
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests;
