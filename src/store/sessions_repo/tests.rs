//! `sessions_repo` against SQLite and PostgreSQL through the contract harness.
//! Failures are injected inside the engine (a trigger that aborts writes to
//! `refresh_token`), then removed.

use std::collections::HashMap;

use super::{
    adds_active, apply_diff, apply_diff_in, diff, index, load_sessions, load_sessions_settled,
    SessionsDiff,
};
use crate::gateway::{approval::Identity, refresh::RefreshRecord};
use crate::store::contract::store_contract_test;
use crate::store::{Db, Store, StoreError};

fn record(token: &str, family: &str, inactive_since: Option<u64>) -> RefreshRecord {
    RefreshRecord {
        token_sha256: token.to_string(),
        identity: Identity {
            sub: "dev@example.com".into(),
            email: "dev@example.com".into(),
            name: "dev".into(),
        },
        family: family.to_string(),
        inactive_since,
        issued_at: 1_000,
    }
}

fn map(records: Vec<RefreshRecord>) -> HashMap<String, RefreshRecord> {
    index(records)
}

/// Armed faults, keyed by the store's hold state. The key is a `Weak`, not the
/// raw address: a weak reference keeps the allocation, so a later store can
/// never be handed an earlier store's address and inherit its unconsumed fault,
/// and entries whose store is gone are purged.
static ARMED_FAULTS: std::sync::Mutex<
    Vec<(std::sync::Weak<crate::store::hold::HoldState>, &'static str)>,
> = std::sync::Mutex::new(Vec::new());

fn arm(store: &Store, fault: &'static str) {
    let mut armed = ARMED_FAULTS.lock().unwrap();
    armed.retain(|(owner, _)| owner.strong_count() > 0);
    armed.push((std::sync::Arc::downgrade(&store.hold), fault));
}

/// Consume one armed `fault` for `store`; each fault fires once.
fn take(store: &Store, fault: &'static str) -> bool {
    let owner = std::sync::Arc::downgrade(&store.hold);
    let mut armed = ARMED_FAULTS.lock().unwrap();
    armed.retain(|(owner, _)| owner.strong_count() > 0);
    armed
        .iter()
        .position(|(armed_owner, armed_fault)| {
            std::sync::Weak::ptr_eq(armed_owner, &owner) && *armed_fault == fault
        })
        .is_some_and(|at| {
            armed.remove(at);
            true
        })
}

/// Make the next [`apply_diff`] or `save_spend` on `store` report an unknown
/// commit outcome after the write really committed.
pub(crate) fn lose_next_commit_ack(store: &Store) {
    arm(store, "commit_ack");
}

pub(crate) fn take_lost_commit_ack(store: &Store) -> bool {
    take(store, "commit_ack")
}

/// Make the next `load_sessions` or `load_spend` on `store` fail.
pub(crate) fn fail_next_load(store: &Store) {
    arm(store, "load");
}

pub(crate) fn take_failed_load(store: &Store) -> bool {
    take(store, "load")
}

/// Hold the instance-hold row (and, on SQLite, the only connection), so every
/// durable write blocks in `begin_durable` until the returned transaction is
/// rolled back.
pub(crate) async fn block_durable_writes(store: &Store) -> crate::store::Tx {
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "UPDATE instance_hold SET heartbeat_at = heartbeat_at WHERE id = 1",
        &[],
    )
    .await
    .unwrap();
    tx
}

async fn run(store: &Store, statements: &[String]) {
    let mut tx = store.db().begin().await.unwrap();
    for statement in statements {
        tx.execute(statement, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
}

/// Make the listed operations (`INSERT`, `UPDATE`, `DELETE`) on `refresh_token`
/// abort inside the engine.
pub(crate) async fn fail_ops(store: &Store, ops: &[&str]) {
    let mut statements = Vec::new();
    match store.db() {
        Db::Sqlite(_) => {
            for op in ops {
                statements.push(format!(
                    "CREATE TRIGGER fail_refresh_{op} BEFORE {op} ON refresh_token \
                     BEGIN SELECT RAISE(ABORT, 'injected'); END"
                ));
            }
        }
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => {
            statements.push(
                "CREATE OR REPLACE FUNCTION fail_refresh_fn() RETURNS trigger AS \
                 $body$ BEGIN RAISE EXCEPTION 'injected'; END $body$ LANGUAGE plpgsql"
                    .to_string(),
            );
            for op in ops {
                statements.push(format!(
                    "CREATE TRIGGER fail_refresh_{op} BEFORE {op} ON refresh_token \
                     FOR EACH ROW EXECUTE FUNCTION fail_refresh_fn()"
                ));
            }
        }
    }
    run(store, &statements).await;
}

pub(crate) async fn fail_writes(store: &Store) {
    fail_ops(store, &["INSERT", "UPDATE", "DELETE"]).await;
}

pub(crate) async fn allow_writes(store: &Store) {
    let mut statements = Vec::new();
    for op in ["INSERT", "UPDATE", "DELETE"] {
        match store.db() {
            Db::Sqlite(_) => statements.push(format!("DROP TRIGGER IF EXISTS fail_refresh_{op}")),
            #[cfg(feature = "store-postgres")]
            Db::Postgres(_) => {
                statements.push(format!(
                    "DROP TRIGGER IF EXISTS fail_refresh_{op} ON refresh_token"
                ));
            }
        }
    }
    run(store, &statements).await;
}

async fn stored(store: &Store) -> HashMap<String, RefreshRecord> {
    map(load_sessions(store).await.unwrap())
}

#[test]
fn diff_lists_new_changed_and_dropped_records() {
    let committed = map(vec![
        record("same", "f1", None),
        record("active", "f1", None),
        record("gone", "f2", None),
    ]);
    let after = map(vec![
        record("same", "f1", None),
        record("active", "f1", Some(2_000)),
        record("new", "f1", None),
    ]);
    let changes = diff(&committed, &after);
    assert_eq!(
        changes,
        SessionsDiff {
            upserts: vec![
                record("active", "f1", Some(2_000)),
                record("new", "f1", None)
            ],
            deletes: vec!["gone".to_string()],
        }
    );
    assert!(diff(&after, &after).is_empty());
}

#[test]
fn adds_active_ignores_tombstones_and_removals() {
    let before = map(vec![record("a", "f", None)]);
    assert!(adds_active(
        &before,
        &map(vec![record("a", "f", None), record("b", "f", None)])
    ));
    assert!(!adds_active(&before, &map(vec![record("a", "f", Some(5))])));
    assert!(!adds_active(&before, &map(vec![])));
    // A *new* tombstone is not a token a client receives.
    assert!(!adds_active(
        &before,
        &map(vec![record("a", "f", None), record("b", "f", Some(5))])
    ));
}

store_contract_test!(nothing_saved_loads_empty, |store| {
    assert!(load_sessions(&store).await.unwrap().is_empty());
});

store_contract_test!(apply_diff_round_trips_tombstones_and_deletes, |store| {
    let first = map(vec![
        record("a", "f1", None),
        record("b", "f1", None),
        record("c", "f2", None),
    ]);
    apply_diff(&store, &diff(&HashMap::new(), &first))
        .await
        .unwrap();
    assert_eq!(stored(&store).await, first);

    // a is superseded (tombstoned), d is new, c is swept.
    let second = map(vec![
        record("a", "f1", Some(1_500)),
        record("b", "f1", None),
        record("d", "f1", None),
    ]);
    apply_diff(&store, &diff(&first, &second)).await.unwrap();
    let loaded = stored(&store).await;
    assert_eq!(loaded, second);
    assert_eq!(loaded["a"].inactive_since, Some(1_500));
    assert_eq!(loaded["b"].inactive_since, None);
});

store_contract_test!(an_empty_diff_touches_nothing, |store| {
    fail_writes(&store).await;
    apply_diff(&store, &SessionsDiff::default()).await.unwrap();
    allow_writes(&store).await;
});

store_contract_test!(a_failed_diff_is_all_or_nothing, |store| {
    let first = map(vec![record("a", "f", None), record("b", "f", None)]);
    apply_diff(&store, &diff(&HashMap::new(), &first))
        .await
        .unwrap();

    // The upserts succeed inside the transaction, the delete aborts: the whole
    // write must roll back, including the tombstone.
    fail_ops(&store, &["DELETE"]).await;
    let second = map(vec![record("a", "f", Some(9)), record("c", "f", None)]);
    let error = apply_diff(&store, &diff(&first, &second))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
    allow_writes(&store).await;
    assert_eq!(stored(&store).await, first);
});

store_contract_test!(a_lost_hold_writes_nothing, |store| {
    run(
        &store,
        &["UPDATE instance_hold SET holder_id = 'ghost' WHERE id = 1".to_string()],
    )
    .await;
    let changes = diff(&HashMap::new(), &map(vec![record("a", "f", None)]));
    let error = apply_diff(&store, &changes).await.unwrap_err();
    assert!(matches!(error, StoreError::HoldLost { .. }), "{error:?}");
    assert!(load_sessions(&store).await.unwrap().is_empty());
});

store_contract_test!(an_undecodable_row_fails_loading, |store| {
    run(
        &store,
        &["INSERT INTO refresh_token (token_sha256, family, identity_json, inactive_since, issued_at) \
           VALUES ('x', 'f', 'not json', NULL, 1)"
            .to_string()],
    )
    .await;
    let error = load_sessions(&store).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
});

store_contract_test!(a_negative_timestamp_fails_loading, |store| {
    run(
        &store,
        &["INSERT INTO refresh_token (token_sha256, family, identity_json, inactive_since, issued_at) \
           VALUES ('y', 'f', '{\"sub\":\"s\",\"email\":\"e\",\"name\":\"n\"}', -5, 1)"
            .to_string()],
    )
    .await;
    let error = load_sessions(&store).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
});

store_contract_test!(
    a_settled_load_waits_for_the_durable_write_in_flight,
    |store| {
        // A durable write that has not committed yet holds the instance-hold row
        // (and, on SQLite, the only connection).
        let written = map(vec![record("a", "f1", None)]);
        let mut tx = store.begin_durable().await.unwrap();
        apply_diff_in(&mut tx, &diff(&HashMap::new(), &written))
            .await
            .unwrap();

        let reader = tokio::spawn({
            let store = store.clone();
            async move { load_sessions_settled(&store).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !reader.is_finished(),
            "a settled load must wait for the write in flight, not read the state before it"
        );

        tx.commit().await.unwrap();
        assert_eq!(map(reader.await.unwrap().unwrap()), written);
    }
);

store_contract_test!(
    a_commit_that_outlasts_the_bound_is_uncertain_and_settles_once_it_lands,
    |store| {
        // Only PostgreSQL bounds a round trip: SQLite is a local file.
        if !matches!(store.db(), Db::Sqlite(_)) {
            #[cfg(feature = "store-postgres")]
            {
                use std::time::Duration;
                // A deferred trigger makes COMMIT itself take a second, the way
                // a COMMIT whose reply is lost looks to the client.
                run(
                    &store,
                    &[
                        "CREATE OR REPLACE FUNCTION slow_commit_fn() RETURNS trigger AS \
                         $body$ BEGIN PERFORM pg_sleep(1); RETURN NULL; END $body$ LANGUAGE plpgsql"
                            .to_string(),
                        "CREATE CONSTRAINT TRIGGER slow_commit AFTER INSERT ON refresh_token \
                         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION slow_commit_fn()"
                            .to_string(),
                    ],
                )
                .await;
                let written = map(vec![record("a", "f1", None)]);

                // Only COMMIT gets the short bound, so a slow BEGIN or INSERT on
                // a loaded runner cannot time out first.
                let bound =
                    crate::store::db::set_round_trip_timeout("COMMIT", Duration::from_millis(300));
                let error = apply_diff(&store, &diff(&HashMap::new(), &written))
                    .await
                    .unwrap_err();
                drop(bound);
                assert!(
                    matches!(error, StoreError::CommitUncertain { .. }),
                    "a COMMIT past the bound must be uncertain, not failed: {error:?}"
                );

                // The server finishes the COMMIT after the client gave up; the
                // settled read waits for it and sees the write.
                assert_eq!(map(load_sessions_settled(&store).await.unwrap()), written);
            }
        }
    }
);
