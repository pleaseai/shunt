use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use super::store::{
    canonical_amount, validate_limit, AuditRecord, OpaqueRecord, SpendLimit, SpendState,
};
use crate::server::AppState;

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

pub(crate) async fn save(state: &AppState, snapshot: &SpendState) -> Result<(), String> {
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

fn load(path: &Path) -> io::Result<Option<PersistedSpend>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let wire: PersistedSpendWire = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("gateway spend-limit state is not valid json: {error}"),
        )
    })?;
    if wire.version != STATE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "gateway spend-limit state version mismatch: found {}, expected {STATE_VERSION}",
                wire.version
            ),
        ));
    }
    let mut limits = Vec::new();
    let mut opaque_limits = Vec::new();
    for value in wire.limits {
        let typed_before = limits.len();
        match serde_json::from_value::<SpendLimit>(value.clone()) {
            Ok(mut limit) => match validate_limit(&limit) {
                Ok(()) if serde_json::to_value(&limit).map_err(io::Error::other)? == value => {
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
    Ok(Some(PersistedSpend {
        state: SpendState {
            limits,
            opaque_limits,
            audit,
            opaque_audit,
            next_audit_id,
        },
    }))
}

fn parse_records<T>(
    values: Vec<serde_json::Value>,
    path: &Path,
    kind: &str,
) -> io::Result<(Vec<T>, Vec<OpaqueRecord>)>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let mut typed = Vec::new();
    let mut opaque = Vec::new();
    for value in values {
        let typed_before = typed.len();
        match serde_json::from_value::<T>(value.clone()) {
            Ok(record) if serde_json::to_value(&record).map_err(io::Error::other)? == value => {
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
