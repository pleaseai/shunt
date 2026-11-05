//! `bound_sessions` on `GET /admin/api/pool` (FR-14, AC-019) and the
//! `PATCH /admin/api/pool` `sort_by_reset` override leaving bindings in place
//! (AC-020).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::config::{AccountConfig, AdminConfig, AdminKey, AuthMode, Config, PoolConfig};
use crate::server::{build_router, AppState};

const PROVIDER: &str = "anthropic";
const ADMIN_KEY: &str = "admin-write-0123456789abcdef012345";

fn account(name: &str, uuid: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        // Never read: the pool view and selection need no credential, and an
        // explicit path keeps the test off the real account store.
        credentials: Some(
            std::env::temp_dir()
                .join(format!(
                    "shunt-admin-binding-{}-missing.json",
                    std::process::id()
                ))
                .to_string_lossy()
                .into_owned(),
        ),
        uuid: Some(uuid.to_string()),
        ..Default::default()
    }
}

/// Two aliases of one identity (`shared`) and a second identity (`solo`).
fn config(pool: Option<PoolConfig>) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut(PROVIDER).unwrap();
    provider.auth = AuthMode::ClaudeOauth;
    provider.accounts = vec![
        account("alias-1", "shared"),
        account("alias-2", "shared"),
        account("solo", "solo-identity"),
    ];
    config.server.pool = pool;
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: format!("SHUNT_TEST_ADMIN_BINDING_UNSET_{}", std::process::id()),
        tokens_file: None,
        write_keys: vec![AdminKey {
            id: "ops".to_string(),
            key: ADMIN_KEY.into(),
        }],
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        hide_observed: false,
        oidc: None,
    });
    config
}

/// Place `session` on `provider` with `accounts[serve]` as the account that
/// served it.
fn bind(state: &AppState, provider: &str, accounts: &[AccountConfig], session: &str, serve: usize) {
    let (_, ticket) =
        state
            .accounts
            .select_order_with_ticket(provider, accounts, Some(session), None, None);
    state.accounts.commit_session_binding(
        &ticket.expect("a session carries a ticket"),
        &accounts[serve],
        false,
    );
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn get_pool(router: &axum::Router) -> (Value, String) {
    let request = Request::get("/admin/api/pool")
        .header("x-shunt-admin-token", ADMIN_KEY)
        .body(Body::empty())
        .unwrap();
    let (status, text) = send(router, request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    (serde_json::from_str(&text).unwrap(), text)
}

/// `(name, account_ref, bound_sessions)` for every account row of `PROVIDER`.
fn rows(body: &Value) -> Vec<(String, String, u64)> {
    body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|table| table["provider"] == PROVIDER)
        .and_then(|table| table["accounts"].as_array())
        .expect("provider accounts present")
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap().to_string(),
                row["account_ref"].as_str().unwrap().to_string(),
                row["bound_sessions"]
                    .as_u64()
                    .expect("every row carries bound_sessions"),
            )
        })
        .collect()
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

#[tokio::test]
async fn bound_sessions_count_once_per_identity_across_aliases() {
    let (router, _shared, state) = build_router(config(None)).unwrap();
    let accounts = accounts_of(&state).await;
    assert_eq!(accounts.len(), 3, "both aliases stay distinct rows");

    // Before any binding every row reports zero.
    let (body, _) = get_pool(&router).await;
    assert!(rows(&body).iter().all(|(_, _, bound)| *bound == 0));

    // Two sessions served by alias-1 and one by alias-2: three sessions on
    // the one `shared` identity. One session on `solo`.
    bind(&state, PROVIDER, &accounts, "raw-session-1", 0);
    bind(&state, PROVIDER, &accounts, "raw-session-2", 0);
    bind(&state, PROVIDER, &accounts, "raw-session-3", 1);
    bind(&state, PROVIDER, &accounts, "raw-session-4", 2);
    // Another provider table's session on the same identity is that table's.
    bind(&state, "elsewhere", &accounts, "raw-session-5", 0);

    let (body, text) = get_pool(&router).await;
    let rows = rows(&body);
    let by_name = |name: &str| rows.iter().find(|row| row.0 == name).unwrap().clone();
    let (alias_1, alias_2, solo) = (by_name("alias-1"), by_name("alias-2"), by_name("solo"));
    assert_eq!(
        alias_1.1, alias_2.1,
        "aliases of one identity share account_ref"
    );
    assert_ne!(alias_1.1, solo.1);
    assert_eq!(alias_1.2, 3, "the identity's count, not alias-1's own two");
    assert_eq!(alias_2.2, 3, "the same one count, not alias-2's own one");
    assert_eq!(solo.2, 1);
    // Counted once per identity: one count per distinct account_ref sums to
    // this provider's four live sessions.
    let mut per_identity: Vec<(String, u64)> =
        rows.iter().map(|row| (row.1.clone(), row.2)).collect();
    per_identity.sort();
    per_identity.dedup();
    assert_eq!(per_identity.iter().map(|(_, bound)| bound).sum::<u64>(), 4);
    // Counts only: neither a raw session id nor a session hash is exposed.
    assert!(!text.contains("raw-session"), "{text}");
    for row in state.accounts.export_bindings() {
        assert!(!text.contains(&row.session_hash), "{text}");
    }
}

#[tokio::test]
async fn patching_sort_by_reset_leaves_bindings_in_place() {
    let (router, _shared, state) = build_router(config(Some(PoolConfig::default()))).unwrap();
    let accounts = accounts_of(&state).await;
    bind(&state, PROVIDER, &accounts, "raw-session-1", 0);
    bind(&state, PROVIDER, &accounts, "raw-session-2", 2);
    bind(&state, PROVIDER, &accounts, "raw-session-3", 2);

    let mut table_before = state.accounts.export_bindings();
    table_before.sort_by(|a, b| a.session_hash.cmp(&b.session_hash));
    let (body, _) = get_pool(&router).await;
    let rows_before = rows(&body);
    assert_eq!(body["sort_by_reset"], false);
    assert!(rows_before.iter().any(|(_, _, bound)| *bound > 0));

    let request = Request::patch("/admin/api/pool")
        .header("x-shunt-admin-token", ADMIN_KEY)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sort_by_reset":true}"#))
        .unwrap();
    let (status, text) = send(&router, request).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (body, _) = get_pool(&router).await;
    assert_eq!(body["sort_by_reset"], true, "the override took effect");
    assert_eq!(rows(&body), rows_before, "bound_sessions unchanged");
    let mut table_after = state.accounts.export_bindings();
    table_after.sort_by(|a, b| a.session_hash.cmp(&b.session_hash));
    assert_eq!(table_after, table_before, "the binding table is unchanged");
}
