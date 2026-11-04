//! The `pool_session_binding` table: one row per session-to-account binding
//! (ADR-0008 §6).
//!
//! Keyed by `(provider, session_hash)`, where `session_hash` is the lowercase
//! hex SHA-256 of the session's selection key; a raw session id is never
//! stored. `account_key` is the bound [`AccountKey`] as serde JSON text, as in
//! `pool_quota`, and `last_used` is unix seconds rounded down to the hour, as
//! the binding table exports it.
//!
//! Unlike `pool_quota`, bindings are written as deltas: [`diff`] compares the
//! current export with the rows last written ([`Snapshot`]) and
//! [`apply_diff`] upserts only new or changed rows and deletes the ones the
//! export no longer holds (moved away, expired, evicted, forgotten). A binding
//! is a cache, so [`load_bindings`] drops a row it cannot decode with a
//! warning instead of failing boot.

use std::collections::HashMap;

use super::{to_i64, Store, StoreError, Tx, Value};
use crate::accounts::{AccountKey, BindingRow};

const UPSERT: &str = "INSERT INTO pool_session_binding \
    (provider, session_hash, account_key, last_used) VALUES ($1, $2, $3, $4) \
    ON CONFLICT (provider, session_hash) DO UPDATE SET \
    account_key = excluded.account_key, last_used = excluded.last_used";

const DELETE: &str = "DELETE FROM pool_session_binding WHERE provider = $1 AND session_hash = $2";

const SELECT: &str = "SELECT provider, session_hash, account_key, last_used \
    FROM pool_session_binding ORDER BY provider, session_hash";

/// A row's primary key: `(provider, session_hash)`.
pub(crate) type RowKey = (String, String);

/// What a stored row holds besides its key: the account key's JSON text and
/// `last_used`. Compared as stored, so an unchanged binding is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stored {
    account_key: String,
    last_used: u64,
}

/// The rows the store holds, as last read or written.
pub(crate) type Snapshot = HashMap<RowKey, Stored>;

/// The writes that make the table equal an export.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Diff {
    pub(crate) upserts: Vec<(RowKey, Stored)>,
    pub(crate) deletes: Vec<RowKey>,
}

impl Diff {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty()
    }
}

fn encode_error(error: &serde_json::Error) -> StoreError {
    StoreError::Database {
        reason: format!("session binding account key did not serialize: {error}"),
    }
}

/// An export as the snapshot it would leave in the store.
pub(crate) fn snapshot_of(rows: &[BindingRow]) -> Result<Snapshot, StoreError> {
    rows.iter()
        .map(|row| {
            Ok((
                (row.provider.clone(), row.session_hash.clone()),
                Stored {
                    account_key: serde_json::to_string(&row.account)
                        .map_err(|e| encode_error(&e))?,
                    last_used: row.last_used,
                },
            ))
        })
        .collect()
}

/// The writes that turn `previous` into `current`: an upsert for every row
/// that is new or differs, a delete for every row `current` lacks.
pub(crate) fn diff(previous: &Snapshot, current: &Snapshot) -> Diff {
    let mut upserts: Vec<_> = current
        .iter()
        .filter(|(key, stored)| previous.get(*key) != Some(*stored))
        .map(|(key, stored)| (key.clone(), stored.clone()))
        .collect();
    let mut deletes: Vec<_> = previous
        .keys()
        .filter(|key| !current.contains_key(*key))
        .cloned()
        .collect();
    upserts.sort_by(|a, b| a.0.cmp(&b.0));
    deletes.sort();
    Diff { upserts, deletes }
}

/// Apply `diff` inside a transaction the caller opened (and will commit or
/// roll back).
pub(crate) async fn apply_diff(tx: &mut Tx, diff: &Diff) -> Result<(), StoreError> {
    for ((provider, session_hash), stored) in &diff.upserts {
        tx.execute(
            UPSERT,
            &[
                provider.as_str().into(),
                session_hash.as_str().into(),
                stored.account_key.as_str().into(),
                to_i64(stored.last_used)?.into(),
            ],
        )
        .await?;
    }
    for (provider, session_hash) in &diff.deletes {
        tx.execute(
            DELETE,
            &[
                Value::from(provider.as_str()),
                Value::from(session_hash.as_str()),
            ],
        )
        .await?;
    }
    Ok(())
}

/// The rows the table holds now, read inside `tx`, as a [`Snapshot`]. Every
/// row is kept, decodable or not, so a diff against it also deletes rows
/// that no longer decode.
pub(crate) async fn read_snapshot(tx: &mut Tx) -> Result<Snapshot, StoreError> {
    let rows = tx.fetch_all(SELECT, &[]).await?;
    rows.iter()
        .map(|row| {
            Ok((
                (row.text(0)?, row.text(1)?),
                Stored {
                    account_key: row.text(2)?,
                    // A negative stamp cannot be ours; it never equals an
                    // export, so the next write replaces or deletes it.
                    last_used: u64::try_from(row.int(3)?).unwrap_or(u64::MAX),
                },
            ))
        })
        .collect()
}

/// Every stored binding that decodes. A row whose account key is not valid
/// JSON or whose `last_used` is negative is dropped with a warning; it stays
/// in the table until the first flush, which reads the table's own
/// [`read_snapshot`] and so deletes it. Session-hash shape, expiry and the cap
/// are left to `AccountPool::import_bindings`, as on the file path.
pub(crate) async fn load_bindings(store: &Store) -> Result<Vec<BindingRow>, StoreError> {
    let mut tx = store.db().begin().await?;
    let fetched = tx.fetch_all(SELECT, &[]).await;
    tx.rollback().await?;
    let mut rows = Vec::new();
    for row in fetched? {
        let provider = row.text(0)?;
        let account = match serde_json::from_str::<AccountKey>(&row.text(2)?) {
            Ok(account) => account,
            Err(error) => {
                tracing::warn!(
                    provider = %provider,
                    %error,
                    "dropping a stored session binding whose account key does not decode"
                );
                continue;
            }
        };
        let last_used = row.int(3)?;
        let Ok(last_used) = u64::try_from(last_used) else {
            tracing::warn!(
                provider = %provider,
                last_used,
                "dropping a stored session binding with a negative last_used"
            );
            continue;
        };
        rows.push(BindingRow {
            provider,
            session_hash: row.text(1)?,
            account,
            last_used,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
