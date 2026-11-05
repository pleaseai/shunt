//! `pool_session_repo` against SQLite and PostgreSQL through the contract
//! harness, plus the pure diff.

use super::{apply_diff, diff, load_bindings, read_snapshot, snapshot_of, Snapshot};
use crate::accounts::{AccountKey, AccountStateIdentity, BindingRow, StoreFamily};
use crate::store::contract::store_contract_test;
use crate::store::{Store, Value};

fn key(name: &str) -> AccountKey {
    AccountKey {
        store_family: StoreFamily::Claude,
        identity: AccountStateIdentity::StoreEntry {
            name: name.to_string(),
        },
    }
}

fn row(session: char, account: &str, last_used: u64) -> BindingRow {
    BindingRow {
        provider: "anthropic".to_string(),
        session_hash: session.to_string().repeat(64),
        account: key(account),
        last_used,
    }
}

async fn write(store: &Store, previous: &Snapshot, rows: &[BindingRow]) -> Snapshot {
    let current = snapshot_of(rows).unwrap();
    let mut tx = store.begin_durable().await.unwrap();
    apply_diff(&mut tx, &diff(previous, &current))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    current
}

async fn stored(store: &Store) -> Vec<BindingRow> {
    let mut rows = load_bindings(store).await.unwrap().0;
    rows.sort_by(|a, b| a.session_hash.cmp(&b.session_hash));
    rows
}

#[test]
fn a_diff_writes_only_changed_rows_and_deletes_absent_ones() {
    let before =
        snapshot_of(&[row('a', "x", 3_600), row('b', "x", 3_600), row('c', "y", 0)]).unwrap();
    let after = snapshot_of(&[
        row('a', "x", 3_600), // unchanged
        row('b', "y", 3_600), // moved
        row('d', "y", 7_200), // new
    ])
    .unwrap();
    let diff = diff(&before, &after);
    let upserted: Vec<_> = diff
        .upserts
        .iter()
        .map(|(k, _)| k.1.chars().next())
        .collect();
    assert_eq!(upserted, [Some('b'), Some('d')], "a is unchanged");
    let deleted: Vec<_> = diff.deletes.iter().map(|k| k.1.chars().next()).collect();
    assert_eq!(deleted, [Some('c')]);
    assert!(
        super::diff(&after, &after).is_empty(),
        "no change, no write"
    );
}

store_contract_test!(rows_round_trip_and_deltas_apply, |store| {
    assert!(stored(&store).await.is_empty());
    let first = vec![row('a', "x", 3_600), row('b', "x", 3_600)];
    let snapshot = write(&store, &Snapshot::new(), &first).await;
    assert_eq!(stored(&store).await, first);

    let second = vec![row('b', "y", 7_200), row('c', "x", 7_200)];
    write(&store, &snapshot, &second).await;
    assert_eq!(stored(&store).await, second, "a deleted, b moved, c added");

    let mut tx = store.db().begin().await.unwrap();
    assert_eq!(
        read_snapshot(&mut tx).await.unwrap(),
        snapshot_of(&second).unwrap()
    );
    tx.rollback().await.unwrap();
});

store_contract_test!(an_undecodable_row_is_dropped_not_fatal, |store| {
    write(&store, &Snapshot::new(), &[row('a', "x", 3_600)]).await;
    let mut tx = store.db().begin().await.unwrap();
    for (hash, account, last_used) in [
        ("b".repeat(64), "not json".to_string(), 3_600_i64),
        (
            "c".repeat(64),
            serde_json::to_string(&key("x")).unwrap(),
            -1_i64,
        ),
    ] {
        tx.execute(
            "INSERT INTO pool_session_binding (provider, session_hash, account_key, last_used) \
             VALUES ($1, $2, $3, $4)",
            &[
                Value::from("anthropic"),
                Value::from(hash.as_str()),
                Value::from(account.as_str()),
                Value::from(last_used),
            ],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    assert_eq!(stored(&store).await, [row('a', "x", 3_600)]);
    // A flush from the table's own snapshot deletes the undecodable rows.
    let mut tx = store.db().begin().await.unwrap();
    let on_disk = read_snapshot(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(on_disk.len(), 3);
    write(&store, &on_disk, &[row('a', "x", 3_600)]).await;
    let mut tx = store.db().begin().await.unwrap();
    assert_eq!(read_snapshot(&mut tx).await.unwrap().len(), 1);
    tx.rollback().await.unwrap();
});

store_contract_test!(load_bindings_reports_whether_a_row_was_dropped, |store| {
    async fn insert(store: &Store, hash: String, account: String, last_used: i64) {
        let mut tx = store.db().begin().await.unwrap();
        tx.execute(
            "INSERT INTO pool_session_binding (provider, session_hash, account_key, last_used) \
             VALUES ($1, $2, $3, $4)",
            &[
                Value::from("anthropic"),
                Value::from(hash.as_str()),
                Value::from(account.as_str()),
                Value::from(last_used),
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    async fn clear(store: &Store) {
        let mut tx = store.db().begin().await.unwrap();
        tx.execute("DELETE FROM pool_session_binding", &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    write(&store, &Snapshot::new(), &[row('a', "x", 3_600)]).await;
    let (rows, dropped) = load_bindings(&store).await.unwrap();
    assert_eq!(rows, [row('a', "x", 3_600)]);
    assert!(!dropped, "only decodable rows: nothing dropped");

    clear(&store).await;
    insert(
        &store,
        "c".repeat(64),
        serde_json::to_string(&key("x")).unwrap(),
        -1,
    )
    .await;
    let (rows, dropped) = load_bindings(&store).await.unwrap();
    assert!(rows.is_empty(), "the negative last_used row is absent");
    assert!(dropped, "a negative last_used row is a drop");

    clear(&store).await;
    insert(&store, "b".repeat(64), "not json".to_string(), 3_600).await;
    let (rows, dropped) = load_bindings(&store).await.unwrap();
    assert!(rows.is_empty(), "the undecodable key row is absent");
    assert!(dropped, "an account key that is not JSON is a drop");
});

/// Migration `0002` is dollar-free on both engines and declares the same
/// statements: only the header comment naming the sibling file differs.
#[test]
fn migration_0002_is_dollar_free_and_identical_across_engines() {
    let sqlite = include_str!("../../../migrations/sqlite/0002_pool_session_binding.sql");
    let postgres = include_str!("../../../migrations/postgres/0002_pool_session_binding.sql");
    for migration in [sqlite, postgres] {
        assert!(!migration.contains('$'));
        assert!(migration.contains("CREATE TABLE pool_session_binding"));
    }
    let statements = |sql: &str| -> Vec<String> {
        sql.lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(statements(sqlite), statements(postgres));
}

store_contract_test!(migration_0002_creates_the_binding_table, |store| {
    let mut tx = store.db().begin().await.unwrap();
    let rows = tx
        .fetch_all(
            "SELECT provider, session_hash, account_key, last_used FROM pool_session_binding",
            &[],
        )
        .await
        .unwrap();
    assert!(rows.is_empty());
    tx.rollback().await.unwrap();
});
