//! The `refresh_token` table: one row per refresh-token record, keyed by the
//! token's SHA-256 (the same form [`RefreshRecord`] holds in memory and the v1
//! state file holds on disk, so the store never holds a usable token).
//!
//! `identity_json` is the record's [`Identity`] as serde JSON text, exactly as
//! the state file stores it, and `inactive_since` is `NULL` for an active token
//! and the unix second a tombstone was written. The write is a *diff*, not a
//! snapshot: the gateway computes [`diff`] between the last export the store
//! accepted and the current one, and [`apply_diff`] applies it in one
//! transaction.

use std::collections::HashMap;

use super::{to_i64, Store, StoreError, Tx};
use crate::gateway::{approval::Identity, refresh::RefreshRecord};

const UPSERT: &str = "INSERT INTO refresh_token \
    (token_sha256, family, identity_json, inactive_since, issued_at) \
    VALUES ($1, $2, $3, $4, $5) \
    ON CONFLICT (token_sha256) DO UPDATE SET \
    family = excluded.family, identity_json = excluded.identity_json, \
    inactive_since = excluded.inactive_since, issued_at = excluded.issued_at";

/// What changed between two exports.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionsDiff {
    /// Records that are new or whose fields changed (a tombstone is a record
    /// whose `inactive_since` became set).
    pub upserts: Vec<RefreshRecord>,
    /// Token hashes `committed` held that the new export no longer does
    /// (expired or trimmed).
    pub deletes: Vec<String>,
}

impl SessionsDiff {
    pub(crate) fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty()
    }
}

/// The records of `after` that differ from `committed`, plus the keys of
/// `committed` that `after` dropped. Both lists are sorted by token hash so the
/// write order is deterministic.
pub(crate) fn diff(
    committed: &HashMap<String, RefreshRecord>,
    after: &HashMap<String, RefreshRecord>,
) -> SessionsDiff {
    let mut upserts: Vec<RefreshRecord> = after
        .iter()
        .filter(|(key, record)| committed.get(*key) != Some(record))
        .map(|(_, record)| record.clone())
        .collect();
    upserts.sort_by(|a, b| a.token_sha256.cmp(&b.token_sha256));
    let mut deletes: Vec<String> = committed
        .keys()
        .filter(|key| !after.contains_key(*key))
        .cloned()
        .collect();
    deletes.sort();
    SessionsDiff { upserts, deletes }
}

/// Index an export by token hash.
pub(crate) fn index(records: Vec<RefreshRecord>) -> HashMap<String, RefreshRecord> {
    records
        .into_iter()
        .map(|record| (record.token_sha256.clone(), record))
        .collect()
}

/// Whether `after` holds an active token `before` did not: a token the caller
/// is about to hand to a client.
pub(crate) fn adds_active(
    before: &HashMap<String, RefreshRecord>,
    after: &HashMap<String, RefreshRecord>,
) -> bool {
    after
        .values()
        .any(|record| record.inactive_since.is_none() && !before.contains_key(&record.token_sha256))
}

fn encode_error(error: &serde_json::Error) -> StoreError {
    StoreError::Database {
        reason: format!("refresh-token identity did not serialize: {error}"),
    }
}

fn decode_error(token_sha256: &str, reason: &str) -> StoreError {
    StoreError::Database {
        reason: format!("refresh_token row {token_sha256} is not valid: {reason}"),
    }
}

/// Apply `diff` in one transaction that starts with [`Store::begin_durable`],
/// so a lost hold writes nothing. Any failure rolls the whole write back. An
/// empty diff issues no statement at all.
pub(crate) async fn apply_diff(store: &Store, diff: &SessionsDiff) -> Result<(), StoreError> {
    if diff.is_empty() {
        return Ok(());
    }
    let mut tx = store.begin_durable().await?;
    match apply_diff_in(&mut tx, diff).await {
        Ok(()) => {
            tx.commit().await.map_err(|error| match error {
                StoreError::Database { reason } => StoreError::CommitUncertain { reason },
                other => other,
            })?;
            #[cfg(test)]
            if tests::take_lost_commit_ack(store) {
                return Err(StoreError::CommitUncertain {
                    reason: "injected: the acknowledgement was lost".to_string(),
                });
            }
            Ok(())
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// The write body of [`apply_diff`], inside a transaction the caller opened
/// (and will commit or roll back), so the import can add its `import_record`.
pub(crate) async fn apply_diff_in(tx: &mut Tx, diff: &SessionsDiff) -> Result<(), StoreError> {
    let mut rows = Vec::with_capacity(diff.upserts.len());
    for record in &diff.upserts {
        rows.push((
            record,
            serde_json::to_string(&record.identity).map_err(|e| encode_error(&e))?,
            record.inactive_since.map(to_i64).transpose()?,
            to_i64(record.issued_at)?,
        ));
    }
    write(tx, &rows, &diff.deletes).await
}

async fn write(
    tx: &mut Tx,
    rows: &[(&RefreshRecord, String, Option<i64>, i64)],
    deletes: &[String],
) -> Result<(), StoreError> {
    for (record, identity_json, inactive_since, issued_at) in rows {
        tx.execute(
            UPSERT,
            &[
                record.token_sha256.as_str().into(),
                record.family.as_str().into(),
                identity_json.as_str().into(),
                (*inactive_since).into(),
                (*issued_at).into(),
            ],
        )
        .await?;
    }
    for token in deletes {
        tx.execute(
            "DELETE FROM refresh_token WHERE token_sha256 = $1",
            &[token.as_str().into()],
        )
        .await?;
    }
    Ok(())
}

/// Every stored record. A row that does not decode is an error, never a
/// skipped row: a dropped tombstone would let a replayed token rotate again,
/// and the next diff would erase the row for good.
pub(crate) async fn load_sessions(store: &Store) -> Result<Vec<RefreshRecord>, StoreError> {
    injected_load_failure(store)?;
    read_sessions(store.db().begin().await?).await
}

/// [`load_sessions`] once every durable write in flight has ended
/// ([`Store::begin_settled`]). This is the read that settles a write whose
/// commit outcome is unknown.
pub(crate) async fn load_sessions_settled(store: &Store) -> Result<Vec<RefreshRecord>, StoreError> {
    injected_load_failure(store)?;
    read_sessions(store.begin_settled().await?).await
}

/// The armed `fail_next_load` fault in tests; nothing otherwise.
pub(crate) fn injected_load_failure(store: &Store) -> Result<(), StoreError> {
    #[cfg(test)]
    if tests::take_failed_load(store) {
        return Err(StoreError::Database {
            reason: "injected: load failed".to_string(),
        });
    }
    #[cfg(not(test))]
    let _ = store;
    Ok(())
}

async fn read_sessions(mut tx: Tx) -> Result<Vec<RefreshRecord>, StoreError> {
    let fetched = tx
        .fetch_all(
            "SELECT token_sha256, family, identity_json, inactive_since, issued_at \
             FROM refresh_token ORDER BY token_sha256",
            &[],
        )
        .await;
    tx.rollback().await?;
    fetched?
        .iter()
        .map(|row| {
            let token_sha256 = row.text(0)?;
            let identity: Identity = serde_json::from_str(&row.text(2)?)
                .map_err(|e| decode_error(&token_sha256, &format!("identity_json: {e}")))?;
            let inactive_since = row
                .opt_int(3)?
                .map(|value| {
                    u64::try_from(value)
                        .map_err(|_| decode_error(&token_sha256, "negative inactive_since"))
                })
                .transpose()?;
            let issued_at = u64::try_from(row.int(4)?)
                .map_err(|_| decode_error(&token_sha256, "negative issued_at"))?;
            Ok(RefreshRecord {
                token_sha256,
                identity,
                family: row.text(1)?,
                inactive_since,
                issued_at,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
