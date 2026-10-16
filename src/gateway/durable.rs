//! The store-backed path for refresh-token mutations (ADR-0006, "Gateway
//! durable path").
//!
//! Without a store the token endpoint keeps the file path
//! ([`super::persist::save_if_dirty`]). With a store, every mutation that hands
//! a client a token commits to the store *before* the response, under one
//! store-owned lock that spans mutate, export and write:
//!
//! 1. `before = export()`, mutate, `after = export()`;
//! 2. write `diff(committed, after)` in one transaction, where `committed` is
//!    the last export the store accepted;
//! 3. on success `committed = after`.
//!
//! On a failed write the outcome depends on what the mutation did.
//!
//! - It **added an active token** (device-grant issue, rotation): memory is
//!   rolled back to `before` and the caller answers `503
//!   temporarily_unavailable`. Rollback is what keeps the client's retry from
//!   becoming a replay that revokes its own family. `before` already holds any
//!   earlier pending revocation, so a rollback never undoes one.
//! - It **only revoked** (a replayed tombstone revokes its family): memory
//!   keeps the revocation and the caller answers the usual `401
//!   invalid_grant`. Revocation fails closed and a store outage never undoes
//!   it. `committed` did not advance, so the next durable write, or the store
//!   persister tick ([`flush_pending`]), writes the pending revocation.

use std::collections::HashMap;

use tokio::sync::Mutex;

use super::{approval::Identity, refresh::RefreshRecord, store::GatewayStores};
use crate::{
    server::AppState,
    store::{
        sessions_repo::{adds_active, apply_diff, diff, index, load_sessions},
        Store, StoreError,
    },
};

/// The store-owned lock and the last export the store accepted. One per
/// [`GatewayStores`]; it replaces `persist_gate` for the store path.
#[derive(Default)]
pub struct DurableSessions {
    committed: Mutex<HashMap<String, RefreshRecord>>,
}

/// A mutation's result plus how its store write went.
struct Committed<T> {
    value: T,
    /// `Some` when the write failed.
    error: Option<StoreError>,
    /// Memory was restored to its pre-mutation contents.
    rolled_back: bool,
}

/// The result of a store-backed rotation.
pub(super) enum Rotation {
    Rotated(Identity, String),
    /// Unknown, expired or replayed token: `401 invalid_grant`.
    Rejected,
    /// The new token could not be committed and memory was rolled back:
    /// `503 temporarily_unavailable`.
    Unavailable,
}

fn record_failure(error: &StoreError, what: &str) {
    crate::metrics::record_store_write_failure("gateway");
    tracing::warn!(%error, "failed to persist gateway login sessions to the store ({what})");
}

/// Run `mutate` and commit its token diff, holding the durable lock from the
/// first export to the end of the write.
async fn commit<T>(
    stores: &GatewayStores,
    store: &Store,
    mutate: impl FnOnce(&super::refresh::RefreshTokenStore) -> T,
) -> Committed<T> {
    let mut committed = stores.durable_sessions.committed.lock().await;
    let before = index(stores.refresh_tokens.export());
    let value = mutate(&stores.refresh_tokens);
    let after = index(stores.refresh_tokens.export());

    let pending = diff(&committed, &after);
    match apply_diff(store, &pending).await {
        Ok(()) => {
            *committed = after;
            Committed {
                value,
                error: None,
                rolled_back: false,
            }
        }
        Err(error) => {
            let rolled_back = adds_active(&before, &after);
            record_failure(
                &error,
                if rolled_back {
                    "rolled back"
                } else {
                    "revocation kept in memory"
                },
            );
            if rolled_back {
                stores.refresh_tokens.import(before.into_values());
            }
            Committed {
                value,
                error: Some(error),
                rolled_back,
            }
        }
    }
}

/// Issue a refresh token for a completed device grant and commit it. An error
/// means nothing was issued (memory is rolled back).
pub(super) async fn issue(
    stores: &GatewayStores,
    store: &Store,
    identity: Identity,
) -> Result<String, StoreError> {
    let outcome = commit(stores, store, |tokens| tokens.issue(identity)).await;
    match outcome.error {
        None => Ok(outcome.value),
        Some(error) => Err(error),
    }
}

/// Rotate `presented` and commit the diff.
pub(super) async fn rotate(stores: &GatewayStores, store: &Store, presented: &str) -> Rotation {
    let outcome = commit(stores, store, |tokens| tokens.rotate(presented)).await;
    match (outcome.value, outcome.error) {
        (Some((identity, token)), None) => Rotation::Rotated(identity, token),
        (Some(_), Some(_)) => {
            debug_assert!(
                outcome.rolled_back,
                "a rotation always adds an active token"
            );
            Rotation::Unavailable
        }
        // Replayed or unknown: 401 whether or not the revocation reached the
        // store; the pending revocation stays in memory.
        (None, _) => Rotation::Rejected,
    }
}

/// Restore refresh sessions from the store at boot and make them the
/// `committed` baseline. A no-op without an attached store. Fail-loud: a store
/// that cannot be read, or a row that does not decode, returns its error.
///
/// The baseline is what was *loaded*, before the import's expiry sweep, so the
/// first [`flush_pending`] also deletes the rows that expired while down.
pub async fn restore_from_store(state: &AppState) -> Result<(), StoreError> {
    let Some(store) = state.gateway_stores.store() else {
        return Ok(());
    };
    let records = load_sessions(store).await?;
    let count = records.len();
    let mut committed = state.gateway_stores.durable_sessions.committed.lock().await;
    let baseline = index(records);
    state
        .gateway_stores
        .refresh_tokens
        .import(baseline.values().cloned());
    *committed = baseline;
    tracing::info!(
        refresh_tokens = count,
        "restored gateway login sessions from the store"
    );
    Ok(())
}

/// Write whatever differs between the last accepted export and memory: a
/// revocation a failed write left pending, or swept rows. A no-op without an
/// attached store or when nothing differs. Used by the store persister tick and
/// by shutdown; a failure counts on `shunt.store.write_failures` and leaves
/// `committed` where it was, so the next call retries.
pub async fn flush_pending(state: &AppState) -> Result<(), StoreError> {
    let Some(store) = state.gateway_stores.store() else {
        return Ok(());
    };
    let stores = &state.gateway_stores;
    let mut committed = stores.durable_sessions.committed.lock().await;
    let after = index(stores.refresh_tokens.export());
    let pending = diff(&committed, &after);
    if pending.is_empty() {
        return Ok(());
    }
    match apply_diff(store, &pending).await {
        Ok(()) => {
            *committed = after;
            Ok(())
        }
        Err(error) => {
            record_failure(&error, "pending flush");
            Err(error)
        }
    }
}
