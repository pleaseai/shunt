//! The spend API with a store attached: success persists to the store, and a
//! store write failure is the existing Anthropic-shaped 500 that leaves memory
//! and the stored state untouched.

use std::sync::Arc;

use super::*;
use crate::gateway::spend::persist;
use crate::gateway::spend::store::{Period, Scope};
use crate::metrics::store_write_failures_for_tests;
use crate::store::contract::store_contract_test;
use crate::store::sessions_repo::tests::{
    block_durable_writes, fail_next_load, lose_next_commit_ack,
};
use crate::store::spend_repo::load_spend;
use crate::store::spend_repo::tests::{allow_writes, fail_writes};

store_contract_test!(
    create_and_delete_fail_cleanly_when_the_store_write_fails,
    |store| {
        let (config, _env) = SpendEnv::config("store-backed-failure");
        let (router, _, state) = build_router(config).unwrap();
        assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));
        let counter = store_write_failures_for_tests("spend");

        // A healthy create persists to the store, not to a file.
        let (response, created) =
            post(&router, WRITE_KEY, organization(json!("100"), "monthly")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let id = created["id"].as_str().unwrap().to_string();
        let stored = load_spend(&store).await.unwrap().unwrap();
        assert_eq!(stored, state.gateway_stores.spend.export());
        assert_eq!(stored.limits.len(), 1);

        fail_writes(&store).await;
        let (response, body) = post(&router, WRITE_KEY, organization(json!("200"), "weekly")).await;
        assert_error_response(&response, &body, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(store_write_failures_for_tests("spend"), counter + 1);

        let (response, body) = send(
            &router,
            request(
                "DELETE",
                &format!("/v1/organizations/spend_limits/{id}"),
                WRITE_KEY,
                None,
            ),
        )
        .await;
        assert_error_response(&response, &body, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(store_write_failures_for_tests("spend"), counter + 2);

        // Memory is unchanged: one limit, one audit record.
        let memory = state.gateway_stores.spend.export();
        assert_eq!(memory.limits.len(), 1);
        assert_eq!(memory.audit.len(), 1);
        allow_writes(&store).await;
        assert_eq!(load_spend(&store).await.unwrap().unwrap(), memory);

        // And the same request now succeeds, with the store as the destination.
        let (response, _) = post(&router, WRITE_KEY, organization(json!("200"), "weekly")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            load_spend(&store).await.unwrap().unwrap(),
            state.gateway_stores.spend.export()
        );
    }
);

store_contract_test!(restore_reproduces_what_save_wrote, |store| {
    let (config, _env) = SpendEnv::config("store-backed-restore");
    let (_, _, writer) = build_router(config.clone()).unwrap();
    writer.gateway_stores.attach_store(Arc::new(store.clone()));
    let mut next = writer.gateway_stores.spend.export();
    next = crate::gateway::spend::SpendStore::upsert_state(
        next,
        Scope::Organization,
        Period::Monthly,
        Some("9".into()),
        "admin-key:writer",
        "2026-10-01T00:00:00.000Z".into(),
    )
    .0;
    persist::save(&writer, &next).await.unwrap();
    writer.gateway_stores.spend.replace(next.clone());

    let (_, _, reader) = build_router(config).unwrap();
    reader.gateway_stores.attach_store(Arc::new(store.clone()));
    persist::restore(&reader).await.unwrap();
    assert_eq!(reader.gateway_stores.spend.export(), next);
});

/// Poll `condition` until it holds; the deadline is far beyond the GREEN
/// path's cost, so only a real hang reaches it.
async fn eventually<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !condition().await {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

store_contract_test!(
    a_dropped_mutation_cannot_split_the_store_from_memory,
    |store| {
        let (config, _env) = SpendEnv::config("store-backed-dropped");
        let (router, _, state) = build_router(config).unwrap();
        assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));

        let blocker = block_durable_writes(&store).await;
        let dropped = {
            let router = router.clone();
            tokio::spawn(async move {
                post(&router, WRITE_KEY, organization(json!("100"), "monthly")).await
            })
        };
        // The request holds the mutation gate from before its commit task is
        // spawned until that task finishes, so a held gate means the mutation
        // is under way and blocked on the durable write.
        let spend = &state.gateway_stores.spend;
        eventually("the request takes the mutation gate", || async {
            tokio::time::timeout(std::time::Duration::ZERO, spend.mutation_gate())
                .await
                .is_err()
        })
        .await;
        dropped.abort();
        blocker.rollback().await.unwrap();

        // The write was in flight: it finishes both halves, or neither.
        eventually("the detached commit replaces memory", || async {
            !spend.export().limits.is_empty()
        })
        .await;
        let memory = state.gateway_stores.spend.export();
        assert_eq!(
            memory.limits.len(),
            1,
            "the commit and the replace both ran"
        );
        assert_eq!(load_spend(&store).await.unwrap().unwrap(), memory);

        let (response, _) = post(&router, WRITE_KEY, organization(json!("200"), "weekly")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let stored = load_spend(&store).await.unwrap().unwrap();
        assert_eq!(stored, state.gateway_stores.spend.export());
        let mut ids: Vec<_> = stored.audit.iter().map(|record| record.id).collect();
        let all = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all, "no duplicate audit id");
    }
);

fn audit_ids_are_unique(state: &crate::gateway::spend::store::SpendState) -> bool {
    let mut ids: Vec<_> = state.audit.iter().map(|record| record.id).collect();
    let all = ids.len();
    ids.sort_unstable();
    ids.dedup();
    ids.len() == all
}

store_contract_test!(a_lost_commit_ack_that_landed_reports_success, |store| {
    let (config, _env) = SpendEnv::config("store-backed-lost-ack");
    let (router, _, state) = build_router(config).unwrap();
    assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));

    lose_next_commit_ack(&store);
    let (response, _) = post(&router, WRITE_KEY, organization(json!("100"), "monthly")).await;
    assert_eq!(response.status(), StatusCode::OK, "the write had landed");
    let memory = state.gateway_stores.spend.export();
    assert_eq!(memory.limits.len(), 1);
    assert_eq!(load_spend(&store).await.unwrap().unwrap(), memory);

    let (response, _) = post(&router, WRITE_KEY, organization(json!("200"), "weekly")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let stored = load_spend(&store).await.unwrap().unwrap();
    assert_eq!(stored, state.gateway_stores.spend.export());
    assert!(audit_ids_are_unique(&stored));
});

store_contract_test!(
    an_unreadable_unknown_commit_resyncs_before_the_next_mutation,
    |store| {
        let (config, _env) = SpendEnv::config("store-backed-resync");
        let (router, _, state) = build_router(config).unwrap();
        assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));

        lose_next_commit_ack(&store);
        fail_next_load(&store);
        let (response, body) =
            post(&router, WRITE_KEY, organization(json!("100"), "monthly")).await;
        assert_error_response(&response, &body, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(state.gateway_stores.spend.export().limits.is_empty());

        let (response, _) = post(&router, WRITE_KEY, organization(json!("200"), "weekly")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let stored = load_spend(&store).await.unwrap().unwrap();
        assert_eq!(stored, state.gateway_stores.spend.export());
        assert_eq!(
            stored.limits.len(),
            2,
            "the landed write was not overwritten"
        );
        assert!(audit_ids_are_unique(&stored));
    }
);
