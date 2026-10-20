use super::*;
use crate::store::error::to_i64;
use crate::store::test_support::{sqlite_config, TempDir};
use crate::store::{Mode, Store};

/// A bound [`super::set_io_timeout`] or [`super::set_round_trip_timeout`] put
/// in place: the timeout, and the one kind of round trip it covers (`None`: all).
#[cfg(feature = "store-postgres")]
type IoTimeoutOverride = Option<(std::time::Duration, Option<&'static str>)>;

#[cfg(feature = "store-postgres")]
thread_local! {
    static IO_TIMEOUT: std::cell::Cell<IoTimeoutOverride> = const { std::cell::Cell::new(None) };
}

#[cfg(feature = "store-postgres")]
pub(super) fn io_timeout_override(what: &str) -> Option<std::time::Duration> {
    match IO_TIMEOUT.with(std::cell::Cell::get) {
        Some((timeout, only)) if only.is_none_or(|only| only == what) => Some(timeout),
        _ => None,
    }
}

/// Restores the bound in place before it was installed, so a panicking test
/// does not leave its bound on the thread.
#[cfg(feature = "store-postgres")]
#[must_use = "the bound is restored when the guard drops"]
pub(crate) struct IoTimeoutGuard {
    previous: IoTimeoutOverride,
}

#[cfg(feature = "store-postgres")]
impl IoTimeoutGuard {
    pub(super) fn install(timeout: std::time::Duration, only: Option<&'static str>) -> Self {
        Self {
            previous: IO_TIMEOUT.with(|cell| cell.replace(Some((timeout, only)))),
        }
    }
}

#[cfg(feature = "store-postgres")]
impl Drop for IoTimeoutGuard {
    fn drop(&mut self) {
        IO_TIMEOUT.with(|cell| cell.set(self.previous));
    }
}

#[test]
fn placeholders_map_dollar_n_to_question_n() {
    assert_eq!(
        rewrite_placeholders("SELECT a FROM t WHERE x = $1 AND y = $2 AND z = $10"),
        "SELECT a FROM t WHERE x = ?1 AND y = ?2 AND z = ?10"
    );
    assert!(matches!(
        rewrite_placeholders("SELECT 1"),
        Cow::Borrowed("SELECT 1")
    ));
}

#[test]
fn placeholders_ignore_dollar_in_string_literals() {
    let sql = "SELECT '$1', 'it''s $2' FROM t WHERE a = $1";
    assert_eq!(
        rewrite_placeholders(sql),
        "SELECT '$1', 'it''s $2' FROM t WHERE a = ?1"
    );
}

#[test]
fn placeholders_ignore_dollar_in_quoted_identifiers() {
    let sql = r#"SELECT "col$1", "it's $2" FROM t WHERE a = $1 AND b = '"$3'"#;
    assert_eq!(
        rewrite_placeholders(sql),
        r#"SELECT "col$1", "it's $2" FROM t WHERE a = ?1 AND b = '"$3'"#
    );
}

#[test]
fn placeholders_leave_a_bare_dollar_alone() {
    assert_eq!(
        rewrite_placeholders("SELECT $a, $ , $1"),
        "SELECT $a, $ , ?1"
    );
}

#[test]
fn no_rewritten_statement_or_migration_keeps_a_dollar() {
    let statements = [
        "INSERT INTO pool_quota (account_key, quota_json, updated_at) VALUES ($1, $2, $3) \
         ON CONFLICT (account_key) DO UPDATE SET quota_json = $2, updated_at = $3",
        "UPDATE instance_hold SET holder_id = $1 WHERE id = 1 AND (holder_id IS NULL OR heartbeat_at < $2)",
        "DELETE FROM spend_audit WHERE ord < $1",
    ];
    for sql in statements {
        let rewritten = rewrite_placeholders(sql);
        assert!(!rewritten.contains('$'), "left a `$` in {rewritten}");
    }
    for migration in [
        include_str!("../../../migrations/sqlite/0001_init.sql"),
        include_str!("../../../migrations/postgres/0001_init.sql"),
    ] {
        assert!(!migration.contains('$'));
    }
}

#[test]
fn u64_conversion_errors_instead_of_wrapping() {
    assert_eq!(to_i64(7).unwrap(), 7);
    assert_eq!(to_i64(i64::MAX as u64).unwrap(), i64::MAX);
    assert!(matches!(
        to_i64(u64::MAX),
        Err(StoreError::OutOfRange { value: u64::MAX })
    ));
}

#[tokio::test]
async fn transaction_round_trips_every_value_kind_and_rolls_back() {
    let dir = TempDir::new("tx");
    let store = Store::open(&sqlite_config(&dir.file("s.db")), Mode::Serve)
        .await
        .unwrap();
    let db = store.db();

    let mut tx = db.begin().await.unwrap();
    let inserted = tx
        .execute(
            "INSERT INTO refresh_token (token_sha256, family, identity_json, inactive_since, issued_at) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                "h1".into(),
                "fam".into(),
                "{}".into(),
                Value::OptInt(None),
                5_i64.into(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1);
    tx.commit().await.unwrap();

    let mut tx = db.begin().await.unwrap();
    let rows = tx
        .fetch_all(
            "SELECT token_sha256, inactive_since, issued_at FROM refresh_token WHERE family = $1",
            &["fam".into()],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text(0).unwrap(), "h1");
    assert_eq!(rows[0].opt_int(1).unwrap(), None);
    assert_eq!(rows[0].int(2).unwrap(), 5);
    tx.execute("DELETE FROM refresh_token", &[]).await.unwrap();
    tx.rollback().await.unwrap();

    let mut tx = db.begin().await.unwrap();
    let kept = tx
        .fetch_optional(
            "SELECT family FROM refresh_token WHERE token_sha256 = $1",
            &["h1".into()],
        )
        .await
        .unwrap()
        .expect("rollback keeps the row");
    assert_eq!(kept.opt_text(0).unwrap().as_deref(), Some("fam"));
    tx.rollback().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn a_fresh_database_has_the_seeded_hold_row() {
    let dir = TempDir::new("seed");
    let store = Store::open(&sqlite_config(&dir.file("s.db")), Mode::Serve)
        .await
        .unwrap();
    let mut tx = store.db().begin().await.unwrap();
    let row = tx
        .fetch_optional("SELECT holder_id FROM instance_hold WHERE id = 1", &[])
        .await
        .unwrap()
        .expect("seed row");
    // `Mode::Serve` acquires the seeded row before returning.
    assert_eq!(row.opt_text(0).unwrap().as_deref(), Some(store.holder_id()));
    tx.rollback().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn fetch_optional_returns_the_first_of_several_rows_or_none() {
    let dir = TempDir::new("optional");
    let store = Store::open(&sqlite_config(&dir.file("s.db")), Mode::Serve)
        .await
        .unwrap();
    let mut tx = store.db().begin().await.unwrap();
    let first = tx
        .fetch_optional(
            "SELECT v FROM (SELECT 9 AS v UNION ALL SELECT 7 UNION ALL SELECT 8) ORDER BY v",
            &[],
        )
        .await
        .unwrap()
        .expect("first row");
    assert_eq!(first.int(0).unwrap(), 7);
    let none = tx
        .fetch_optional(
            "SELECT holder_id FROM instance_hold WHERE id = $1",
            &[2_i64.into()],
        )
        .await
        .unwrap();
    assert!(none.is_none());
    tx.rollback().await.unwrap();
    store.close().await;
}

crate::store::contract::store_contract_test!(
    a_postgres_round_trip_that_never_answers_gives_up,
    |store| {
        // SQLite is a local file: there is no connection to go half-open.
        if !matches!(store.db(), Db::Sqlite(_)) {
            #[cfg(feature = "store-postgres")]
            {
                use std::time::Duration;
                let _bound = super::set_io_timeout(Duration::from_millis(300));
                let mut tx = store.db().begin().await.unwrap();
                // A statement that has not answered yet stands in for one whose
                // reply a half-open connection will never deliver.
                let outcome = tokio::time::timeout(
                    Duration::from_secs(5),
                    tx.fetch_optional("SELECT pg_sleep(2)", &[]),
                )
                .await
                .expect("a postgres round trip must give up at its bound");
                match outcome {
                    Err(StoreError::Database { reason }) => {
                        assert!(reason.contains("did not complete"), "{reason}");
                    }
                    Err(other) => panic!("expected the round trip to time out, got {other:?}"),
                    Ok(_) => panic!("the round trip must not answer within its bound"),
                }
            }
        }
    }
);

#[cfg(feature = "store-postgres")]
crate::store::contract::store_contract_test!(
    a_timed_out_postgres_connection_is_closed_not_returned_to_the_pool,
    |store| {
        // SQLite is a local file: there is no connection to go half-open.
        if let Db::Postgres(pool) = store.db() {
            use std::time::Duration;
            let _bound = super::set_io_timeout(Duration::from_millis(300));
            let mut tx = store.db().begin().await.unwrap();
            let before = pool.size();
            let outcome = tx.fetch_optional("SELECT pg_sleep(3)", &[]).await;
            assert!(outcome.is_err());
            // The abandoned transaction neither waits out another bound nor
            // commits, and its connection leaves the pool with it.
            let started = std::time::Instant::now();
            match tx.fetch_optional("SELECT 1", &[]).await {
                Err(StoreError::Database { reason }) => {
                    assert!(reason.contains("skipped"), "{reason}")
                }
                Err(other) => panic!("a statement after a timeout must be skipped, got {other:?}"),
                Ok(_) => panic!("a statement after a timeout must be skipped"),
            }
            tx.rollback().await.unwrap();
            assert!(started.elapsed() < Duration::from_millis(200));
            assert_eq!(
                pool.size(),
                before - 1,
                "the timed-out connection must be closed"
            );
        }
    }
);

#[cfg(feature = "store-postgres")]
crate::store::contract::store_contract_test!(
    a_timed_out_connection_is_closed_before_the_server_answers,
    |store| {
        if let Db::Postgres(pool) = store.db() {
            use std::time::Duration;
            let _bound = super::set_io_timeout(Duration::from_millis(100));
            let mut tx = store.db().begin().await.unwrap();
            let before = pool.size();
            let outcome = tx.fetch_optional("SELECT pg_sleep(0.5)", &[]).await;
            assert!(outcome.is_err());
            tx.rollback().await.unwrap();
            // The server would answer well within the default bound, yet the
            // connection must not wait for it and come back: a cancelled
            // request may have left the server inside a transaction sqlx does
            // not know about. Closing it is immediate, not a release-time drain.
            assert_eq!(
                pool.size(),
                before - 1,
                "a timed-out connection must be closed"
            );
        }
    }
);

#[cfg(feature = "store-postgres")]
crate::store::contract::store_contract_test!(
    an_unfinished_transaction_closes_its_connection,
    |store| {
        if let Db::Postgres(pool) = store.db() {
            // A BEGIN or statement that gave up, or a `Tx` dropped without
            // COMMIT or ROLLBACK, may leave the server inside a transaction no
            // client will finish. All of them drop a `PgTx` that never
            // finished, which must close its connection rather than return it.
            let tx = store.db().begin().await.unwrap();
            let before = pool.size();
            drop(tx);
            assert_eq!(pool.size(), before - 1, "the connection must be closed");
            store.db().begin().await.unwrap().rollback().await.unwrap();
        }
    }
);

#[cfg(feature = "store-postgres")]
crate::store::contract::store_contract_test!(a_commit_past_its_bound_is_uncertain, |store| {
    if let Db::Postgres(_) = store.db() {
        use std::time::Duration;
        let mut tx = store.db().begin().await.unwrap();
        // A deferred trigger makes COMMIT itself take a second, the way a
        // COMMIT whose reply is lost looks to the client.
        for sql in [
            "CREATE TABLE slow_commit_probe (x BIGINT)",
            "CREATE FUNCTION slow_commit_probe_fn() RETURNS trigger AS \
             $body$ BEGIN PERFORM pg_sleep(1); RETURN NULL; END $body$ LANGUAGE plpgsql",
            "CREATE CONSTRAINT TRIGGER slow_commit_probe AFTER INSERT ON slow_commit_probe \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION slow_commit_probe_fn()",
            "INSERT INTO slow_commit_probe (x) VALUES (1)",
        ] {
            tx.execute(sql, &[]).await.unwrap();
        }
        let _bound = super::set_round_trip_timeout("COMMIT", Duration::from_millis(300));
        match tx.commit().await {
            Err(StoreError::CommitUncertain { reason }) => {
                assert!(reason.contains("did not complete"), "{reason}");
            }
            other => panic!("a COMMIT past its bound must be uncertain, got {other:?}"),
        }
    }
});
