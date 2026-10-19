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
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
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

impl Db {
    /// Start a transaction. SQLite has one connection, so a second concurrent
    /// `begin` waits for the first to finish.
    pub async fn begin(&self) -> Result<Tx, StoreError> {
        match self {
            Self::Sqlite(pool) => Ok(Tx::Sqlite(
                pool.begin().await.map_err(|e| database_error(&e))?,
            )),
            #[cfg(feature = "store-postgres")]
            Self::Postgres(pool) => Ok(Tx::Postgres(
                pool.begin().await.map_err(|e| database_error(&e))?,
            )),
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
                let query = bind_values!(sqlx::query(AssertSqlSafe(sql.to_string())), params);
                let done = query
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| database_error(&e))?;
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
                let rows = query
                    .fetch_all(&mut **tx)
                    .await
                    .map_err(|e| database_error(&e))?;
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
                let row = query
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(|e| database_error(&e))?;
                Ok(row.map(Row::Postgres))
            }
        }
    }

    pub async fn commit(self) -> Result<(), StoreError> {
        match self {
            Self::Sqlite(tx) => tx.commit().await,
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => tx.commit().await,
        }
        .map_err(|e| database_error(&e))
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        match self {
            Self::Sqlite(tx) => tx.rollback().await,
            #[cfg(feature = "store-postgres")]
            Self::Postgres(tx) => tx.rollback().await,
        }
        .map_err(|e| database_error(&e))
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
