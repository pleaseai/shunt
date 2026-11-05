//! Session bindings through the store persister (ADR-0008 §6; FR-11, FR-12),
//! on SQLite and PostgreSQL.

use std::{sync::Arc, time::Duration};

use super::{restore_from_store, PoolPersister};
use crate::{
    accounts::BindingRow,
    config::{AccountConfig, AuthMode, Config},
    server::AppState,
    store::{contract::store_contract_test, pool_session_repo, Store, Value},
};

const PROVIDER: &str = "anthropic";

fn account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        uuid: Some(format!("uuid-{name}")),
        token_env: Some(format!("SHUNT_TEST_STORE_BINDING_{name}")),
        ..Default::default()
    }
}

fn pool_state(names: &[&str]) -> AppState {
    let mut config = Config::default();
    let provider = config.providers.get_mut(PROVIDER).unwrap();
    provider.auth = AuthMode::ClaudeOauth;
    provider.accounts = names.iter().map(|name| account(name)).collect();
    AppState::new(config, reqwest::Client::new()).expect("valid test config")
}

async fn accounts_of(state: &AppState) -> Vec<AccountConfig> {
    state
        .config
        .provider(PROVIDER)
        .unwrap()
        .resolve_pool_accounts()
        .await
        .unwrap()
}

fn persister(state: &AppState, store: &Store) -> PoolPersister {
    PoolPersister::store(
        state.clone(),
        Arc::new(store.clone()),
        Duration::from_secs(3600),
    )
}

/// Select for `session`, then commit `serve` as the account that served.
fn serve(state: &AppState, accounts: &[AccountConfig], session: &str, serve: usize) {
    let (_, ticket) =
        state
            .accounts
            .select_order_with_ticket(PROVIDER, accounts, Some(session), None, None);
    state
        .accounts
        .commit_session_binding(&ticket.unwrap(), &accounts[serve], true);
}

fn bound(state: &AppState, accounts: &[AccountConfig], session: &str) -> Option<String> {
    let (_, ticket) =
        state
            .accounts
            .select_order_with_ticket(PROVIDER, accounts, Some(session), None, None);
    let ticket = ticket.unwrap();
    accounts
        .iter()
        .find(|account| ticket.is_bound_to(account))
        .map(|account| account.name.clone())
}

/// Whether the pool holds any binding for `session`, whatever its account.
fn has_binding(state: &AppState, accounts: &[AccountConfig], session: &str) -> bool {
    let (_, ticket) =
        state
            .accounts
            .select_order_with_ticket(PROVIDER, accounts, Some(session), None, None);
    ticket.unwrap().is_bound()
}

async fn stored(store: &Store) -> Vec<BindingRow> {
    pool_session_repo::load_bindings(store).await.unwrap().0
}

async fn table_len(store: &Store) -> i64 {
    let mut tx = store.db().begin().await.unwrap();
    let row = tx
        .fetch_optional("SELECT COUNT(*) FROM pool_session_binding", &[])
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    row.int(0).unwrap()
}

// A flush writes bindings as deltas, hashed and hour-rounded; a restart
// restores the live rows of still-configured accounts and drops the rest,
// whose rows the next flush deletes.
store_contract_test!(
    bindings_persist_as_deltas_and_restore_by_the_file_rules,
    |store| {
        let before = pool_state(&["a", "b"]);
        let accounts = accounts_of(&before).await;
        let writer = persister(&before, &store);
        serve(&before, &accounts, "raw-session-one", 0);
        serve(&before, &accounts, "raw-session-two", 1);
        writer.flush_now().await.unwrap();
        let rows = stored(&store).await;
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.session_hash.len(), 64, "a SHA-256 hex, not a raw id");
            assert!(!row.session_hash.contains("raw-session"));
            assert_eq!(row.last_used % 3_600, 0, "hour-rounded");
        }

        // Only changed rows are written: a row removed behind the persister's
        // back stays removed when another binding is placed and flushed.
        let mut tx = store.db().begin().await.unwrap();
        tx.execute(
            "DELETE FROM pool_session_binding WHERE account_key = $1",
            &[Value::from(
                serde_json::to_string(&crate::accounts::account_key(PROVIDER, &accounts[1]))
                    .unwrap(),
            )],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        serve(&before, &accounts, "raw-session-three", 1);
        writer.flush_now().await.unwrap();
        assert_eq!(
            table_len(&store).await,
            2,
            "one kept, one inserted, none rewritten"
        );
        writer.stop().await;

        // Restart without account-b: only the binding to account-a survives.
        let after = pool_state(&["a"]);
        restore_from_store(&after, &store).await.unwrap();
        let remaining = accounts_of(&after).await;
        assert_eq!(
            bound(&after, &remaining, "raw-session-one").as_deref(),
            Some("a")
        );
        assert_eq!(bound(&after, &remaining, "raw-session-three"), None);
        assert_eq!(
            after.accounts.export_bindings().len(),
            1,
            "only the live row was restored"
        );
        assert!(
            !has_binding(&after, &remaining, "raw-session-three"),
            "no binding survives for the removed account"
        );
        assert!(
            after.accounts.take_dirty(),
            "dropped rows mark the pool dirty"
        );
        after.accounts.mark_dirty();
        let writer = persister(&after, &store);
        writer.flush_now().await.unwrap();
        assert_eq!(table_len(&store).await, 1, "the dropped row is deleted");
        writer.stop().await;
    }
);

// An undecodable stored binding is dropped with a warning instead of failing
// boot, and the next flush deletes it.
store_contract_test!(
    an_undecodable_stored_binding_is_dropped_at_restore,
    |store| {
        let mut tx = store.db().begin().await.unwrap();
        tx.execute(
            "INSERT INTO pool_session_binding (provider, session_hash, account_key, last_used) \
         VALUES ($1, $2, $3, $4)",
            &[
                Value::from(PROVIDER),
                Value::from("ab".repeat(32).as_str()),
                Value::from("not json"),
                Value::from(3_600_i64),
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let state = pool_state(&["a"]);
        restore_from_store(&state, &store).await.unwrap();
        assert!(state.accounts.export_bindings().is_empty());
        assert!(
            state.accounts.take_dirty(),
            "restore must leave the pool dirty so the flush deletes the dropped row"
        );
        state.accounts.mark_dirty();
        let writer = persister(&state, &store);
        writer.flush_now().await.unwrap();
        assert_eq!(table_len(&store).await, 0);
        writer.stop().await;
    }
);

// A COMMIT that errors may still land, so the flush after it re-reads the
// table instead of diffing against the snapshot from before it: a binding the
// uncertain commit inserted and that is gone by the next flush is deleted.
store_contract_test!(a_failed_commit_drops_the_binding_baseline, |store| {
    // Only PostgreSQL bounds a round trip: SQLite is a local file.
    if !matches!(store.db(), crate::store::db::Db::Sqlite(_)) {
        #[cfg(feature = "store-postgres")]
        {
            use crate::state_persist::store_persister::StoreSink;
            use std::sync::atomic::AtomicBool;

            let mut tx = store.db().begin().await.unwrap();
            // A deferred trigger makes COMMIT itself take a second, the
            // way a COMMIT whose reply is lost looks to the client.
            for sql in [
                "CREATE FUNCTION slow_binding_commit_fn() RETURNS trigger AS \
                     $body$ BEGIN PERFORM pg_sleep(1); RETURN NULL; END $body$ LANGUAGE plpgsql",
                "CREATE CONSTRAINT TRIGGER slow_binding_commit AFTER INSERT \
                     ON pool_session_binding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                     EXECUTE FUNCTION slow_binding_commit_fn()",
            ] {
                tx.execute(sql, &[]).await.unwrap();
            }
            tx.commit().await.unwrap();

            let sink = StoreSink {
                store: Arc::new(store.clone()),
                gate: tokio::sync::Mutex::new(None),
                halted: AtomicBool::new(false),
            };
            let row = BindingRow {
                provider: PROVIDER.to_string(),
                session_hash: "ab".repeat(32),
                account: crate::accounts::account_key(PROVIDER, &account("a")),
                last_used: 3_600,
            };
            let mut written = None;
            let bound =
                crate::store::db::set_round_trip_timeout("COMMIT", Duration::from_millis(300));
            let error = sink
                .write(&[], std::slice::from_ref(&row), &mut written)
                .await
                .unwrap_err();
            drop(bound);
            assert!(
                matches!(error, crate::store::StoreError::CommitUncertain { .. }),
                "{error:?}"
            );
            assert!(written.is_none(), "a failed COMMIT leaves no baseline");

            // The uncertain COMMIT lands (the next durable write waits on
            // the hold row it locked); the binding is then gone from the
            // pool, and the next flush must delete its row.
            sink.write(&[], &[], &mut written).await.unwrap();
            assert_eq!(table_len(&store).await, 0);
        }
    }
});
