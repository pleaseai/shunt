use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use super::store::{canonical_amount, validate_limit, AuditRecord, SpendLimit, SpendState};
use crate::server::AppState;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSpend {
    version: u32,
    #[serde(flatten)]
    state: SpendState,
}

#[derive(Serialize)]
struct PersistedSpendRef<'a> {
    version: u32,
    limits: Vec<serde_json::Value>,
    audit: &'a [AuditRecord],
    next_audit_id: u64,
}

#[derive(Deserialize)]
struct PersistedSpendWire {
    version: u32,
    limits: Vec<serde_json::Value>,
    audit: Vec<AuditRecord>,
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
        match serde_json::from_value::<SpendLimit>(value.clone()) {
            Ok(limit) => match validate_limit(&limit) {
                Ok(()) => {
                    let mut limit = limit;
                    limit.amount = canonical_amount(limit.amount.as_deref())
                        .expect("validated persisted amount is canonicalizable");
                    limits.push(limit);
                }
                Err(error) => {
                    tracing::warn!(
                        id = %limit.id,
                        field = error.field(),
                        "preserving invalid gateway spend-limit record from persisted state"
                    );
                    opaque_limits.push(value);
                }
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "preserving malformed gateway spend-limit record from persisted state"
                );
                opaque_limits.push(value);
            }
        }
    }
    let next_audit_id = wire
        .audit
        .iter()
        .map(|record| record.id.saturating_add(1))
        .max()
        .unwrap_or(1)
        .max(wire.next_audit_id);
    Ok(Some(PersistedSpend {
        version: wire.version,
        state: SpendState {
            limits,
            opaque_limits,
            audit: wire.audit,
            next_audit_id,
        },
    }))
}

fn save_snapshot(path: &Path, state: &SpendState) -> io::Result<()> {
    let mut limits = state
        .limits
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    limits.extend(state.opaque_limits.iter().cloned());
    let persisted = PersistedSpendRef {
        version: STATE_VERSION,
        limits,
        audit: &state.audit,
        next_audit_id: state.next_audit_id,
    };
    let json = serde_json::to_vec_pretty(&persisted).map_err(io::Error::other)?;
    crate::atomic_file::write_private_atomic(path, &json)
}

#[cfg(test)]
mod tests;
