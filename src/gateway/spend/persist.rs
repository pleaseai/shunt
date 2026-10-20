use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use super::store::{
    canonical_amount, validate_limit, AuditRecord, OpaqueRecord, SpendLimit, SpendState,
};
use crate::{
    server::AppState,
    store::{
        import::LoadIssue,
        spend_repo::{load_spend, load_spend_settled, save_spend},
        Store, StoreError,
    },
};

const STATE_VERSION: u32 = 1;

#[derive(Debug)]
struct PersistedSpend {
    state: SpendState,
}

#[derive(Serialize)]
struct PersistedSpendRef {
    version: u32,
    limits: Vec<serde_json::Value>,
    audit: Vec<serde_json::Value>,
    next_audit_id: u64,
}

#[derive(Deserialize)]
struct PersistedSpendWire {
    version: u32,
    limits: Vec<serde_json::Value>,
    audit: Vec<serde_json::Value>,
    next_audit_id: u64,
}

pub async fn restore(state: &AppState) -> io::Result<()> {
    if let Some(store) = state.gateway_stores.store() {
        return restore_from_store(state, store).await;
    }
    let Some(path) = state
        .gateway_stores
        .spend
        .state_path()
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    let load_path = path.clone();
    match tokio::task::spawn_blocking(move || load(&load_path)).await {
        Ok(Ok(Some(persisted))) => {
            let limits = persisted.state.limits.len();
            let audit_records = persisted.state.audit.len();
            state.gateway_stores.spend.replace(persisted.state);
            tracing::info!(
                path = %path.display(),
                limits,
                audit_records,
                "restored gateway spend-limit state from disk"
            );
            Ok(())
        }
        Ok(Ok(None)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(io::Error::other(format!(
            "gateway spend-limit restore task panicked: {error}"
        ))),
    }
}

/// Persist `snapshot`. With a store attached the store is the only
/// destination; the caller holds `mutation_gate` and has not yet called
/// `replace`, so the in-memory state is exactly what the store holds.
pub(crate) async fn save(state: &AppState, snapshot: &SpendState) -> Result<(), String> {
    if let Some(store) = state.gateway_stores.store() {
        let before = state.gateway_stores.spend.export();
        return save_spend(store, &before, snapshot).await.map_err(|error| {
            crate::metrics::record_store_write_failure("spend");
            tracing::warn!(%error, "failed to persist gateway spend-limit state to the store");
            format!("failed to persist gateway spend-limit state: {error}")
        });
    }
    let Some(path) = state
        .gateway_stores
        .spend
        .state_path()
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || save_snapshot(&path, &snapshot))
        .await
        .map_err(|error| format!("gateway spend-limit persistence task panicked: {error}"))?
        .map_err(|error| format!("failed to persist gateway spend-limit state: {error}"))
}

/// [`save`] `next`, then make it the in-memory state, as one step a dropped
/// request cannot split. `gate` is the caller's `mutation_gate` guard; it moves
/// into the spawned task, so the store never gets ahead of memory (a later
/// mutation would reuse `next_audit_id`) and no second mutation starts before
/// this one has finished.
pub(crate) async fn commit(
    state: &AppState,
    gate: tokio::sync::OwnedMutexGuard<()>,
    next: SpendState,
) -> Result<(), String> {
    let state = state.clone();
    tokio::spawn(async move {
        let _gate = gate;
        match state.gateway_stores.store() {
            Some(store) => commit_to_store(&state, store, next).await,
            None => {
                save(&state, &next).await?;
                state.gateway_stores.spend.replace(next);
                Ok(())
            }
        }
    })
    .await
    .map_err(|error| format!("gateway spend-limit commit task failed: {error}"))?
}

/// The store half of [`commit`]. A commit whose outcome is unknown is settled
/// against what the store holds, read once that commit has ended
/// ([`load_spend_settled`]): memory follows the store, so audit ids never go
/// backwards. If the store cannot be read back, the next mutation resyncs.
async fn commit_to_store(state: &AppState, store: &Store, next: SpendState) -> Result<(), String> {
    let spend = &state.gateway_stores.spend;
    let Err(error) = save_spend(store, &spend.export(), &next).await else {
        spend.replace(next);
        return Ok(());
    };
    crate::metrics::record_store_write_failure("spend");
    tracing::warn!(%error, "failed to persist gateway spend-limit state to the store");
    if matches!(error, StoreError::CommitUncertain { .. }) {
        match load_spend_settled(store).await {
            Ok(Some(stored)) if stored == next => {
                spend.replace(next);
                return Ok(());
            }
            Ok(Some(stored)) => {
                spend.replace(stored);
            }
            Ok(None) => {}
            Err(_) => spend.set_needs_resync(true),
        }
    }
    Err(format!(
        "failed to persist gateway spend-limit state: {error}"
    ))
}

/// Before a mutation computes its snapshot (under `mutation_gate`): if an
/// earlier commit's outcome was left unknown, reload memory from the store,
/// failing with the store error while it still cannot be read.
pub(crate) async fn resync_if_needed(state: &AppState) -> Result<(), String> {
    let spend = &state.gateway_stores.spend;
    let Some(store) = state
        .gateway_stores
        .store()
        .filter(|_| spend.needs_resync())
    else {
        return Ok(());
    };
    let loaded = load_spend_settled(store)
        .await
        .map_err(|error| format!("failed to resync gateway spend-limit state: {error}"))?;
    if let Some(stored) = loaded {
        spend.replace(stored);
    }
    spend.set_needs_resync(false);
    Ok(())
}

async fn restore_from_store(state: &AppState, store: &Store) -> io::Result<()> {
    let loaded = load_spend(store).await.map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("gateway spend-limit state could not be loaded from the store: {error}"),
        )
    })?;
    if let Some(persisted) = loaded {
        tracing::info!(
            limits = persisted.limits.len(),
            audit_records = persisted.audit.len(),
            "restored gateway spend-limit state from the store"
        );
        state.gateway_stores.spend.replace(persisted);
    }
    Ok(())
}

fn load(path: &Path) -> io::Result<Option<PersistedSpend>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    crate::store::marker::warn_if_imported("spend", path, &bytes);
    parse(&bytes, path)
        .map(|state| Some(PersistedSpend { state }))
        .map_err(|issue| {
            let message = match issue {
                LoadIssue::NotJson(error) => {
                    format!("gateway spend-limit state is not valid json: {error}")
                }
                LoadIssue::Version { found, expected } => format!(
                    "gateway spend-limit state version mismatch: found {found}, expected {expected}"
                ),
            };
            io::Error::new(io::ErrorKind::InvalidData, message)
        })
}

/// The spend state a snapshot holds, for the store import. Strict, and the
/// same decode the store-less [`load`] performs.
pub(crate) fn parse_state(bytes: &[u8], path: &Path) -> Result<SpendState, LoadIssue> {
    parse(bytes, path)
}

/// Decode a snapshot's bytes; `path` is only for log context.
fn parse(bytes: &[u8], path: &Path) -> Result<SpendState, LoadIssue> {
    let not_json = |error: &dyn std::fmt::Display| LoadIssue::NotJson(error.to_string());
    let wire: PersistedSpendWire = serde_json::from_slice(bytes).map_err(|e| not_json(&e))?;
    if wire.version != STATE_VERSION {
        return Err(LoadIssue::Version {
            found: wire.version,
            expected: STATE_VERSION.to_string(),
        });
    }
    let mut limits = Vec::new();
    let mut opaque_limits = Vec::new();
    for value in wire.limits {
        let typed_before = limits.len();
        match serde_json::from_value::<SpendLimit>(value.clone()) {
            Ok(mut limit) => match validate_limit(&limit) {
                Ok(()) if serde_json::to_value(&limit).map_err(|e| not_json(&e))? == value => {
                    limit.amount = canonical_amount(limit.amount.as_deref())
                        .expect("validated persisted amount is canonicalizable");
                    limits.push(limit);
                }
                Ok(()) => {
                    tracing::warn!(
                        id = %limit.id,
                        "preserving lossy gateway spend-limit record from persisted state"
                    );
                    opaque_limits.push(OpaqueRecord {
                        typed_before,
                        value,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        id = %limit.id,
                        field = error.field(),
                        "preserving invalid gateway spend-limit record from persisted state"
                    );
                    opaque_limits.push(OpaqueRecord {
                        typed_before,
                        value,
                    });
                }
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "preserving malformed gateway spend-limit record from persisted state"
                );
                opaque_limits.push(OpaqueRecord {
                    typed_before,
                    value,
                });
            }
        }
    }
    let next_audit_id = wire
        .audit
        .iter()
        .filter_map(|record| record.get("id").and_then(serde_json::Value::as_u64))
        .map(|id| id.saturating_add(1))
        .max()
        .unwrap_or(1)
        .max(wire.next_audit_id);
    let (audit, opaque_audit) = parse_records::<AuditRecord>(wire.audit, path, "audit")?;
    Ok(SpendState {
        limits,
        opaque_limits,
        audit,
        opaque_audit,
        next_audit_id,
    })
}

pub(crate) fn parse_records<T>(
    values: Vec<serde_json::Value>,
    path: &Path,
    kind: &str,
) -> Result<(Vec<T>, Vec<OpaqueRecord>), LoadIssue>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let mut typed = Vec::new();
    let mut opaque = Vec::new();
    for value in values {
        let typed_before = typed.len();
        match serde_json::from_value::<T>(value.clone()) {
            Ok(record)
                if serde_json::to_value(&record)
                    .map_err(|e| LoadIssue::NotJson(e.to_string()))?
                    == value =>
            {
                typed.push(record);
            }
            Ok(_) => {
                tracing::warn!(
                    path = %path.display(),
                    kind,
                    "preserving lossy gateway spend-limit record from persisted state"
                );
                opaque.push(OpaqueRecord {
                    typed_before,
                    value,
                });
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    kind,
                    %error,
                    "preserving malformed gateway spend-limit record from persisted state"
                );
                opaque.push(OpaqueRecord {
                    typed_before,
                    value,
                });
            }
        }
    }
    Ok((typed, opaque))
}

fn interleave_records<T: Serialize>(
    typed: &[T],
    opaque: &[OpaqueRecord],
) -> io::Result<Vec<serde_json::Value>> {
    let mut output = Vec::with_capacity(typed.len() + opaque.len());
    let mut opaque = opaque.iter().peekable();
    for (index, record) in typed.iter().enumerate() {
        while opaque
            .peek()
            .is_some_and(|record| record.typed_before <= index)
        {
            output.push(opaque.next().expect("peeked opaque record").value.clone());
        }
        output.push(serde_json::to_value(record).map_err(io::Error::other)?);
    }
    output.extend(opaque.map(|record| record.value.clone()));
    Ok(output)
}

fn save_snapshot(path: &Path, state: &SpendState) -> io::Result<()> {
    let limits = interleave_records(&state.limits, &state.opaque_limits)?;
    let audit = interleave_records(&state.audit, &state.opaque_audit)?;
    let persisted = PersistedSpendRef {
        version: STATE_VERSION,
        limits,
        audit,
        next_audit_id: state.next_audit_id,
    };
    let json = serde_json::to_vec_pretty(&persisted).map_err(io::Error::other)?;
    crate::atomic_file::write_private_atomic(path, &json)
}

#[cfg(test)]
mod tests;
