//! Smoke contract tests proving the harness, and the schema parity check.

use std::collections::BTreeSet;

use crate::store::{Store, Value};

const CREATE: &str = "CREATE TABLE contract_kv (\
    k TEXT PRIMARY KEY, n BIGINT NOT NULL, o_n BIGINT NULL, o_t TEXT NULL)";

async fn scratch_table(store: &Store) {
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(CREATE, &[]).await.unwrap();
    tx.commit().await.unwrap();
}

store_contract_test!(value_kinds_round_trip, |store| {
    scratch_table(&store).await;
    let mut tx = store.db().begin().await.unwrap();
    let insert = "INSERT INTO contract_kv (k, n, o_n, o_t) VALUES ($1, $2, $3, $4)";
    let some = [
        Value::from("a"),
        Value::from(7_i64),
        Value::from(Some(9_i64)),
        Value::from(Some("t".to_string())),
    ];
    assert_eq!(tx.execute(insert, &some).await.unwrap(), 1);
    let none = [
        Value::from("b"),
        Value::from(8_i64),
        Value::OptInt(None),
        Value::OptText(None),
    ];
    assert_eq!(tx.execute(insert, &none).await.unwrap(), 1);
    let rows = tx
        .fetch_all("SELECT k, n, o_n, o_t FROM contract_kv ORDER BY k", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].text(0).unwrap(), "a");
    assert_eq!(rows[0].int(1).unwrap(), 7);
    assert_eq!(rows[0].opt_int(2).unwrap(), Some(9));
    assert_eq!(rows[0].opt_text(3).unwrap().as_deref(), Some("t"));
    assert_eq!(rows[1].opt_int(2).unwrap(), None);
    assert_eq!(rows[1].opt_text(3).unwrap(), None);
    let missing = tx
        .fetch_optional(
            "SELECT k FROM contract_kv WHERE k = $1",
            &[Value::from("zz")],
        )
        .await
        .unwrap();
    assert!(missing.is_none());
    tx.commit().await.unwrap();
});

store_contract_test!(upsert_updates_in_place, |store| {
    scratch_table(&store).await;
    let upsert = "INSERT INTO contract_kv (k, n) VALUES ($1, $2) \
        ON CONFLICT (k) DO UPDATE SET n = excluded.n";
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(upsert, &[Value::from("k"), Value::from(1_i64)])
        .await
        .unwrap();
    tx.execute(upsert, &[Value::from("k"), Value::from(2_i64)])
        .await
        .unwrap();
    let rows = tx
        .fetch_all("SELECT n FROM contract_kv", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].int(0).unwrap(), 2);
    tx.commit().await.unwrap();
});

store_contract_test!(i64_bounds_and_rollback, |store| {
    scratch_table(&store).await;
    let mut tx = store.db().begin().await.unwrap();
    for (k, n) in [("min", i64::MIN), ("max", i64::MAX)] {
        tx.execute(
            "INSERT INTO contract_kv (k, n) VALUES ($1, $2)",
            &[Value::from(k), Value::from(n)],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let mut tx = store.db().begin().await.unwrap();
    tx.execute("DELETE FROM contract_kv", &[]).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = store.db().begin().await.unwrap();
    let rows = tx
        .fetch_all("SELECT n FROM contract_kv ORDER BY n", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].int(0).unwrap(), i64::MIN);
    assert_eq!(rows[1].int(0).unwrap(), i64::MAX);
    tx.rollback().await.unwrap();
});

store_contract_test!(migrations_apply_to_a_fresh_schema, |store| {
    assert!(store.schema_version().await.unwrap() >= 1);
    assert_eq!(store.pending_migrations().await.unwrap(), 0);
});

const SQLITE_COLUMNS: &str = "SELECT m.name, p.name FROM sqlite_master m, \
    pragma_table_info(m.name) p WHERE m.type = 'table' \
    AND m.name NOT LIKE 'sqlite_%' AND m.name <> '_sqlx_migrations'";
#[cfg(feature = "store-postgres")]
const POSTGRES_COLUMNS: &str = "SELECT table_name, column_name FROM information_schema.columns \
    WHERE table_schema = current_schema() AND table_name <> '_sqlx_migrations'";

async fn columns(store: &Store, sql: &str) -> BTreeSet<(String, String)> {
    let mut tx = store.db().begin().await.unwrap();
    let rows = tx.fetch_all(sql, &[]).await.unwrap();
    let set = rows
        .iter()
        .map(|row| (row.text(0).unwrap(), row.text(1).unwrap()))
        .collect();
    tx.rollback().await.unwrap();
    set
}

#[tokio::test]
async fn sqlite_schema_is_not_empty() {
    let harness = super::Harness::sqlite().await;
    let sqlite = columns(&harness.store, SQLITE_COLUMNS).await;
    assert!(sqlite.contains(&("pool_quota".to_string(), "account_key".to_string())));
    harness.finish().await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
async fn both_engines_migrate_to_the_same_tables_and_columns() {
    let Some(postgres) = super::postgres::harness().await else {
        return;
    };
    let sqlite = super::Harness::sqlite().await;
    let from_sqlite = columns(&sqlite.store, SQLITE_COLUMNS).await;
    let from_postgres = columns(&postgres.store, POSTGRES_COLUMNS).await;
    assert!(!from_sqlite.is_empty());
    assert_eq!(
        from_sqlite
            .symmetric_difference(&from_postgres)
            .collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "sqlite and postgres schemas differ"
    );
    sqlite.finish().await;
    postgres.finish().await;
}
