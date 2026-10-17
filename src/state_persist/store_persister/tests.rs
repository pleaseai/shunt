//! The store persister on SQLite and PostgreSQL. Failures are injected inside
//! the engine (a trigger that aborts writes to `pool_quota`), then removed.

use std::{collections::HashMap, sync::Arc, time::Duration};

use super::{restore_from_store, PoolPersister};
use crate::{
    accounts::{AccountKey, AccountStateIdentity, QuotaState, StoreFamily},
    config::Config,
    metrics::store_write_failures_for_tests,
    server::AppState,
    store::{contract::store_contract_test, pool_repo, Db, Store, StoreError},
};

fn key(name: &str) -> AccountKey {
    AccountKey {
        store_family: StoreFamily::Claude,
        identity: AccountStateIdentity::StoreEntry {
            name: name.to_string(),
        },
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn quota(utilization: f64) -> QuotaState {
    let now = unix_now();
    QuotaState {
        utilization_5h: Some(utilization),
        reset_5h: Some(4_000_000_000),
        status: Some("allowed".to_string()),
        observed_at_5h: Some(now),
        observed_at_status: Some(now),
        ..Default::default()
    }
}

fn app_state() -> AppState {
    AppState::new(Config::default(), reqwest::Client::new()).expect("valid default config")
}

fn mutate(state: &AppState, entries: &[(&str, f64)]) {
    state
        .accounts
        .import_quotas(entries.iter().map(|(name, u)| (key(name), quota(*u))));
    state.accounts.mark_dirty();
}

async fn run(store: &Store, statements: &[&str]) {
    let mut tx = store.db().begin().await.unwrap();
    for statement in statements {
        tx.execute(statement, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
}

/// Make every insert/update on `pool_quota` abort inside the engine.
async fn fail_writes(store: &Store) {
    match store.db() {
        Db::Sqlite(_) => {
            run(
                store,
                &[
                    "CREATE TRIGGER fail_pool BEFORE INSERT ON pool_quota \
                     BEGIN SELECT RAISE(ABORT, 'injected'); END",
                    "CREATE TRIGGER fail_pool_update BEFORE UPDATE ON pool_quota \
                     BEGIN SELECT RAISE(ABORT, 'injected'); END",
                ],
            )
            .await;
        }
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => {
            run(
                store,
                &[
                    "CREATE OR REPLACE FUNCTION fail_pool_fn() RETURNS trigger AS \
                     $body$ BEGIN RAISE EXCEPTION 'injected'; END $body$ LANGUAGE plpgsql",
                    "CREATE TRIGGER fail_pool BEFORE INSERT OR UPDATE ON pool_quota \
                     FOR EACH ROW EXECUTE FUNCTION fail_pool_fn()",
                ],
            )
            .await;
        }
    }
}

async fn allow_writes(store: &Store) {
    match store.db() {
        Db::Sqlite(_) => {
            run(
                store,
                &["DROP TRIGGER fail_pool", "DROP TRIGGER fail_pool_update"],
            )
            .await;
        }
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => {
            run(store, &["DROP TRIGGER fail_pool ON pool_quota"]).await;
        }
    }
}

async fn stored(store: &Store) -> HashMap<AccountKey, QuotaState> {
    pool_repo::load_pool(store)
        .await
        .unwrap()
        .into_iter()
        .collect()
}

fn exported(state: &AppState) -> HashMap<AccountKey, QuotaState> {
    state.accounts.export_quotas().into_iter().collect()
}

store_contract_test!(a_failed_write_is_retried_with_the_latest_export, |store| {
    let state = app_state();
    let persister = PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_secs(3600),
    );
    let before = store_write_failures_for_tests("pool");

    mutate(&state, &[("a", 0.1)]);
    fail_writes(&store).await;
    let error = persister.flush_now().await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
    assert_eq!(store_write_failures_for_tests("pool"), before + 1);
    assert!(
        stored(&store).await.is_empty(),
        "the failed write left nothing"
    );

    // The pool keeps serving from memory and keeps changing.
    mutate(&state, &[("a", 0.6), ("b", 0.2)]);
    allow_writes(&store).await;
    persister.flush_now().await.unwrap();

    let want = exported(&state);
    assert_eq!(want.len(), 2);
    assert_eq!(
        stored(&store).await,
        want,
        "the retry wrote the latest export"
    );
    assert_eq!(store_write_failures_for_tests("pool"), before + 1);

    // Idle: nothing dirty, so nothing is written even while writes would fail.
    fail_writes(&store).await;
    persister.flush_now().await.unwrap();
    assert_eq!(store_write_failures_for_tests("pool"), before + 1);
    allow_writes(&store).await;
    persister.stop().await;
});

store_contract_test!(flush_now_writes_pending_state, |store| {
    let state = app_state();
    let persister = PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_secs(3600),
    );
    mutate(&state, &[("a", 0.3)]);
    persister.flush_now().await.unwrap();
    assert_eq!(stored(&store).await, exported(&state));
    persister.stop().await;
});

store_contract_test!(
    stop_and_flush_writes_a_claim_lost_to_an_aborted_flush,
    |store| {
        let state = app_state();
        let persister = PoolPersister::store(
            state.clone(),
            Arc::new(store.clone()),
            Duration::from_secs(3600),
        );
        mutate(&state, &[("a", 0.3)]);
        // An abort inside a flush leaves the flag claimed but nothing written.
        assert!(state.accounts.take_dirty());
        persister.stop_and_flush().await.unwrap();
        assert_eq!(stored(&store).await, exported(&state));
    }
);

store_contract_test!(a_lost_hold_halts_flushing_without_retry, |store| {
    let state = app_state();
    let persister = PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_secs(3600),
    );
    let before = store_write_failures_for_tests("pool");
    run(
        &store,
        &["UPDATE instance_hold SET holder_id = 'ghost' WHERE id = 1"],
    )
    .await;

    mutate(&state, &[("a", 0.3)]);
    let error = persister.flush_now().await.unwrap_err();
    assert!(matches!(error, StoreError::HoldLost { .. }), "{error:?}");
    assert!(!state.accounts.take_dirty(), "a lost hold is not retried");
    assert_eq!(store_write_failures_for_tests("pool"), before + 1);

    // Halted: later flushes fail fast and count nothing more.
    let again = persister.flush_now().await.unwrap_err();
    assert!(matches!(again, StoreError::HoldLost { .. }), "{again:?}");
    assert_eq!(store_write_failures_for_tests("pool"), before + 1);
    assert!(stored(&store).await.is_empty());
    persister.stop().await;
});

store_contract_test!(the_spawned_loop_flushes_on_its_own, |store| {
    let state = app_state();
    let persister = PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_millis(25),
    );
    mutate(&state, &[("a", 0.4)]);
    let want = exported(&state);
    let mut written = false;
    for _ in 0..200 {
        if stored(&store).await == want {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(written, "the loop never flushed the dirty pool");
    persister.stop().await;
});

store_contract_test!(restore_from_store_seeds_the_pool, |store| {
    let saved = quota(0.8);
    pool_repo::save_pool(&store, &[(key("a"), saved.clone())])
        .await
        .unwrap();
    let state = app_state();
    restore_from_store(&state, &store).await.unwrap();
    assert_eq!(
        state.accounts.raw_quota_for_test(&key("a")),
        Some((true, saved))
    );
});

store_contract_test!(restore_from_store_fails_loudly_on_a_bad_row, |store| {
    run(
        &store,
        &["INSERT INTO pool_quota (account_key, quota_json, updated_at) VALUES ('x', '{}', 1)"],
    )
    .await;
    let error = restore_from_store(&app_state(), &store).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
});

fn gateway_identity() -> crate::gateway::approval::Identity {
    crate::gateway::approval::Identity {
        sub: "dev@example.com".into(),
        email: "dev@example.com".into(),
        name: "dev".into(),
    }
}

async fn stored_sessions(store: &Store) -> HashMap<String, crate::gateway::refresh::RefreshRecord> {
    crate::store::sessions_repo::load_sessions(store)
        .await
        .unwrap()
        .into_iter()
        .map(|record| (record.token_sha256.clone(), record))
        .collect()
}

store_contract_test!(flush_now_writes_pending_gateway_sessions, |store| {
    let state = app_state();
    assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));
    let persister = PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_secs(3600),
    );
    // A session mutation the durable path never committed (for example a
    // revocation kept in memory through an outage) is pending.
    state
        .gateway_stores
        .refresh_tokens
        .issue(gateway_identity());
    assert!(stored_sessions(&store).await.is_empty());

    persister.flush_now().await.unwrap();
    let written = stored_sessions(&store).await;
    assert_eq!(written.len(), 1);
    assert_eq!(
        written.into_values().collect::<Vec<_>>(),
        state.gateway_stores.refresh_tokens.export()
    );
    persister.stop().await;
});

store_contract_test!(the_spawned_loop_flushes_pending_gateway_sessions, |store| {
    let state = app_state();
    assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));
    let persister = PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_millis(25),
    );
    state
        .gateway_stores
        .refresh_tokens
        .issue(gateway_identity());
    let mut written = false;
    for _ in 0..200 {
        if stored_sessions(&store).await.len() == 1 {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(written, "the loop never flushed the pending session");
    persister.stop().await;
});

store_contract_test!(
    a_lost_hold_on_a_gateway_only_flush_halts_the_loop,
    |store| {
        let state = app_state();
        assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));
        let persister = PoolPersister::store(
            state.clone(),
            Arc::new(store.clone()),
            Duration::from_millis(25),
        );
        run(
            &store,
            &["UPDATE instance_hold SET holder_id = 'ghost' WHERE id = 1"],
        )
        .await;
        // Only a session is pending; the pool stays clean.
        state
            .gateway_stores
            .refresh_tokens
            .issue(gateway_identity());

        let mut ended = false;
        for _ in 0..200 {
            if persister
                .task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(ended, "the loop kept retrying a fenced gateway write");
        assert!(persister.sink.as_ref().unwrap().halted());
        assert!(stored_sessions(&store).await.is_empty());
        persister.stop().await;
    }
);
