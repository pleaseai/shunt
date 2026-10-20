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

/// A PostgreSQL transaction on its own pooled connection. BEGIN, COMMIT and
/// ROLLBACK are plain statements rather than an `sqlx::Transaction`, so a
/// transaction that does not finish can discard its connection: a cancelled
/// BEGIN or statement may leave the server inside a transaction sqlx does not
/// track. Only a successful COMMIT or ROLLBACK returns the connection to the
/// pool; anything else closes it on drop.
#[cfg(feature = "store-postgres")]
pub struct PgTx {
    /// `None` only once [`Drop`] has taken it.
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
    /// A round trip timed out: the cancelled request left the connection
    /// mid-protocol, so a later ROLLBACK or COMMIT would only wait out another
    /// bound on it.
    timed_out: bool,
    finished: bool,
}

/// The connection in [`PgTx::conn`], borrowing that field alone so a round trip
/// can also record a timeout in [`PgTx::timed_out`].
#[cfg(feature = "store-postgres")]
fn pg_conn(
    conn: &mut Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
) -> &mut sqlx::PgConnection {
    conn.as_deref_mut()
        .expect("the connection is taken only on drop")
}

#[cfg(feature = "store-postgres")]
impl Drop for PgTx {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if !self.finished {
                // Detaching frees the pool slot at once; dropping the raw
                // connection closes its socket, and the server rolls back.
                drop(conn.detach());
            }
        }
    }
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
/// remaining round trips and closes its connection. The migrator's own run is
/// not a [`Tx`] and is not bounded; its schema-version preflight is.
#[cfg(feature = "store-postgres")]
const POSTGRES_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[cfg_attr(not(test), allow(unused_variables))]
#[cfg(feature = "store-postgres")]
fn io_timeout(what: &str) -> std::time::Duration {
    #[cfg(test)]
    if let Some(timeout) = tests::io_timeout_override(what) {
        return timeout;
    }
    POSTGRES_IO_TIMEOUT
}

/// Replace [`POSTGRES_IO_TIMEOUT`] for every round trip on the current test
/// thread until the returned guard drops. Each test runs on its own thread, so
/// this never reaches another test.
#[cfg(all(test, feature = "store-postgres"))]
pub(crate) fn set_io_timeout(timeout: std::time::Duration) -> tests::IoTimeoutGuard {
    tests::IoTimeoutGuard::install(timeout, None)
}

/// [`set_io_timeout`] for one kind of round trip (`"BEGIN"`, `"COMMIT"`, ...)
/// alone: every other round trip keeps [`POSTGRES_IO_TIMEOUT`].
#[cfg(all(test, feature = "store-postgres"))]
pub(crate) fn set_round_trip_timeout(
    what: &'static str,
    timeout: std::time::Duration,
) -> tests::IoTimeoutGuard {
    tests::IoTimeoutGuard::install(timeout, Some(what))
}

/// Await one PostgreSQL round trip, giving up after [`POSTGRES_IO_TIMEOUT`]
/// with a [`StoreError::Database`]. [`Tx::commit`] reports a COMMIT that times
/// out this way as [`StoreError::CommitUncertain`], and the gateway token and
/// spend writers read the outcome back.
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
    let timeout = io_timeout(what);
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
            Self::Postgres(pool) => {
                // The pool's acquire timeout bounds the wait for a connection;
                // the round-trip bound covers BEGIN alone.
                let conn = pool.acquire().await.map_err(|e| database_error(&e))?;
                let mut tx = PgTx {
                    conn: Some(conn),
                    timed_out: false,
                    finished: false,
                };
                bounded(
                    "BEGIN",
                    sqlx::raw_sql("BEGIN").execute(pg_conn(&mut tx.conn)),
                )
                .await?;
                Ok(Tx::Postgres(tx))
            }
        }
    }

    /// Start a transaction that reads before it writes. SQLite refuses a
    /// deferred transaction's read-to-write upgrade at once when another
    /// process holds or has just committed a write, without waiting out the
    /// busy timeout; `BEGIN IMMEDIATE` takes the write lock up front, so it
    /// waits instead. PostgreSQL needs no such lock and starts a plain one.
    pub async fn begin_immediate(&self) -> Result<Tx, StoreError> {
        match self {
            Self::Sqlite(pool) => Ok(Tx::Sqlite(
                pool.begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(|e| database_error(&e))?,
            )),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(_) => self.begin().await,
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
                if tx.timed_out {
                    return Err(abandoned("statement"));
                }
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let done = bounded_on(
                    &mut tx.timed_out,
                    "statement",
                    query.execute(pg_conn(&mut tx.conn)),
                )
                .await?;
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
                if tx.timed_out {
                    return Err(abandoned("statement"));
                }
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let rows = bounded_on(
                    &mut tx.timed_out,
                    "statement",
                    query.fetch_all(pg_conn(&mut tx.conn)),
                )
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
                if tx.timed_out {
                    return Err(abandoned("statement"));
                }
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let row = bounded_on(
                    &mut tx.timed_out,
                    "statement",
                    query.fetch_optional(pg_conn(&mut tx.conn)),
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
            Self::Postgres(mut tx) => {
                let mut timed_out = false;
                let commit = sqlx::raw_sql("COMMIT").execute(pg_conn(&mut tx.conn));
                match bounded_on(&mut timed_out, "COMMIT", commit).await {
                    Ok(_) => {
                        tx.finished = true;
                        Ok(())
                    }
                    // The server may have committed before the reply was lost.
                    Err(StoreError::Database { reason }) if timed_out => {
                        Err(StoreError::CommitUncertain { reason })
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        match self {
            Self::Sqlite(tx) => tx.rollback().await.map_err(|e| database_error(&e)),
            #[cfg(feature = "store-postgres")]
            // Dropping closes the connection, and the server rolls back.
            Self::Postgres(tx) if tx.timed_out => Ok(()),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(mut tx) => {
                let rollback = sqlx::raw_sql("ROLLBACK").execute(pg_conn(&mut tx.conn));
                bounded("ROLLBACK", rollback).await?;
                tx.finished = true;
                Ok(())
            }
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
