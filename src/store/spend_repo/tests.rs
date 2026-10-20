//! `spend_repo` against SQLite and PostgreSQL through the contract harness.
//! Failures are injected inside the engine (a trigger that aborts writes),
//! then removed.

use super::{load_spend, load_spend_settled, save_spend, write_state};
use crate::gateway::spend::store::{
    OpaqueRecord, Period, Scope, SpendState, SpendStore, MAX_AUDIT_RECORDS,
};
use crate::store::contract::store_contract_test;
use crate::store::{Db, Store, StoreError};

fn fresh() -> SpendState {
    SpendState {
        next_audit_id: 1,
        ..SpendState::default()
    }
}

fn now(n: usize) -> String {
    format!("2026-10-01T00:00:{:02}.000Z", n % 60)
}

fn upsert(state: SpendState, period: Period, amount: &str, n: usize) -> SpendState {
    SpendStore::upsert_state(
        state,
        Scope::Organization,
        period,
        Some(amount.to_string()),
        "admin-key:writer",
        now(n),
    )
    .0
}

/// Save `after` as the transition from `before` and require success.
async fn step(store: &Store, before: &SpendState, after: SpendState) -> SpendState {
    save_spend(store, before, &after).await.unwrap();
    after
}

async fn run(store: &Store, statements: &[String]) {
    let mut tx = store.db().begin().await.unwrap();
    for statement in statements {
        tx.execute(statement, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
}

/// Make every insert/update/delete on the spend tables abort in the engine.
pub(crate) async fn fail_writes(store: &Store) {
    let mut statements = Vec::new();
    for table in ["spend_limit", "spend_audit", "spend_meta"] {
        match store.db() {
            Db::Sqlite(_) => {
                for op in ["INSERT", "UPDATE", "DELETE"] {
                    statements.push(format!(
                        "CREATE TRIGGER fail_{table}_{op} BEFORE {op} ON {table} \
                         BEGIN SELECT RAISE(ABORT, 'injected'); END"
                    ));
                }
            }
            #[cfg(feature = "store-postgres")]
            Db::Postgres(_) => {
                statements.push(
                    "CREATE OR REPLACE FUNCTION fail_spend_fn() RETURNS trigger AS \
                     $body$ BEGIN RAISE EXCEPTION 'injected'; END $body$ LANGUAGE plpgsql"
                        .to_string(),
                );
                statements.push(format!(
                    "CREATE TRIGGER fail_{table} BEFORE INSERT OR UPDATE OR DELETE ON {table} \
                     FOR EACH ROW EXECUTE FUNCTION fail_spend_fn()"
                ));
            }
        }
    }
    run(store, &statements).await;
}

pub(crate) async fn allow_writes(store: &Store) {
    let mut statements = Vec::new();
    for table in ["spend_limit", "spend_audit", "spend_meta"] {
        match store.db() {
            Db::Sqlite(_) => {
                for op in ["INSERT", "UPDATE", "DELETE"] {
                    statements.push(format!("DROP TRIGGER fail_{table}_{op}"));
                }
            }
            #[cfg(feature = "store-postgres")]
            Db::Postgres(_) => statements.push(format!("DROP TRIGGER fail_{table} ON {table}")),
        }
    }
    run(store, &statements).await;
}

async fn count(store: &Store, table: &str) -> i64 {
    let mut tx = store.db().begin().await.unwrap();
    let row = tx
        .fetch_optional(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    row.int(0).unwrap()
}

store_contract_test!(nothing_saved_loads_none, |store| {
    assert_eq!(load_spend(&store).await.unwrap(), None);
});

store_contract_test!(save_then_load_round_trips_state, |store| {
    let before = fresh();
    let state = upsert(before.clone(), Period::Monthly, "100", 1);
    let state = step(&store, &before, state.clone()).await;
    let next = upsert(state.clone(), Period::Monthly, "200", 2);
    let next = upsert(next, Period::Daily, "5", 3);
    let next = step(&store, &state, next).await;

    let loaded = load_spend(&store).await.unwrap().unwrap();
    assert_eq!(loaded, next);
    assert_eq!(loaded.audit.len(), 3);
    assert_eq!(count(&store, "spend_audit").await, 3);
});

store_contract_test!(
    opaque_records_round_trip_in_their_original_positions,
    |store| {
        let mut state = fresh();
        state = upsert(state, Period::Daily, "1", 1);
        state = upsert(state, Period::Weekly, "2", 2);
        state = upsert(state, Period::Monthly, "3", 3);
        // Opaque limits: leading, between typed, trailing; with an id and without.
        state.opaque_limits = vec![
            OpaqueRecord {
                typed_before: 0,
                value: serde_json::json!({"id":"spl_lead","amount":7}),
            },
            OpaqueRecord {
                typed_before: 2,
                value: serde_json::json!({"weird":[1,2,{"x":null}],"id":"spl_mid"}),
            },
            OpaqueRecord {
                typed_before: 2,
                value: serde_json::json!("not even an object"),
            },
            OpaqueRecord {
                typed_before: 3,
                value: serde_json::json!({"id":"spl_tail"}),
            },
        ];
        // Opaque audit: before the first, between, and trailing records.
        state.opaque_audit = vec![
            OpaqueRecord {
                typed_before: 0,
                value: serde_json::json!({"id":0,"note":"ancient"}),
            },
            OpaqueRecord {
                typed_before: 1,
                value: serde_json::json!({"id":"not a number"}),
            },
            OpaqueRecord {
                typed_before: 3,
                value: serde_json::json!({"id":99,"actor":false}),
            },
        ];
        state.next_audit_id = 100;
        // First save of a non-empty state writes everything, opaque included.
        save_spend(&store, &fresh(), &state).await.unwrap();
        assert_eq!(load_spend(&store).await.unwrap().unwrap(), state);

        // A later mutation keeps every opaque record at its position.
        let mut next = upsert(state.clone(), Period::Weekly, "20", 4);
        next = SpendStore::delete_state(next.clone(), &next.limits[0].id.clone(), "a", now(5))
            .unwrap()
            .0;
        save_spend(&store, &state, &next).await.unwrap();
        assert_eq!(load_spend(&store).await.unwrap().unwrap(), next);
        assert_eq!(count(&store, "spend_limit").await, 6);
    }
);

store_contract_test!(the_10001st_audit_record_trims_the_oldest, |store| {
    let mut state = fresh();
    state = upsert(state, Period::Monthly, "0", 0);
    state = step(&store, &fresh(), state).await;
    // Fill to the cap in one save, then add one more.
    let mut full = state.clone();
    for n in 1..MAX_AUDIT_RECORDS {
        full = upsert(full, Period::Monthly, &n.to_string(), n);
    }
    assert_eq!(full.audit.len(), MAX_AUDIT_RECORDS);
    let full = step(&store, &state, full).await;
    assert_eq!(count(&store, "spend_audit").await, MAX_AUDIT_RECORDS as i64);

    let over = upsert(full.clone(), Period::Monthly, "999999", 7);
    assert_eq!(over.audit.len(), MAX_AUDIT_RECORDS);
    assert_eq!(over.audit[0].id, 2, "memory dropped the oldest");
    save_spend(&store, &full, &over).await.unwrap();

    assert_eq!(count(&store, "spend_audit").await, MAX_AUDIT_RECORDS as i64);
    let loaded = load_spend(&store).await.unwrap().unwrap();
    assert_eq!(loaded, over);
    assert_eq!(loaded.audit.first().unwrap().id, 2);
    assert_eq!(
        loaded.audit.last().unwrap().id,
        MAX_AUDIT_RECORDS as u64 + 1
    );
});

store_contract_test!(trim_counts_opaque_audit_records_too, |store| {
    let mut state = fresh();
    state.opaque_audit = vec![OpaqueRecord {
        typed_before: 0,
        value: serde_json::json!({"id":0}),
    }];
    state = upsert(state, Period::Monthly, "0", 0);
    state = step(&store, &fresh(), state).await;
    let mut full = state.clone();
    for n in 1..MAX_AUDIT_RECORDS - 1 {
        full = upsert(full, Period::Monthly, &n.to_string(), n);
    }
    assert_eq!(
        full.audit.len() + full.opaque_audit.len(),
        MAX_AUDIT_RECORDS
    );
    let full = step(&store, &state, full).await;

    let over = upsert(full.clone(), Period::Monthly, "999999", 7);
    assert!(over.opaque_audit.is_empty(), "the opaque record was oldest");
    save_spend(&store, &full, &over).await.unwrap();
    assert_eq!(load_spend(&store).await.unwrap().unwrap(), over);
    assert_eq!(count(&store, "spend_audit").await, MAX_AUDIT_RECORDS as i64);
});

store_contract_test!(a_no_op_save_issues_no_write, |store| {
    let state = upsert(fresh(), Period::Monthly, "1", 1);
    fail_writes(&store).await;
    // Any statement would hit the trigger; an unchanged state sends none.
    save_spend(&store, &state, &state.clone()).await.unwrap();
    assert_eq!(load_spend(&store).await.unwrap(), None);
    assert!(save_spend(&store, &fresh(), &state).await.is_err());
    allow_writes(&store).await;
});

store_contract_test!(a_failed_save_leaves_the_stored_state_unchanged, |store| {
    let first = upsert(fresh(), Period::Monthly, "1", 1);
    let first = step(&store, &fresh(), first).await;
    let second = upsert(first.clone(), Period::Daily, "2", 2);

    fail_writes(&store).await;
    let error = save_spend(&store, &first, &second).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
    allow_writes(&store).await;

    assert_eq!(load_spend(&store).await.unwrap().unwrap(), first);
    save_spend(&store, &first, &second).await.unwrap();
    assert_eq!(load_spend(&store).await.unwrap().unwrap(), second);
});

store_contract_test!(a_lost_hold_writes_nothing, |store| {
    let first = upsert(fresh(), Period::Monthly, "1", 1);
    let first = step(&store, &fresh(), first).await;
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "UPDATE instance_hold SET holder_id = $1 WHERE id = 1",
        &["ghost".into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let second = upsert(first.clone(), Period::Daily, "2", 2);
    let error = save_spend(&store, &first, &second).await.unwrap_err();
    assert!(matches!(error, StoreError::HoldLost { .. }), "{error:?}");
    assert_eq!(load_spend(&store).await.unwrap().unwrap(), first);
});

store_contract_test!(an_undecodable_row_fails_loudly, |store| {
    let state = step(&store, &fresh(), upsert(fresh(), Period::Monthly, "1", 1)).await;
    drop(state);
    run(
        &store,
        &["UPDATE spend_limit SET body = 'not json' WHERE opaque = 0".to_string()],
    )
    .await;
    let error = load_spend(&store).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
});

store_contract_test!(a_typed_row_with_an_unknown_field_loads_opaque, |store| {
    let saved = step(&store, &fresh(), upsert(fresh(), Period::Monthly, "1", 1)).await;
    assert_eq!((saved.limits.len(), saved.audit.len()), (1, 1));
    // What a newer binary would have written: a typed row with a field this
    // binary's model does not have.
    for table in ["spend_limit", "spend_audit"] {
        let mut tx = store.db().begin().await.unwrap();
        let row = tx
            .fetch_optional(
                &format!("SELECT ord, body FROM {table} WHERE opaque = 0"),
                &[],
            )
            .await
            .unwrap()
            .unwrap();
        let mut body: serde_json::Value = serde_json::from_str(&row.text(1).unwrap()).unwrap();
        body["future_field"] = serde_json::json!(true);
        tx.execute(
            &format!("UPDATE {table} SET body = $1 WHERE ord = $2"),
            &[body.to_string().as_str().into(), row.int(0).unwrap().into()],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let loaded = load_spend(&store).await.unwrap().unwrap();
    assert!(
        loaded.limits.is_empty() && loaded.audit.is_empty(),
        "{loaded:?}"
    );
    assert_eq!(loaded.opaque_limits[0].value["future_field"], true);
    assert_eq!(loaded.opaque_audit[0].value["future_field"], true);

    // The next mutation keeps both records with the field.
    let next = upsert(loaded.clone(), Period::Daily, "2", 2);
    save_spend(&store, &loaded, &next).await.unwrap();
    let reloaded = load_spend(&store).await.unwrap().unwrap();
    assert_eq!(reloaded.opaque_limits, loaded.opaque_limits);
    assert_eq!(reloaded.opaque_audit, loaded.opaque_audit);
});

store_contract_test!(
    a_settled_load_waits_for_the_durable_save_in_flight,
    |store| {
        // A durable save that has not committed yet holds the instance-hold row
        // (and, on SQLite, the only connection).
        let after = upsert(fresh(), Period::Daily, "1", 1);
        let mut tx = store.begin_durable().await.unwrap();
        write_state(&mut tx, &fresh(), &after).await.unwrap();

        let reader = tokio::spawn({
            let store = store.clone();
            async move { load_spend_settled(&store).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !reader.is_finished(),
            "a settled load must wait for the save in flight, not read the state before it"
        );

        tx.commit().await.unwrap();
        assert_eq!(reader.await.unwrap().unwrap(), Some(after));
    }
);
