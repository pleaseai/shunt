//! `pool_repo` against SQLite and PostgreSQL through the contract harness.

use std::collections::HashMap;

use super::{load_pool, save_pool};
use crate::accounts::{AccountKey, AccountStateIdentity, QuotaState, StoreFamily};
use crate::store::contract::store_contract_test;
use crate::store::{Store, StoreError};

fn key(name: &str) -> AccountKey {
    AccountKey {
        store_family: StoreFamily::Claude,
        identity: AccountStateIdentity::StoreEntry {
            name: name.to_string(),
        },
    }
}

fn quota(utilization: f64) -> QuotaState {
    QuotaState {
        utilization_5h: Some(utilization),
        reset_5h: Some(4_000_000_000),
        status: Some("allowed".to_string()),
        observed_at_5h: Some(1_700_000_000),
        ..Default::default()
    }
}

async fn stored(store: &Store) -> HashMap<AccountKey, QuotaState> {
    load_pool(store).await.unwrap().into_iter().collect()
}

store_contract_test!(save_then_load_round_trips, |store| {
    assert!(load_pool(&store).await.unwrap().is_empty());
    let exported = vec![(key("a"), quota(0.25)), (key("b"), quota(0.5))];
    save_pool(&store, &exported).await.unwrap();
    assert_eq!(stored(&store).await, exported.into_iter().collect());
});

store_contract_test!(save_upserts_changes_and_deletes_absent_keys, |store| {
    save_pool(&store, &[(key("a"), quota(0.1)), (key("b"), quota(0.2))])
        .await
        .unwrap();
    save_pool(&store, &[(key("a"), quota(0.9)), (key("c"), quota(0.3))])
        .await
        .unwrap();
    let want: HashMap<_, _> = [(key("a"), quota(0.9)), (key("c"), quota(0.3))].into();
    assert_eq!(stored(&store).await, want, "b deleted, a updated, c added");

    save_pool(&store, &[]).await.unwrap();
    assert!(
        stored(&store).await.is_empty(),
        "an empty export clears the table"
    );
});

store_contract_test!(a_lost_hold_writes_nothing, |store| {
    save_pool(&store, &[(key("a"), quota(0.1))]).await.unwrap();
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "UPDATE instance_hold SET holder_id = $1 WHERE id = 1",
        &["ghost".into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let error = save_pool(&store, &[(key("z"), quota(0.7))])
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::HoldLost { .. }), "{error:?}");
    let want: HashMap<_, _> = [(key("a"), quota(0.1))].into();
    assert_eq!(stored(&store).await, want);
});

store_contract_test!(an_undecodable_row_fails_loudly, |store| {
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "INSERT INTO pool_quota (account_key, quota_json, updated_at) VALUES ($1, $2, $3)",
        &["not json".into(), "{}".into(), 1_i64.into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let error = load_pool(&store).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
});
