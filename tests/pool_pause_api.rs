//! `PATCH /admin/api/pool/{provider}/accounts/{account_ref}` — runtime account pause.
//!
//! Covers the admin-surface half of pool pause: a write-tier credential can
//! toggle `paused` and see it reflected on `GET /admin/api/pool` immediately
//! (even before the account was ever selected), a read-tier credential is
//! refused, and the paused account drops out of `select_order`.

use std::net::SocketAddr;

use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, AdminConfig, AdminKey, AuthMode, Config, PoolConfig},
    server,
};
use tokio::task::JoinHandle;

mod common;

struct Gateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

/// A path that is guaranteed not to exist, so an explicit account entry never
/// falls back to scanning the real on-disk credential store.
fn nonexistent_credentials_path() -> String {
    std::env::temp_dir()
        .join(format!(
            "shunt-pool-pause-test-{}-missing.json",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

const ADMIN_WRITE_KEY: &str = "admin-write-0123456789abcdef012345";
const ADMIN_READ_KEY: &str = "admin-read-0123456789abcdef0123456";

fn admin_config() -> Config {
    let mut config = Config::default();
    let anthropic = config.providers.get_mut("anthropic").unwrap();
    anthropic.auth = AuthMode::ClaudeOauth;
    anthropic.accounts = vec![AccountConfig {
        name: "pause-me".to_string(),
        credentials: Some(nonexistent_credentials_path()),
        uuid: Some("pause-me-uuid".to_string()),
        ..Default::default()
    }];
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: "SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE".to_string(),
        tokens_file: None,
        write_keys: vec![AdminKey {
            id: "terraform".to_string(),
            key: ADMIN_WRITE_KEY.into(),
        }],
        read_keys: vec![AdminKey {
            id: "reporting".to_string(),
            key: ADMIN_READ_KEY.into(),
        }],
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        hide_observed: false,
        oidc: None,
    });
    config
}

/// `admin_config` plus `[server.pool]`, so `sort_by_reset` (config value or
/// runtime override) actually governs selection — required to observe the
/// override end to end, since `effective_sort_by_reset` is always `false`
/// without a pool table (see `AccountPool::effective_sort_by_reset`).
fn admin_config_with_pool() -> Config {
    let mut config = admin_config();
    config.server.pool = Some(PoolConfig::default());
    config
}

async fn start(mut config: Config) -> (Gateway, shunt::server::AppState) {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        Gateway {
            base_url: format!("http://{addr}"),
            task,
        },
        state,
    )
}

async fn pool_accounts(
    client: &reqwest::Client,
    gateway: &Gateway,
    provider: &str,
) -> Vec<serde_json::Value> {
    let response = client
        .get(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|table| table["provider"] == provider)
        .and_then(|table| table["accounts"].as_array())
        .cloned()
        .expect("provider accounts present")
}

async fn pool_account_ref(
    client: &reqwest::Client,
    gateway: &Gateway,
    provider: &str,
    name: &str,
) -> String {
    pool_accounts(client, gateway, provider)
        .await
        .into_iter()
        .find(|account| account["name"].as_str() == Some(name))
        .and_then(|account| account["account_ref"].as_str().map(str::to_string))
        .expect("account_ref present")
}

#[tokio::test]
async fn patch_pool_account_pauses_and_reflects_in_snapshot_and_selection() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:unused");
    let (gateway, state) = start(admin_config()).await;
    let client = reqwest::Client::new();
    let account_ref = pool_account_ref(&client, &gateway, "anthropic", "pause-me").await;

    let response = client
        .patch(format!(
            "{}/admin/api/pool/anthropic/accounts/{account_ref}",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{"paused":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Reflected on the dashboard read immediately -- no traffic needed first.
    let response = client
        .get(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    let accounts = body["providers"][0]["accounts"].as_array().unwrap();
    let account = accounts
        .iter()
        .find(|a| a["name"] == "pause-me")
        .expect("patched account present in pool snapshot");
    assert_eq!(account["paused"], true);
    assert_eq!(account["available"], false);

    // And enforced at selection time.
    let config = admin_config();
    let accounts = config.providers.get("anthropic").unwrap().accounts.clone();
    assert!(state
        .accounts
        .select_order("anthropic", &accounts, None, None, None)
        .is_empty());

    drop(gateway);
}

#[tokio::test]
async fn patch_pool_account_is_refused_for_a_read_key() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:unused-2");
    let (gateway, _state) = start(admin_config()).await;
    let client = reqwest::Client::new();
    let account_ref = pool_account_ref(&client, &gateway, "anthropic", "pause-me").await;

    let response = client
        .patch(format!(
            "{}/admin/api/pool/anthropic/accounts/{account_ref}",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_READ_KEY)
        .header("content-type", "application/json")
        .body(r#"{"paused":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    drop(gateway);
}

#[tokio::test]
async fn patch_pool_account_404s_for_an_unknown_account() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:unused-3");
    let (gateway, _state) = start(admin_config()).await;
    let client = reqwest::Client::new();

    let response = client
        .patch(format!(
            "{}/admin/api/pool/anthropic/accounts/acct_does_not_exist",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{"paused":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    drop(gateway);
}

fn admin_config_with_duplicate_names() -> Config {
    let mut config = admin_config();
    let anthropic = config.providers.get_mut("anthropic").unwrap();
    anthropic.accounts = vec![
        AccountConfig {
            name: "same-name".to_string(),
            credentials: Some(nonexistent_credentials_path()),
            uuid: Some("same-name-a".to_string()),
            ..Default::default()
        },
        AccountConfig {
            name: "same-name".to_string(),
            credentials: Some(nonexistent_credentials_path()),
            uuid: Some("same-name-b".to_string()),
            ..Default::default()
        },
    ];
    config
}

#[tokio::test]
async fn patch_pool_account_disambiguates_duplicate_display_names() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:duplicate-names");
    let config = admin_config_with_duplicate_names();
    let configured_accounts = config.providers.get("anthropic").unwrap().accounts.clone();
    let (gateway, state) = start(config).await;
    let client = reqwest::Client::new();

    let before = pool_accounts(&client, &gateway, "anthropic").await;
    let same_name: Vec<_> = before
        .iter()
        .filter(|account| account["name"].as_str() == Some("same-name"))
        .collect();
    assert_eq!(same_name.len(), 2);
    let first_ref = same_name[0]["account_ref"].as_str().unwrap().to_string();
    let second_ref = same_name[1]["account_ref"].as_str().unwrap().to_string();
    assert_ne!(first_ref, second_ref);

    let response = client
        .patch(format!(
            "{}/admin/api/pool/anthropic/accounts/{second_ref}",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{"paused":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let after = pool_accounts(&client, &gateway, "anthropic").await;
    let first = after
        .iter()
        .find(|account| account["account_ref"].as_str() == Some(first_ref.as_str()))
        .unwrap();
    let second = after
        .iter()
        .find(|account| account["account_ref"].as_str() == Some(second_ref.as_str()))
        .unwrap();
    assert_eq!(first["paused"], false);
    assert_eq!(second["paused"], true);

    let order = state
        .accounts
        .select_order("anthropic", &configured_accounts, None, None, None);
    assert_eq!(order.len(), 1, "only the targeted identity is excluded");

    drop(gateway);
}

/// `[server.pool]` is process-wide, so `PATCH /admin/api/pool` toggles the
/// reset-priority sort for every provider at once, with no `shunt.toml` edit
/// or restart. Also covers clearing the override back to config-following
/// with an explicit `{"sort_by_reset": null}` (distinct from omitting the
/// field, which is a no-op).
#[tokio::test]
async fn patch_pool_sort_by_reset_flips_the_process_wide_setting() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:unused-4");
    let (gateway, state) = start(admin_config_with_pool()).await;
    let client = reqwest::Client::new();

    // Starts unset: falls back to the config file's own value (false, the
    // `PoolConfig` default).
    let pool_cfg = PoolConfig::default();
    assert!(!state.accounts.effective_sort_by_reset(Some(&pool_cfg)));

    let response = client
        .get(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["sort_by_reset"], false);

    let response = client
        .patch(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{"sort_by_reset":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(state.accounts.effective_sort_by_reset(Some(&pool_cfg)));

    let response = client
        .get(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["sort_by_reset"], true);

    // A read key may see the setting but not change it.
    let response = client
        .patch(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_READ_KEY)
        .header("content-type", "application/json")
        .body(r#"{"sort_by_reset":false}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(state.accounts.effective_sort_by_reset(Some(&pool_cfg)));

    // Omitting the field entirely is a no-op: the override stays set.
    let response = client
        .patch(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(state.accounts.effective_sort_by_reset(Some(&pool_cfg)));

    // An explicit `null` clears the override, reverting to the config file's
    // own value (`false`, unchanged in this test).
    let response = client
        .patch(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{"sort_by_reset":null}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!state.accounts.effective_sort_by_reset(Some(&pool_cfg)));

    drop(gateway);
}

/// `effective_sort_by_reset` (and therefore `GET /admin/api/pool`) must report
/// `false` when `[server.pool]` is absent, even with the runtime override set
/// — that legacy branch never consults `sort_by_reset`, so reporting the
/// override as active there would claim an effect selection does not have.
#[tokio::test]
async fn patch_pool_sort_by_reset_has_no_effect_without_a_pool_table() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:unused-5");
    let (gateway, state) = start(admin_config()).await;
    let client = reqwest::Client::new();

    let response = client
        .patch(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .header("content-type", "application/json")
        .body(r#"{"sort_by_reset":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !state.accounts.effective_sort_by_reset(None),
        "no [server.pool] table means the override has no effect"
    );

    let response = client
        .get(format!("{}/admin/api/pool", gateway.base_url))
        .header("x-shunt-admin-token", ADMIN_WRITE_KEY)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["sort_by_reset"], false);

    drop(gateway);
}
