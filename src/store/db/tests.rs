use super::*;
use crate::store::error::to_i64;
use crate::store::test_support::{sqlite_config, TempDir};
use crate::store::{Mode, Store};

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
