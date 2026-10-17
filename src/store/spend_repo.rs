//! The `spend_limit`, `spend_audit` and `spend_meta` tables.
//!
//! Each record is one row: `ord` is its position in the persisted sequence
//! (typed and opaque records interleaved exactly as the v1 state file
//! interleaves them), `opaque` marks a record the typed model could not
//! round-trip, and `body` is the record's JSON text. Opaque bodies are the
//! stored `serde_json::Value` verbatim, so they survive a save/load unchanged;
//! `typed_before` is not stored, it is recomputed from `ord` on load.
//!
//! [`save_spend`] writes a transition, not a snapshot: limits are replaced,
//! audit records are only appended and trimmed, which keeps the 10 000-record
//! audit log from being rewritten on every change.

use super::{sessions_repo::injected_load_failure, to_i64, Store, StoreError, Tx};
use crate::gateway::spend::store::{
    AuditRecord, OpaqueRecord, SpendLimit, SpendState, MAX_AUDIT_RECORDS,
};

const INSERT_LIMIT: &str =
    "INSERT INTO spend_limit (ord, id, opaque, body) VALUES ($1, $2, $3, $4)";
const INSERT_AUDIT: &str =
    "INSERT INTO spend_audit (ord, audit_id, opaque, body) VALUES ($1, $2, $3, $4)";
const UPSERT_META: &str = "INSERT INTO spend_meta (id, next_audit_id) VALUES (1, $1) \
    ON CONFLICT (id) DO UPDATE SET next_audit_id = excluded.next_audit_id";

/// One persisted row before it is bound: `(key, opaque, body)`.
type Row3 = (Option<String>, i64, String);
type AuditRow = (Option<i64>, i64, String);

fn encode_error(error: &serde_json::Error) -> StoreError {
    StoreError::Database {
        reason: format!("spend record did not serialize: {error}"),
    }
}

fn decode_error(table: &str, ord: i64, error: &serde_json::Error) -> StoreError {
    StoreError::Database {
        reason: format!("{table} row {ord} is not valid json for its kind: {error}"),
    }
}

/// `typed` and `opaque` merged in the order the state file uses: an opaque
/// record sits before the typed record at index `typed_before`.
fn interleave<'a, T>(typed: &'a [T], opaque: &'a [OpaqueRecord]) -> Vec<Either<'a, T>> {
    let mut output = Vec::with_capacity(typed.len() + opaque.len());
    let mut opaque = opaque.iter().peekable();
    for (index, record) in typed.iter().enumerate() {
        while opaque.peek().is_some_and(|o| o.typed_before <= index) {
            output.push(Either::Opaque(opaque.next().expect("peeked opaque record")));
        }
        output.push(Either::Typed(record));
    }
    output.extend(opaque.map(Either::Opaque));
    output
}

enum Either<'a, T> {
    Typed(&'a T),
    Opaque(&'a OpaqueRecord),
}

fn opaque_id(record: &OpaqueRecord) -> Option<String> {
    record
        .value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn limit_rows(state: &SpendState) -> Result<Vec<Row3>, StoreError> {
    interleave(&state.limits, &state.opaque_limits)
        .into_iter()
        .map(|entry| match entry {
            Either::Typed(limit) => Ok((
                Some(limit.id.clone()),
                0,
                serde_json::to_string(limit).map_err(|e| encode_error(&e))?,
            )),
            Either::Opaque(record) => Ok((opaque_id(record), 1, record.value.to_string())),
        })
        .collect()
}

fn audit_rows(state: &SpendState) -> Result<Vec<AuditRow>, StoreError> {
    interleave(&state.audit, &state.opaque_audit)
        .into_iter()
        .map(|entry| match entry {
            Either::Typed(record) => Ok((
                Some(to_i64(record.id)?),
                0,
                serde_json::to_string(record).map_err(|e| encode_error(&e))?,
            )),
            Either::Opaque(record) => Ok((
                record
                    .value
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|id| i64::try_from(id).ok()),
                1,
                record.value.to_string(),
            )),
        })
        .collect()
}

/// Move the store from `before` (what the previous save left, which is the
/// in-memory state until the caller replaces it) to `after`.
///
/// A save that changes nothing issues no statement at all, not even the hold
/// check, matching the file path's "unchanged upsert skips persistence".
/// Otherwise one transaction, started with [`Store::begin_durable`] so a lost
/// hold writes nothing:
///
/// 1. replace every `spend_limit` row;
/// 2. append the typed audit records with `id >= before.next_audit_id`. Opaque
///    audit records are never new: mutations only ever append typed ones, and
///    opaque ones come from a restore. They keep their rows until a trim
///    removes them. When the store holds no `spend_meta` row yet (nothing was
///    ever saved), every audit record of `after` is written instead;
/// 3. trim the oldest audit rows (typed and opaque alike) so at most
///    [`MAX_AUDIT_RECORDS`] remain, the same rule the in-memory store applies;
/// 4. upsert `spend_meta.next_audit_id`.
pub(crate) async fn save_spend(
    store: &Store,
    before: &SpendState,
    after: &SpendState,
) -> Result<(), StoreError> {
    if before == after {
        return Ok(());
    }
    let mut tx = store.begin_durable().await?;
    match write_state(&mut tx, before, after).await {
        Ok(()) => {
            tx.commit().await.map_err(|error| match error {
                StoreError::Database { reason } => StoreError::CommitUncertain { reason },
                other => other,
            })?;
            #[cfg(test)]
            if crate::store::sessions_repo::tests::take_lost_commit_ack(store) {
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

/// The write body of [`save_spend`] (without its unchanged-state shortcut),
/// inside a transaction the caller opened (and will commit or roll back), so
/// the import can add its `import_record`. With no `spend_meta` row yet it
/// writes every record of `after`.
pub(crate) async fn write_state(
    tx: &mut Tx,
    before: &SpendState,
    after: &SpendState,
) -> Result<(), StoreError> {
    let limits = limit_rows(after)?;
    let audit = audit_rows(after)?;
    let next_audit_id = to_i64(after.next_audit_id)?;
    let boundary = to_i64(before.next_audit_id)?;
    write(tx, &limits, &audit, boundary, next_audit_id).await
}

async fn write(
    tx: &mut Tx,
    limits: &[Row3],
    audit: &[AuditRow],
    boundary: i64,
    next_audit_id: i64,
) -> Result<(), StoreError> {
    let first_save = tx
        .fetch_optional("SELECT id FROM spend_meta WHERE id = 1", &[])
        .await?
        .is_none();

    tx.execute("DELETE FROM spend_limit", &[]).await?;
    for (ord, (id, opaque, body)) in (0_i64..).zip(limits) {
        tx.execute(
            INSERT_LIMIT,
            &[
                ord.into(),
                id.clone().into(),
                (*opaque).into(),
                body.as_str().into(),
            ],
        )
        .await?;
    }

    let mut next_ord = if first_save {
        0
    } else {
        tx.fetch_optional("SELECT MAX(ord) FROM spend_audit", &[])
            .await?
            .map(|row| row.opt_int(0))
            .transpose()?
            .flatten()
            .map_or(0, |max| max + 1)
    };
    for (id, opaque, body) in audit {
        let is_new = first_save || (*opaque == 0 && id.is_some_and(|id| id >= boundary));
        if !is_new {
            continue;
        }
        tx.execute(
            INSERT_AUDIT,
            &[
                next_ord.into(),
                (*id).into(),
                (*opaque).into(),
                body.as_str().into(),
            ],
        )
        .await?;
        next_ord += 1;
    }
    trim_audit(tx).await?;

    tx.execute(UPSERT_META, &[next_audit_id.into()]).await?;
    Ok(())
}

/// Delete the oldest audit rows beyond [`MAX_AUDIT_RECORDS`].
async fn trim_audit(tx: &mut Tx) -> Result<(), StoreError> {
    let count = tx
        .fetch_optional("SELECT COUNT(*) FROM spend_audit", &[])
        .await?
        .map_or(Ok(0), |row| row.int(0))?;
    let excess = count - to_i64(MAX_AUDIT_RECORDS as u64)?;
    if excess <= 0 {
        return Ok(());
    }
    let Some(cutoff) = tx
        .fetch_optional(
            "SELECT ord FROM spend_audit ORDER BY ord LIMIT 1 OFFSET $1",
            &[(excess - 1).into()],
        )
        .await?
    else {
        return Ok(());
    };
    tx.execute(
        "DELETE FROM spend_audit WHERE ord <= $1",
        &[cutoff.int(0)?.into()],
    )
    .await?;
    Ok(())
}

/// The stored spend state, or `None` when nothing was ever saved. A row that
/// does not decode is an error, never a skipped record: dropping it would let
/// the next save erase it.
pub(crate) async fn load_spend(store: &Store) -> Result<Option<SpendState>, StoreError> {
    injected_load_failure(store)?;
    read_spend(store.db().begin().await?).await
}

/// [`load_spend`] once every durable write in flight has ended
/// ([`Store::begin_settled`]). This is the read that settles a save whose
/// commit outcome is unknown.
pub(crate) async fn load_spend_settled(store: &Store) -> Result<Option<SpendState>, StoreError> {
    injected_load_failure(store)?;
    read_spend(store.begin_settled().await?).await
}

async fn read_spend(mut tx: Tx) -> Result<Option<SpendState>, StoreError> {
    let fetched = fetch_all(&mut tx).await;
    tx.rollback().await?;
    let (meta, limits, audit) = fetched?;
    let Some(meta) = meta else {
        return Ok(None);
    };

    let mut state = SpendState::default();
    for (ord, opaque, body) in limits {
        match decode_row::<SpendLimit>("spend_limit", ord, opaque, &body)? {
            Decoded::Typed(limit) => state.limits.push(limit),
            Decoded::Opaque(value) => state.opaque_limits.push(OpaqueRecord {
                typed_before: state.limits.len(),
                value,
            }),
        }
    }
    let mut next_audit_id = u64::try_from(meta).unwrap_or(1);
    for (ord, opaque, body) in audit {
        match decode_row::<AuditRecord>("spend_audit", ord, opaque, &body)? {
            Decoded::Typed(record) => {
                next_audit_id = next_audit_id.max(record.id.saturating_add(1));
                state.audit.push(record);
            }
            Decoded::Opaque(value) => {
                if let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) {
                    next_audit_id = next_audit_id.max(id.saturating_add(1));
                }
                state.opaque_audit.push(OpaqueRecord {
                    typed_before: state.audit.len(),
                    value,
                });
            }
        }
    }
    state.next_audit_id = next_audit_id;
    Ok(Some(state))
}

enum Decoded<T> {
    Typed(T),
    Opaque(serde_json::Value),
}

/// One row's body. A typed row (`opaque = 0`) stays typed only when it
/// re-serializes to the stored JSON, the same rule the state-file loader
/// applies: serde ignores unknown fields, so a row a newer binary wrote with a
/// field this one lacks would otherwise decode and lose that field on the next
/// save. Such a row is kept opaque, verbatim.
fn decode_row<T>(table: &str, ord: i64, opaque: i64, body: &str) -> Result<Decoded<T>, StoreError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| decode_error(table, ord, &e))?;
    if opaque != 0 {
        return Ok(Decoded::Opaque(value));
    }
    let typed: T =
        serde_json::from_value(value.clone()).map_err(|e| decode_error(table, ord, &e))?;
    if serde_json::to_value(&typed).map_err(|e| encode_error(&e))? == value {
        return Ok(Decoded::Typed(typed));
    }
    tracing::warn!(
        target: "shunt::store",
        table,
        ord,
        "preserving a spend record this binary cannot round-trip"
    );
    Ok(Decoded::Opaque(value))
}

type Loaded = (
    Option<i64>,
    Vec<(i64, i64, String)>,
    Vec<(i64, i64, String)>,
);

async fn fetch_all(tx: &mut Tx) -> Result<Loaded, StoreError> {
    let meta = tx
        .fetch_optional("SELECT next_audit_id FROM spend_meta WHERE id = 1", &[])
        .await?
        .map(|row| row.int(0))
        .transpose()?;
    let limits = tx
        .fetch_all(
            "SELECT ord, opaque, body FROM spend_limit ORDER BY ord",
            &[],
        )
        .await?;
    let audit = tx
        .fetch_all(
            "SELECT ord, opaque, body FROM spend_audit ORDER BY ord",
            &[],
        )
        .await?;
    let decode = |rows: Vec<super::Row>| -> Result<Vec<(i64, i64, String)>, StoreError> {
        rows.iter()
            .map(|row| Ok((row.int(0)?, row.int(1)?, row.text(2)?)))
            .collect()
    };
    Ok((meta, decode(limits)?, decode(audit)?))
}

#[cfg(test)]
pub(crate) mod tests;
