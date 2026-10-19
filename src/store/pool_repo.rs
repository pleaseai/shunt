//! The `pool_quota` table: one row per observed physical account.
//!
//! `account_key` is the account's [`AccountKey`] and `quota_json` its
//! [`QuotaState`], both as serde JSON text, so the rows carry exactly what the
//! v3 state file carries and `import_quotas` sees the same values on either
//! path. Pool quota is write-behind state: [`save_pool`] replaces the whole
//! table with the latest export, never a delta.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{to_i64, Store, StoreError, Tx, Value};
use crate::accounts::{AccountKey, QuotaState};

const UPSERT: &str = "INSERT INTO pool_quota (account_key, quota_json, updated_at) \
    VALUES ($1, $2, $3) \
    ON CONFLICT (account_key) DO UPDATE SET \
    quota_json = excluded.quota_json, updated_at = excluded.updated_at";

fn encode_error(error: &serde_json::Error) -> StoreError {
    StoreError::Database {
        reason: format!("pool quota did not serialize: {error}"),
    }
}

fn decode_error(column: &str, error: &serde_json::Error) -> StoreError {
    StoreError::Database {
        reason: format!("pool_quota.{column} is not valid json: {error}"),
    }
}

/// Make `pool_quota` equal `exported`: upsert every entry and delete the keys
/// the export no longer holds, in one transaction that starts with
/// [`Store::begin_durable`] (so a lost hold writes nothing). Any failure rolls
/// the whole write back.
pub(crate) async fn save_pool(
    store: &Store,
    exported: &[(AccountKey, QuotaState)],
) -> Result<(), StoreError> {
    let mut tx = store.begin_durable().await?;
    match write_pool(&mut tx, exported).await {
        Ok(()) => tx.commit().await,
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// The write body of [`save_pool`], inside a transaction the caller opened
/// (and will commit or roll back), so the import can add its `import_record`.
pub(crate) async fn write_pool(
    tx: &mut Tx,
    exported: &[(AccountKey, QuotaState)],
) -> Result<(), StoreError> {
    let mut rows = Vec::with_capacity(exported.len());
    for (key, quota) in exported {
        rows.push((
            serde_json::to_string(key).map_err(|e| encode_error(&e))?,
            serde_json::to_string(quota).map_err(|e| encode_error(&e))?,
        ));
    }
    let now = to_i64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    )?;
    write_rows(tx, &rows, now).await
}

async fn write_rows(tx: &mut Tx, rows: &[(String, String)], now: i64) -> Result<(), StoreError> {
    for (key, json) in rows {
        tx.execute(
            UPSERT,
            &[key.as_str().into(), json.as_str().into(), now.into()],
        )
        .await?;
    }
    let keep: HashSet<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
    let stored = tx
        .fetch_all("SELECT account_key FROM pool_quota", &[])
        .await?;
    for row in stored {
        let key = row.text(0)?;
        if !keep.contains(key.as_str()) {
            tx.execute(
                "DELETE FROM pool_quota WHERE account_key = $1",
                &[Value::from(key)],
            )
            .await?;
        }
    }
    Ok(())
}

/// Every stored account's quota. A row that does not decode is an error, not a
/// skipped row: the caller decides whether to boot without it.
pub(crate) async fn load_pool(store: &Store) -> Result<Vec<(AccountKey, QuotaState)>, StoreError> {
    let mut tx = store.db().begin().await?;
    let fetched = tx
        .fetch_all(
            "SELECT account_key, quota_json FROM pool_quota ORDER BY account_key",
            &[],
        )
        .await;
    tx.rollback().await?;
    fetched?
        .iter()
        .map(|row| {
            let key =
                serde_json::from_str(&row.text(0)?).map_err(|e| decode_error("account_key", &e))?;
            let quota =
                serde_json::from_str(&row.text(1)?).map_err(|e| decode_error("quota_json", &e))?;
            Ok((key, quota))
        })
        .collect()
}

#[cfg(test)]
mod tests;
