//! Contract tests for the instance hold, run on SQLite and PostgreSQL.

use std::time::Duration;

use sqlx::migrate::Migrator;

use super::*;
use crate::store::contract::store_contract_test;
use crate::store::db::Db;
use crate::store::open::Mode;

/// Run one statement in its own committed transaction.
async fn exec(store: &Store, sql: &str, params: &[Value]) -> u64 {
    let mut tx = store.db().begin().await.unwrap();
    let n = tx.execute(sql, params).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// (holder_id, holder_label, heartbeat_at) as stored.
async fn row(store: &Store) -> (Option<String>, Option<String>, Option<i64>) {
    let mut tx = store.db().begin().await.unwrap();
    let r = tx.fetch_optional(READ_ROW, &[]).await.unwrap().unwrap();
    let out = (
        r.opt_text(0).unwrap(),
        r.opt_text(1).unwrap(),
        r.opt_int(2).unwrap(),
    );
    tx.rollback().await.unwrap();
    out
}

async fn steal(store: &Store) {
    exec(
        store,
        "UPDATE instance_hold SET holder_id = $1, holder_label = $2, heartbeat_at = $3 WHERE id = 1",
        &["thief".into(), "other-host:99".into(), now_secs().into()],
    )
    .await;
}

async fn age_heartbeat(store: &Store, seconds: i64) {
    exec(
        store,
        "UPDATE instance_hold SET heartbeat_at = $1 WHERE id = 1",
        &[(now_secs() - seconds).into()],
    )
    .await;
}

async fn migration_count(store: &Store) -> i64 {
    let mut tx = store.db().begin().await.unwrap();
    let r = tx
        .fetch_optional("SELECT COUNT(*) FROM _sqlx_migrations", &[])
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    r.int(0).unwrap()
}

/// A second opener over the same database, with its own pool.
async fn open_second(store: &Store, stale: u64) -> Result<Store, StoreError> {
    let mut config = store.config.clone();
    config.hold_stale_seconds = stale;
    Store::open(&config, Mode::Serve).await
}

/// A migrator holding 0001 plus an extra 0002 (a table that must not appear).
async fn migrator_with_extra(store: &Store) -> &'static Migrator {
    let engine = match store.db() {
        Db::Sqlite(_) => "sqlite",
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => "postgres",
    };
    let dir = crate::store::test_support::TempDir::new("extra-migration");
    let source = format!(
        "{}/migrations/{engine}/0001_init.sql",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::copy(source, dir.file("0001_init.sql")).unwrap();
    std::fs::write(
        dir.file("0002_probe.sql"),
        "CREATE TABLE hold_probe (x BIGINT);",
    )
    .unwrap();
    let migrator = Migrator::new(dir.0.as_path()).await.unwrap();
    Box::leak(Box::new(migrator))
}

store_contract_test!(open_acquires_and_ensure_is_idempotent, |store| {
    let (holder, label, beat) = row(&store).await;
    assert_eq!(holder.as_deref(), Some(store.holder_id()));
    assert_eq!(label.as_deref(), Some(store.holder_label()));
    assert!(beat.is_some());
    assert!(store.holder_label().contains(':'));
    store.ensure_hold_table().await.unwrap();
    store.ensure_hold_table().await.unwrap();
    // Re-acquiring as the same holder is a renewal, not a takeover.
    assert_eq!(store.acquire_hold().await.unwrap(), None);
    let tx = store.begin_durable().await.unwrap();
    tx.commit().await.unwrap();
});

store_contract_test!(live_holder_is_refused_with_label_and_age, |store| {
    age_heartbeat(&store, 2).await;
    let error = open_second(&store, 30).await.unwrap_err();
    match &error {
        StoreError::HoldHeld {
            label, age_secs, ..
        } => {
            assert_eq!(label, store.holder_label());
            assert!((2..30).contains(age_secs), "age {age_secs}");
        }
        other => panic!("expected HoldHeld, got {other:?}"),
    }
    let shown = error.to_string();
    assert!(shown.contains(store.holder_label()), "{shown}");
    assert!(shown.contains("last renewed"), "{shown}");
    // The refused opener changed nothing: we still hold it.
    assert_eq!(row(&store).await.0.as_deref(), Some(store.holder_id()));
    drop(store.begin_durable().await.unwrap());
});

store_contract_test!(refused_opener_leaves_migrations_unchanged, |store| {
    assert_eq!(migration_count(&store).await, 1);
    let migrator = migrator_with_extra(&store).await;
    let mut config = store.config.clone();
    config.hold_stale_seconds = 30;
    let error = Store::open_with(&config, Mode::Serve, migrator)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::HoldHeld { .. }), "{error:?}");
    assert_eq!(migration_count(&store).await, 1);
    assert_eq!(store.schema_version().await.unwrap(), 1);
});

store_contract_test!(stale_holder_is_taken_over_and_fenced_out, |store| {
    age_heartbeat(&store, 100).await;
    let second = open_second(&store, 3).await.unwrap();
    assert_eq!(row(&store).await.0.as_deref(), Some(second.holder_id()));
    // The old holder's next durable write is refused and writes nothing.
    let before = row(&store).await;
    assert!(matches!(
        store.begin_durable().await,
        Err(StoreError::HoldLost { .. })
    ));
    assert!(store.is_fenced());
    assert_eq!(row(&store).await, before);
    let tx = second.begin_durable().await.unwrap();
    tx.rollback().await.unwrap();
    second.close().await;
});

store_contract_test!(takeover_names_the_previous_holder, |store| {
    exec(
        &store,
        "UPDATE instance_hold SET holder_id = $1, holder_label = $2, heartbeat_at = $3 WHERE id = 1",
        &["ghost".into(), "old-host:7".into(), (now_secs() - 120).into()],
    )
    .await;
    let takeover = store.acquire_hold().await.unwrap().expect("took over");
    assert_eq!(takeover.label, "old-host:7");
    assert!((120..150).contains(&takeover.age_secs), "{takeover:?}");
    assert_eq!(row(&store).await.0.as_deref(), Some(store.holder_id()));
});

store_contract_test!(stolen_row_is_refused_before_the_next_heartbeat, |store| {
    steal(&store).await;
    assert!(!store.is_fenced(), "no heartbeat has run yet");
    let before = row(&store).await;
    assert!(matches!(
        store.begin_durable().await,
        Err(StoreError::HoldLost { .. })
    ));
    assert!(store.is_fenced());
    assert_eq!(
        row(&store).await,
        before,
        "the refused write changed nothing"
    );
    // Fenced: refused again without the database (even if it were ours again).
    exec(
        &store,
        "UPDATE instance_hold SET holder_id = $1 WHERE id = 1",
        &[store.holder_id().into()],
    )
    .await;
    assert!(matches!(
        store.begin_durable().await,
        Err(StoreError::HoldLost { .. })
    ));
});

store_contract_test!(heartbeat_renews_and_fences_when_stolen, |store| {
    age_heartbeat(&store, 50).await;
    assert!(store.heartbeat_once().await.unwrap());
    let beat = row(&store).await.2.unwrap();
    assert!(now_secs() - beat < 5);
    assert!(!store.is_fenced());

    steal(&store).await;
    assert!(!store.heartbeat_once().await.unwrap());
    assert!(store.is_fenced());
    assert!(matches!(
        store.begin_durable().await,
        Err(StoreError::HoldLost { .. })
    ));
});

store_contract_test!(heartbeat_task_runs_and_stops, |store| {
    // hold_stale_seconds 30 gives a 10 s period; re-open the store view with a
    // 3 s hold so the period is the 1 s floor.
    let fast = {
        let mut config = store.config.clone();
        config.hold_stale_seconds = 3;
        config
    };
    // Release ours so the 3 s opener can take the hold, then close ours.
    store.release_hold().await.unwrap();
    let fast = Store::open(&fast, Mode::Serve).await.unwrap();
    age_heartbeat(&fast, 2).await;
    let old = row(&fast).await.2.unwrap();
    let handle = fast.spawn_heartbeat();
    let mut renewed = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if row(&fast).await.2.unwrap() > old {
            renewed = true;
            break;
        }
    }
    handle.stop().await;
    assert!(renewed, "heartbeat task did not renew within 10 s");
    fast.close().await;
});

store_contract_test!(release_clears_the_row_and_allows_reacquire, |store| {
    store.release_hold().await.unwrap();
    assert_eq!(row(&store).await, (None, None, None));
    // Not stale, but released: a second opener takes it immediately.
    let second = open_second(&store, 30).await.unwrap();
    assert_eq!(row(&store).await.0.as_deref(), Some(second.holder_id()));
    // Releasing a hold we no longer own changes nothing.
    store.release_hold().await.unwrap();
    assert_eq!(row(&store).await.0.as_deref(), Some(second.holder_id()));
    assert!(matches!(
        store.begin_durable().await,
        Err(StoreError::HoldLost { .. })
    ));
    second.close().await;
});

store_contract_test!(
    a_settled_begin_gives_up_on_a_write_that_never_ends,
    |store| {
        // SQLite has one connection, so a second transaction waits for the pool,
        // not the row lock: only PostgreSQL reaches the lock timeout.
        if !matches!(store.db(), Db::Sqlite(_)) {
            store.set_settled_lock_timeout(Duration::from_millis(200));
            // A durable transaction that never ends, like a backend left behind by
            // a connection lost mid-commit, holds the hold row.
            let stuck = store.begin_durable().await.unwrap();

            let outcome = tokio::time::timeout(Duration::from_secs(5), store.begin_settled())
                .await
                .expect("a settled begin must give up on a held hold row, not wait for it");
            match outcome {
                Err(StoreError::Database { reason }) => {
                    assert!(reason.contains("lock timeout"), "{reason}");
                }
                Err(other) => panic!("expected a lock timeout, got {other:?}"),
                Ok(_) => panic!("a settled begin must not get the hold row while it is held"),
            }
            assert!(!store.is_fenced(), "a lock timeout is not a lost hold");

            stuck.rollback().await.unwrap();
            store
                .begin_settled()
                .await
                .unwrap()
                .rollback()
                .await
                .unwrap();
        }
    }
);

#[tokio::test]
async fn inspect_never_touches_the_hold() {
    use crate::store::test_support::{sqlite_config, TempDir};
    let dir = TempDir::new("hold-inspect");
    let config = sqlite_config(&dir.file("s.db"));
    let store = Store::open(&config, Mode::Serve).await.unwrap();
    let before = row(&store).await;
    let inspect = Store::open(&config, Mode::Inspect).await.unwrap();
    inspect.close().await;
    assert_eq!(row(&store).await, before);
    store.close().await;
}
