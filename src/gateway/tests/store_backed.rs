//! The token endpoint with a store attached ([`crate::gateway::durable`]): the
//! token diff commits before the response, a failed add-active write rolls
//! memory back to a `503`, and a failed revoke-only write keeps the revocation.
//! Failures are injected inside the engine (a trigger that aborts writes to
//! `refresh_token`), then removed.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use axum::http::HeaderMap;

use super::*;
use crate::gateway::{durable, refresh::RefreshRecord};
use crate::metrics::store_write_failures_for_tests;
use crate::store::contract::store_contract_test;
use crate::store::sessions_repo::{
    index, load_sessions,
    tests::{
        allow_writes, block_durable_writes, fail_next_load, fail_writes, lose_next_commit_ack,
    },
};
use crate::store::{Store, StoreError};

static NEXT_LABEL: AtomicU64 = AtomicU64::new(0);

const DEVICE_CODE_GRANT: &str = "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code";

/// A gateway router whose state has `store` attached. `GatewayEnv` must stay
/// alive for as long as the config may be rebuilt into another router.
fn gateway(store: &Store) -> (Config, GatewayEnv, Router, AppState) {
    let label = format!("durable-{}", NEXT_LABEL.fetch_add(1, Ordering::Relaxed));
    let (config, env) = GatewayEnv::config(&label);
    let (router, state) = restarted(&config, store);
    (config, env, router, state)
}

/// A fresh process over the same `store`: new router, new memory, same store.
fn restarted(config: &Config, store: &Store) -> (Router, AppState) {
    let (router, _, state) = build_router(config.clone()).unwrap();
    assert!(state.gateway_stores.attach_store(Arc::new(store.clone())));
    (router, state)
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

async fn post_token(router: &Router, body: String) -> Reply {
    let response = router
        .clone()
        .oneshot(form_request("/oauth/token", body))
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).expect("JSON response"),
    }
}

async fn refresh(router: &Router, token: &str) -> Reply {
    post_token(
        router,
        format!("grant_type=refresh_token&refresh_token={token}&client_id=claude-code"),
    )
    .await
}

/// Approve a device grant directly in the grant store and return its code.
fn approved_device_code(state: &AppState, name: &str) -> String {
    let device_code = format!("device-{name}");
    let user_code = format!("BCDF-{name}");
    let grants = &state.gateway_stores.device_grants;
    assert!(grants.create(device_code.clone(), user_code.clone()));
    assert!(grants.approve(
        &user_code,
        Identity {
            sub: format!("{name}@example.com"),
            email: format!("{name}@example.com"),
            name: name.to_string(),
        },
    ));
    device_code
}

async fn device_grant(router: &Router, state: &AppState, name: &str) -> Reply {
    let device_code = approved_device_code(state, name);
    post_token(
        router,
        format!("grant_type={DEVICE_CODE_GRANT}&device_code={device_code}&client_id=claude-code"),
    )
    .await
}

async fn login(router: &Router, state: &AppState, name: &str) -> String {
    let reply = device_grant(router, state, name).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["refresh_token"].as_str().unwrap().to_string()
}

fn sha256(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn memory(state: &AppState) -> HashMap<String, RefreshRecord> {
    index(state.gateway_stores.refresh_tokens.export())
}

async fn stored(store: &Store) -> HashMap<String, RefreshRecord> {
    index(load_sessions(store).await.unwrap())
}

fn assert_unavailable(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body, json!({ "error": "temporarily_unavailable" }));
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
}

fn active(records: &HashMap<String, RefreshRecord>) -> usize {
    records
        .values()
        .filter(|record| record.inactive_since.is_none())
        .count()
}

store_contract_test!(issue_and_rotation_commit_before_the_response, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let first = login(&router, &state, "ada").await;
    assert_eq!(stored(&store).await, memory(&state));
    assert_eq!(active(&stored(&store).await), 1);

    let rotated = refresh(&router, &first).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    assert_eq!(rotated.headers[header::CACHE_CONTROL], "no-store");
    let second = rotated.body["refresh_token"].as_str().unwrap().to_string();

    let rows = stored(&store).await;
    assert_eq!(
        rows,
        memory(&state),
        "the store holds exactly what memory does"
    );
    assert_eq!(rows.len(), 2);
    assert!(
        rows[&sha256(&first)].inactive_since.is_some(),
        "old token is a tombstone"
    );
    assert_eq!(
        rows[&sha256(&second)].inactive_since,
        None,
        "new token is active"
    );
    assert!(
        rows.keys()
            .all(|key| key.len() == 64 && key != &first && key != &second),
        "only hashes are stored"
    );
});

store_contract_test!(
    a_failed_rotation_rolls_back_and_the_retry_succeeds,
    |store| {
        let (_config, _env, router, state) = gateway(&store);
        let first = login(&router, &state, "ada").await;
        let before = memory(&state);
        let failures = store_write_failures_for_tests("gateway");

        fail_writes(&store).await;
        let reply = refresh(&router, &first).await;
        assert_unavailable(&reply);
        assert_eq!(store_write_failures_for_tests("gateway"), failures + 1);
        assert_eq!(memory(&state), before, "memory is rolled back");
        assert_eq!(stored(&store).await, before, "the store is untouched");

        // The same refresh token still rotates: the failed attempt was not a use.
        allow_writes(&store).await;
        let retry = refresh(&router, &first).await;
        assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
        let second = retry.body["refresh_token"].as_str().unwrap().to_string();
        let rows = stored(&store).await;
        assert_eq!(rows, memory(&state));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[&sha256(&second)].inactive_since, None);
        assert!(rows[&sha256(&first)].inactive_since.is_some());
        assert_eq!(store_write_failures_for_tests("gateway"), failures + 1);
    }
);

store_contract_test!(a_failed_device_grant_issues_nothing, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let failures = store_write_failures_for_tests("gateway");

    fail_writes(&store).await;
    let reply = device_grant(&router, &state, "ada").await;
    assert_unavailable(&reply);
    assert!(reply.body.get("refresh_token").is_none());
    assert_eq!(store_write_failures_for_tests("gateway"), failures + 1);
    assert!(
        memory(&state).is_empty(),
        "no token is left active in memory"
    );
    assert!(stored(&store).await.is_empty());

    allow_writes(&store).await;
    login(&router, &state, "grace").await;
    assert_eq!(active(&stored(&store).await), 1);
});

store_contract_test!(
    a_replay_during_an_outage_stays_revoked_and_is_flushed,
    |store| {
        let (config, _env, router, state) = gateway(&store);
        let first = login(&router, &state, "ada").await;
        let second = refresh(&router, &first).await.body["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        let failures = store_write_failures_for_tests("gateway");

        fail_writes(&store).await;
        let replay = refresh(&router, &first).await;
        assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
        assert_eq!(replay.body, json!({ "error": "invalid_grant" }));
        assert_eq!(replay.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(store_write_failures_for_tests("gateway"), failures + 1);
        assert_eq!(
            active(&memory(&state)),
            0,
            "the family stays revoked in memory"
        );
        assert_eq!(active(&stored(&store).await), 1, "the store is behind");

        // Still failing: the live descendant is denied from memory, not rolled back.
        let denied = refresh(&router, &second).await;
        assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
        assert_eq!(active(&memory(&state)), 0);

        // The store recovers; the persister tick writes the pending revocation.
        allow_writes(&store).await;
        durable::flush_pending(&state).await.unwrap();
        assert_eq!(stored(&store).await, memory(&state));
        assert_eq!(active(&stored(&store).await), 0);

        // A restart over the store still denies the family.
        let (restarted_router, restarted_state) = restarted(&config, &store);
        durable::restore_from_store(&restarted_state).await.unwrap();
        let after_restart = refresh(&restarted_router, &second).await;
        assert_eq!(after_restart.status, StatusCode::UNAUTHORIZED);
        assert_eq!(after_restart.body, json!({ "error": "invalid_grant" }));
    }
);

store_contract_test!(
    the_next_durable_write_carries_a_pending_revocation,
    |store| {
        let (_config, _env, router, state) = gateway(&store);
        let first = login(&router, &state, "ada").await;
        let second = refresh(&router, &first).await.body["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();

        fail_writes(&store).await;
        assert_eq!(
            refresh(&router, &first).await.status,
            StatusCode::UNAUTHORIZED
        );
        allow_writes(&store).await;

        // Another user's login is the next durable write; it persists the revocation.
        login(&router, &state, "grace").await;
        let rows = stored(&store).await;
        assert_eq!(rows, memory(&state));
        assert!(rows[&sha256(&second)].inactive_since.is_some());
        assert_eq!(active(&rows), 1, "only grace's token is active");
    }
);

store_contract_test!(
    a_rolled_back_rotation_does_not_undo_a_pending_revocation,
    |store| {
        let (_config, _env, router, state) = gateway(&store);
        let ada = login(&router, &state, "ada").await;
        let ada_next = refresh(&router, &ada).await.body["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        let grace = login(&router, &state, "grace").await;

        fail_writes(&store).await;
        assert_eq!(
            refresh(&router, &ada).await.status,
            StatusCode::UNAUTHORIZED
        );
        // Grace's rotation fails and rolls back to `before`, which already holds
        // the revocation of ada's family.
        assert_unavailable(&refresh(&router, &grace).await);
        let rows = memory(&state);
        assert!(rows[&sha256(&ada_next)].inactive_since.is_some());
        assert_eq!(rows[&sha256(&grace)].inactive_since, None);

        allow_writes(&store).await;
        assert_eq!(refresh(&router, &grace).await.status, StatusCode::OK);
        assert_eq!(stored(&store).await, memory(&state));
        assert!(stored(&store).await[&sha256(&ada_next)]
            .inactive_since
            .is_some());
    }
);

store_contract_test!(concurrent_rotations_of_one_token_rotate_once, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let first = login(&router, &state, "ada").await;

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let router = router.clone();
        let first = first.clone();
        tasks.push(tokio::spawn(async move { refresh(&router, &first).await }));
    }
    let mut rotated = Vec::new();
    let mut denied = 0;
    for task in tasks {
        let reply = task.await.unwrap();
        match reply.status {
            StatusCode::OK => {
                rotated.push(reply.body["refresh_token"].as_str().unwrap().to_string())
            }
            StatusCode::UNAUTHORIZED => denied += 1,
            other => panic!("unexpected status {other}: {}", reply.body),
        }
    }
    assert_eq!(
        rotated.len(),
        1,
        "exactly one rotation of the token succeeds"
    );
    assert_eq!(denied, 31);

    // The 31 replays revoke the family, so the one descendant is not resurrected.
    let rows = stored(&store).await;
    assert_eq!(rows, memory(&state), "the store matches memory");
    assert_eq!(rows.len(), 2);
    assert_eq!(active(&rows), 0);
    assert_eq!(
        refresh(&router, &rotated[0]).await.status,
        StatusCode::UNAUTHORIZED
    );
});

store_contract_test!(concurrent_logins_each_commit_once, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let mut tasks = Vec::new();
    for index in 0..32 {
        let router = router.clone();
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            device_grant(&router, &state, &format!("user{index}"))
                .await
                .status
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
    let rows = stored(&store).await;
    assert_eq!(rows.len(), 32);
    assert_eq!(rows, memory(&state));
});

store_contract_test!(a_lost_hold_fails_a_rotation_with_503, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let first = login(&router, &state, "ada").await;
    let before = memory(&state);
    let failures = store_write_failures_for_tests("gateway");

    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "UPDATE instance_hold SET holder_id = 'ghost' WHERE id = 1",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert_unavailable(&refresh(&router, &first).await);
    assert_eq!(store_write_failures_for_tests("gateway"), failures + 1);
    assert_eq!(memory(&state), before, "the rotation is rolled back");
    assert_eq!(stored(&store).await, before);
    let error = durable::flush_pending(&state).await;
    assert!(
        error.is_ok(),
        "nothing is pending after the rollback: {error:?}"
    );
});

store_contract_test!(
    a_dropped_request_does_not_strand_an_uncommitted_rotation,
    |store| {
        let (_config, _env, router, state) = gateway(&store);
        let first = login(&router, &state, "ada").await;

        let stores = state.gateway_stores.clone();
        let shared = Arc::new(store.clone());
        let request = tokio::spawn(async move { durable::rotate(stores, shared, &first).await });
        // Let the commit reach its store write, then drop the request as a client
        // disconnect would.
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        request.abort();

        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if stored(&store).await == memory(&state) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(settled.is_ok(), "memory and the store converge");
        assert_eq!(stored(&store).await, memory(&state));
        assert_eq!(
            stored(&store).await.len(),
            2,
            "the rotation committed whole, not half in memory"
        );
    }
);

store_contract_test!(aborted_waiters_leave_no_detached_commits_queued, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let first = login(&router, &state, "ada").await;

    let blocker = block_durable_writes(&store).await;
    let running = {
        let (router, first) = (router.clone(), first.clone());
        tokio::spawn(async move { refresh(&router, &first).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // The running commit holds the durable lock; these queue behind it.
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let (router, first) = (router.clone(), first.clone());
            tokio::spawn(async move { refresh(&router, &first).await })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    for waiter in &waiters {
        waiter.abort();
    }
    blocker.rollback().await.unwrap();

    assert_eq!(running.await.unwrap().status, StatusCode::OK);
    // Detached waiters would replay `first` and revoke the family.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(active(&memory(&state)), 1, "no waiter replayed the token");
    assert_eq!(stored(&store).await, memory(&state));
});

store_contract_test!(
    an_unknown_commit_outcome_is_reconciled_with_the_store,
    |store| {
        let (_config, _env, router, state) = gateway(&store);
        let first = login(&router, &state, "ada").await;

        lose_next_commit_ack(&store);
        let reply = refresh(&router, &first).await;
        assert_eq!(reply.status, StatusCode::OK, "the write had landed");
        let second = reply.body["refresh_token"].as_str().unwrap().to_string();
        assert_eq!(stored(&store).await, memory(&state));
        assert_eq!(active(&memory(&state)), 1);

        // The next rotation works and the family was not revoked.
        assert_eq!(refresh(&router, &second).await.status, StatusCode::OK);
        assert_eq!(stored(&store).await, memory(&state));
        assert_eq!(active(&memory(&state)), 1);
    }
);

store_contract_test!(an_unreadable_unknown_commit_resyncs_the_baseline, |store| {
    let (_config, _env, router, state) = gateway(&store);
    let first = login(&router, &state, "ada").await;

    // The rotation lands, its ack is lost, and the read-back fails too.
    lose_next_commit_ack(&store);
    fail_next_load(&store);
    assert_unavailable(&refresh(&router, &first).await);

    // The next commit reloads the baseline, then rewrites the store to match
    // memory: the token the client still holds rotates, the family survives.
    assert_eq!(refresh(&router, &first).await.status, StatusCode::OK);
    assert_eq!(stored(&store).await, memory(&state));
    assert_eq!(active(&memory(&state)), 1);
});

store_contract_test!(restore_from_store_seeds_memory_and_the_baseline, |store| {
    let (config, _env, router, state) = gateway(&store);
    let first = login(&router, &state, "ada").await;
    let second = refresh(&router, &first).await.body["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    let (restarted_router, restarted_state) = restarted(&config, &store);
    durable::restore_from_store(&restarted_state).await.unwrap();
    assert_eq!(memory(&restarted_state), stored(&store).await);
    let rotated = refresh(&restarted_router, &second).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    assert_eq!(stored(&store).await, memory(&restarted_state));
    assert_eq!(
        refresh(&restarted_router, &first).await.status,
        StatusCode::UNAUTHORIZED,
        "a pre-restart tombstone still detects replay"
    );
});

store_contract_test!(restore_fails_loudly_on_an_undecodable_row, |store| {
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "INSERT INTO refresh_token (token_sha256, family, identity_json, inactive_since, issued_at) \
         VALUES ('x', 'f', 'nope', NULL, 1)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (_config, _env, _router, state) = gateway(&store);
    let error = durable::restore_from_store(&state).await.unwrap_err();
    assert!(matches!(error, StoreError::Database { .. }), "{error:?}");
    assert!(memory(&state).is_empty());
});

store_contract_test!(
    flush_pending_is_a_noop_without_a_store_or_changes,
    |store| {
        let (config, _env, _router, state) = gateway(&store);
        fail_writes(&store).await;
        durable::flush_pending(&state).await.unwrap();
        allow_writes(&store).await;

        let (_, _, detached) = build_router(config).unwrap();
        durable::flush_pending(&detached).await.unwrap();
        durable::restore_from_store(&detached).await.unwrap();
    }
);
