use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::ErrorKind,
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, AuthMode, Config, RouteConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

struct BearerToken(String);

impl Match for BearerToken {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(auth("Bearer", &self.0).as_str())
    }
}

struct AccountUuidBody {
    expected: &'static str,
    forbidden: &'static str,
}
impl Match for AccountUuidBody {
    fn matches(&self, request: &Request) -> bool {
        let Ok(outer) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
            return false;
        };
        let Some(user_id) = outer
            .pointer("/metadata/user_id")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id) else {
            return false;
        };
        let account_uuid = inner
            .get("account_uuid")
            .and_then(serde_json::Value::as_str);
        account_uuid == Some(self.expected) && account_uuid != Some(self.forbidden)
    }
}

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn auth(scheme: &str, token: &str) -> String {
    format!("{scheme} {token}")
}

fn account(name: &str, token_env: &str, uuid: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        uuid: Some(uuid.to_string()),
        ..Default::default()
    }
}

/// A name-only pool entry that resolves against the shunt account store
/// (`SHUNT_CLAUDE_ACCOUNTS_DIR/<name>.json`).
fn store_account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        ..Default::default()
    }
}

/// Serializes the refresh-path tests, which set the process-global
/// `SHUNT_CLAUDE_ACCOUNTS_DIR` / `SHUNT_CLAUDE_TOKEN_URL` env vars.
static REFRESH_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn unique_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-multi-refresh-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a refreshable store account file whose access token is valid far into
/// the future, so it is used verbatim on the first upstream POST (the 401 is
/// what drives the RefreshRetry path) rather than being refreshed on read.
fn write_store_account(dir: &std::path::Path, name: &str, access: &str, refresh: &str, uuid: &str) {
    let body = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":4102444800000}},"shuntAccountUuid":"{uuid}"}}"#
    );
    fs::write(dir.join(format!("{name}.json")), body).unwrap();
}

fn test_config(upstream_base_url: &str, first: AccountConfig, second: AccountConfig) -> Config {
    test_config_accounts(upstream_base_url, vec![first, second])
}

/// Like `test_config`, but accepts an arbitrary account list (e.g. for tests
/// that need a duplicate-identity alias alongside two distinct accounts).
fn test_config_accounts(upstream_base_url: &str, accounts: Vec<AccountConfig>) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.base_url = upstream_base_url.to_string();
    provider.auth = AuthMode::ClaudeOauth;
    provider.accounts = accounts;
    config.routes.push(RouteConfig {
        model: "pooled-model".to_string(),
        provider: "anthropic".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    config
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// Like [`start_gateway_with`], but also hands back the request `AppState` so a
/// test can inspect process-lifetime pool health (the needs-re-login mark) that
/// no response header reports.
async fn start_gateway_with_state(mut config: Config) -> (TestGateway, shunt::server::AppState) {
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
        TestGateway {
            base_url: format!("http://{addr}"),
            task,
        },
        state,
    )
}

/// The `AccountConfig` shape `resolve_pool_accounts` hands the request path for
/// a name-only pool entry backed by the shunt account store: the store family
/// is stamped inline, and the UUID stays `None` because such an entry carries
/// neither a `credentials` path nor a `token_env` for `inline_identity_key` to
/// key on. Pool health is keyed off exactly this, so a test asserting on the
/// mark must reconstruct it rather than reusing the pre-resolution config.
///
/// Every caller pairs its assertion with a `has_state` check, which fails loudly
/// if this reconstruction ever stops matching the key selection actually
/// created — a silently wrong key would otherwise read as "not marked".
fn resolved_store_account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        store_family: Some(shunt::accounts::StoreFamily::Claude),
        ..Default::default()
    }
}

/// Assert the pool actually observed this account, so a `needs_relogin`
/// assertion below can never pass or fail against a key nothing ever wrote.
fn assert_pool_observed(state: &shunt::server::AppState, account: &AccountConfig) {
    let snapshots = state
        .accounts
        .snapshot("anthropic", std::slice::from_ref(account), None, None);
    assert!(
        snapshots[0].has_state,
        "the pool must hold health for {:?} — the reconstructed account key does not match \
         the one the request path created",
        account.name
    );
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

fn session_id_for_account(index: usize, account_count: usize) -> String {
    (0..1000)
        .map(|candidate| format!("session-{candidate}"))
        .find(|session_id| {
            let mut hasher = DefaultHasher::new();
            session_id.hash(&mut hasher);
            hasher.finish() as usize % account_count == index
        })
        .expect("a session id should map to the requested account")
}

async fn post_messages(gateway: &TestGateway, session_id: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"pooled-model","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
        );
    if let Some(session_id) = session_id {
        request = request.header("x-claude-code-session-id", session_id);
    }
    request.send().await.unwrap()
}

async fn post_messages_with_account_uuid(
    gateway: &TestGateway,
    account_uuid: &str,
) -> reqwest::Response {
    let user_id = serde_json::json!({
        "account_uuid": account_uuid,
        "device": "cli"
    })
    .to_string();
    reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "model": "pooled-model",
            "max_tokens": 16,
            "metadata": {"user_id": user_id},
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn account_uuid_is_rewritten_for_each_account_during_rotation() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "uuid-a"].concat();
    let token_b = ["fake-oauth-", "uuid-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_UUID_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_UUID_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .and(AccountUuidBody {
            expected: "uuid-a",
            forbidden: "inbound-uuid",
        })
        .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"error":"rotate"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .and(AccountUuidBody {
            expected: "uuid-b",
            forbidden: "uuid-a",
        })
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_UUID_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_UUID_B", "uuid-b"),
    ))
    .await;

    let response = post_messages_with_account_uuid(&gateway, "inbound-uuid").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_UUID_A");
    std::env::remove_var("SHUNT_TEST_MULTI_UUID_B");
}

#[tokio::test]
async fn quota_429_rotates_and_cools_down_the_rejected_account() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "quota-a"].concat();
    let token_b = ["fake-oauth-", "quota-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_QUOTA_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_QUOTA_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .insert_header("anthropic-ratelimit-unified-5h-status", "rejected")
                .set_body_string(r#"{"error":"account a quota exhausted"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(2)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_QUOTA_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_QUOTA_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn unauthorized_static_account_cools_down_and_rotates() {
    // A 401 classifies as RefreshRetry, but a token_env (static, non-refreshable)
    // account cannot be refreshed — it must be cooled down and the pool must
    // rotate to the next account rather than relaying the 401 to the client.
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "unauth-a"].concat();
    let token_b = ["fake-oauth-", "unauth-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_UNAUTH_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_UNAUTH_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":"account a token revoked"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(2)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_UNAUTH_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_UNAUTH_B", "uuid-b"),
    ))
    .await;

    // First request rotates off the 401'd account to the healthy one.
    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    // A session that hashes to account-a still lands on account-b because
    // account-a is now cooled down (so the upstream never sees a second a call).
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn plain_429_retries_the_same_account_without_rotating() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "throttle-a"].concat();
    let token_b = ["fake-oauth-", "throttle-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_THROTTLE_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_THROTTLE_B", &token_b);

    let upstream = MockServer::start().await;
    let error_body = r#"{"error":"temporary throttle on account a"}"#;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(error_body),
        )
        .expect(2)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_THROTTLE_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_THROTTLE_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    assert_eq!(response.text().await.unwrap(), error_body);
    upstream.verify().await;
}

#[tokio::test]
async fn exhausted_pool_relays_the_last_upstream_body_verbatim() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "exhaust-a"].concat();
    let token_b = ["fake-oauth-", "exhaust-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_EXHAUST_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_EXHAUST_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .insert_header("anthropic-ratelimit-unified-5h-status", "rejected")
                .set_body_string(r#"{"error":"first account exhausted"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let last_body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"recognizable final upstream body"}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .insert_header("anthropic-ratelimit-unified-7d-status", "rejected")
                .set_body_string(last_body),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_EXHAUST_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_EXHAUST_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.text().await.unwrap(), last_body);
    upstream.verify().await;
}

#[tokio::test]
async fn refresh_retry_refreshes_then_succeeds_on_401() {
    // A refreshable store account whose upstream returns 401 forces a token
    // refresh; the retry with the refreshed token then succeeds.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "refresh-stale"].concat();
    let fresh = ["fake-oauth-", "refresh-fresh"].concat();

    let accounts_dir = unique_temp_dir("succeeds");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "refresh-token-a",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"access_token":"{fresh}","expires_in":3600}}"#)),
        )
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"a"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_REFRESH_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn refresh_retry_non_success_rotates_to_next_account() {
    // If the refreshed retry still fails with a non-401/non-2xx status (5xx),
    // the pool must fail over to the next account instead of relaying it.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "rotate-stale"].concat();
    let fresh = ["fake-oauth-", "rotate-fresh"].concat();
    let token_b = ["fake-oauth-", "rotate-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_ROTATE_B", &token_b);

    let accounts_dir = unique_temp_dir("rotates");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "refresh-token-a",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"access_token":"{fresh}","expires_in":3600}}"#)),
        )
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"upstream down"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_ROTATE_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_ROTATE_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn unresolvable_account_cools_down_and_rotates() {
    // An account whose token_env is unset cannot be resolved: the pool must cool
    // it down and rotate to the next account rather than failing the request.
    if !can_bind_loopback() {
        return;
    }
    // account-a points at an env var that is never set; account-b is healthy.
    std::env::remove_var("SHUNT_TEST_MULTI_MISSING_A");
    let token_b = ["fake-oauth-", "resolve-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_RESOLVE_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_MISSING_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_RESOLVE_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_RESOLVE_B");
}

#[tokio::test]
async fn server_error_rotates_and_cools_down_the_failing_account() {
    // A 5xx classifies as Rotate (not the 429 sub-branch): the account is cooled
    // down for the fixed non-throttle window and the pool moves to the next one.
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "server-a"].concat();
    let token_b = ["fake-oauth-", "server-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_SERVER_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_SERVER_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(500).set_body_string(r#"{"error":"account a upstream error"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(2)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_SERVER_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_SERVER_B", "uuid-b"),
    ))
    .await;

    // First request rotates off the 500'd account to the healthy one.
    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    // A session that hashes to account-a still lands on account-b because
    // account-a is cooled down (the upstream never sees a second a call).
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_SERVER_A");
    std::env::remove_var("SHUNT_TEST_MULTI_SERVER_B");
}

#[tokio::test]
async fn refresh_retry_still_unauthorized_cools_down_and_rotates() {
    // Refresh succeeds but the refreshed token is still rejected with 401: the
    // account is genuinely broken, so it is cooled down and the pool rotates
    // rather than relaying the second 401 to the client.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "still401-stale"].concat();
    let fresh = ["fake-oauth-", "still401-fresh"].concat();
    let token_b = ["fake-oauth-", "still401-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_STILL401_B", &token_b);

    let accounts_dir = unique_temp_dir("still401");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "refresh-token-a",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"access_token":"{fresh}","expires_in":3600}}"#)),
        )
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"still revoked"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_STILL401_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_STILL401_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn all_accounts_unresolvable_returns_bad_gateway() {
    // When every account fails to resolve, the pool never reaches an upstream:
    // it surfaces a 502 and the upstream is never called.
    if !can_bind_loopback() {
        return;
    }
    std::env::remove_var("SHUNT_TEST_MULTI_MISSING_ALL_A");
    std::env::remove_var("SHUNT_TEST_MULTI_MISSING_ALL_B");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"unexpected":true}"#))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_MISSING_ALL_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_MISSING_ALL_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    upstream.verify().await;
}

#[tokio::test]
async fn pause_same_retry_succeeds_and_relays_without_rotating() {
    // A plain 429 (no quota header) pauses and retries the SAME account; when the
    // retry clears (200), that response is relayed and the account marked healthy.
    // The pool never rotates to account-b.
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "pauseok-a"].concat();
    let token_b = ["fake-oauth-", "pauseok-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_PAUSEOK_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_PAUSEOK_B", &token_b);

    let upstream = MockServer::start().await;
    // First call to account-a: a plain 429 (throttle). Higher priority + capped at
    // one response so the post-sleep retry falls through to the 200 mock below.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"transient throttle"}"#),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&upstream)
        .await;
    // The retry on the same account succeeds.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"a"}"#))
        .with_priority(2)
        .expect(1)
        .mount(&upstream)
        .await;
    // account-b must never be touched.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_PAUSEOK_A", "uuid-a"),
        account("account-b", "SHUNT_TEST_MULTI_PAUSEOK_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    assert_eq!(response.text().await.unwrap(), r#"{"account":"a"}"#);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_PAUSEOK_A");
    std::env::remove_var("SHUNT_TEST_MULTI_PAUSEOK_B");
}

/// Write a store account file marked as a long-lived, non-refreshable setup token
/// (`shuntCredentialKind: "setup_token"`, no refreshToken) with a far-future
/// expiry so its access token is used verbatim on the upstream POST.
fn write_setup_token_account(dir: &std::path::Path, name: &str, access: &str) {
    let body = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{access}","expiresAt":4102444800000,"shuntCredentialKind":"setup_token"}}}}"#
    );
    fs::write(dir.join(format!("{name}.json")), body).unwrap();
}

#[tokio::test]
async fn static_setup_token_account_cools_down_without_refreshing() {
    // A store account flagged as a setup token is non-refreshable: a 401 must cool
    // it down and rotate WITHOUT attempting a token refresh. This exercises
    // account_is_static_store_token()'s store path (vs. the token_env short-circuit
    // covered by unauthorized_static_account_cools_down_and_rotates).
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let setup = ["fake-oauth-", "setup-static"].concat();
    let token_b = ["fake-oauth-", "setupstatic-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_SETUPSTATIC_B", &token_b);

    let accounts_dir = unique_temp_dir("setupstatic");
    write_setup_token_account(&accounts_dir, "account-a", &setup);
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    // The refresh endpoint must never be called for a setup token.
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"access_token":"unexpected","expires_in":3600}"#),
        )
        .expect(0)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(setup.clone()))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":"revoked setup token"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_SETUPSTATIC_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
    // expect(0) on the refresh endpoint: a setup token is never refreshed.
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_SETUPSTATIC_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn duplicate_identity_alias_is_never_retried_as_a_separate_account() {
    // Two aliases sharing the same upstream identity (uuid) coalesce to a
    // single representative in `AccountPool::select_order`, so a failure on
    // the representative must rotate straight to a *distinct* identity — the
    // duplicate alias must never receive a request of its own, either as the
    // first pick or as a retry target.
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "dup-a"].concat();
    let token_a_dup = ["fake-oauth-", "dup-a-alias"].concat();
    let token_b = ["fake-oauth-", "dup-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_DUP_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_DUP_A_ALIAS", &token_a_dup);
    std::env::set_var("SHUNT_TEST_MULTI_DUP_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(500).set_body_string(r#"{"error":"account a upstream error"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    // The duplicate-identity alias must never be dialed: it is not a distinct
    // account in the pool's eyes, so it cannot serve as a retry target.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a_dup.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"a-dup"}"#))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config_accounts(
        &upstream.uri(),
        vec![
            account("account-a", "SHUNT_TEST_MULTI_DUP_A", "uuid-shared"),
            account(
                "account-a-alias",
                "SHUNT_TEST_MULTI_DUP_A_ALIAS",
                "uuid-shared",
            ),
            account("account-b", "SHUNT_TEST_MULTI_DUP_B", "uuid-b"),
        ],
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_DUP_A");
    std::env::remove_var("SHUNT_TEST_MULTI_DUP_A_ALIAS");
    std::env::remove_var("SHUNT_TEST_MULTI_DUP_B");
}

#[tokio::test]
async fn storm_control_spills_concurrent_request_to_next_account() {
    // Issue #195 storm control on the Anthropic pool loop: with
    // `ramp_initial_concurrency = 1`, a second concurrent request for the same
    // sticky account is denied admission (`try_admit` in `forward_claude_oauth`)
    // and spills to the next account instead of piling onto the first. Mirrors
    // the Codex suite's test of the same name — the two adapters wire the
    // admission gate independently, so each needs its own regression coverage.
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-oauth-", "storm-a"].concat();
    let token_b = ["fake-oauth-", "storm-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_STORM_A", &token_a);
    std::env::set_var("SHUNT_TEST_MULTI_STORM_B", &token_b);

    let upstream = MockServer::start().await;
    // Account-a's turn is slow, so it is still in flight (holding its single
    // admission slot) when the second request arrives.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(500))
                .set_body_string(r#"{"account":"a"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut config = test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_MULTI_STORM_A", "uuid-storm-a"),
        account("account-b", "SHUNT_TEST_MULTI_STORM_B", "uuid-storm-b"),
    );
    config.server.pool = Some(shunt::config::PoolConfig {
        ramp_initial_concurrency: Some(1),
        ..Default::default()
    });
    let gateway = start_gateway_with(config).await;

    // Both requests carry a session id that maps to account-a under the REAL
    // bucket assignment (`accounts::stable_session_index`: first 8 bytes of
    // SHA-256, big-endian, mod count) — this file's `session_id_for_account`
    // helper hashes with `DefaultHasher` and would land on the wrong account.
    let session_id = (0..1000)
        .map(|candidate| format!("session-{candidate}"))
        .find(|session_id| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(session_id.as_bytes());
            let prefix = u64::from_be_bytes(digest[..8].try_into().unwrap());
            prefix % 2 == 0
        })
        .expect("a session id should map to account-a");

    let first = {
        let gateway_url = gateway.base_url.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{gateway_url}/v1/messages"))
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", session_id)
                .body(
                    r#"{"model":"pooled-model","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
                )
                .send()
                .await
                .unwrap()
        })
    };
    // Give the first request time to reach the upstream and occupy the slot.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let second = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second.headers().get("x-shunt-account").unwrap(),
        "account-b",
        "a gated concurrent request should spill to the next account"
    );

    let first = first.await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers().get("x-shunt-account").unwrap(), "account-a");
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_STORM_A");
    std::env::remove_var("SHUNT_TEST_MULTI_STORM_B");
}

/// Asserts the forwarded body does *not* carry the Claude Code identity block.
///
/// Deliberately an absence check rather than an exact-body comparison: the pool
/// path also applies `normalize_upstream_model_request` and
/// `rewrite_account_uuid_request`, so pinning the whole body would couple this
/// test to mutations it is not about.
struct BodyLacksIdentity;

impl Match for BodyLacksIdentity {
    fn matches(&self, request: &Request) -> bool {
        !String::from_utf8_lossy(&request.body).contains(CLAUDE_CODE_IDENTITY)
    }
}

struct BodyCarriesIdentity;

impl Match for BodyCarriesIdentity {
    fn matches(&self, request: &Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(CLAUDE_CODE_IDENTITY)
    }
}

const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Post a body shaped like Claude Code's auto-mode permission classifier
/// request: the classifier prompt as the first `system` block, no identity
/// block. This is the one shape `auto_mode_classifier` repairs.
async fn post_classifier_request(gateway: &TestGateway) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "model": "pooled-model",
            "max_tokens": 64,
            "stop_sequences": ["</block>"],
            "system": [{
                "type": "text",
                "text": "You are a security monitor for autonomous AI coding agents.\n\n## Context\n\n…",
            }],
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn pool_classifier_request_on_a_subscription_oauth_account_gains_the_identity_block() {
    if !can_bind_loopback() {
        return;
    }
    let token = ["sk-ant-oat01-", "pool-classifier"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_CLASSIFIER_OAUTH", &token);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token.clone()))
        .and(BodyCarriesIdentity)
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config_accounts(
        &upstream.uri(),
        vec![account(
            "oauth-account",
            "SHUNT_TEST_MULTI_CLASSIFIER_OAUTH",
            "uuid-oauth",
        )],
    ))
    .await;

    assert_eq!(
        post_classifier_request(&gateway).await.status(),
        StatusCode::OK
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_CLASSIFIER_OAUTH");
}

#[tokio::test]
async fn pool_classifier_request_on_a_non_oauth_token_env_account_is_not_rewritten() {
    if !can_bind_loopback() {
        return;
    }
    // The pool resolves every account to `Credential::ClaudeOauth`, but the
    // `token_env` branch wraps whatever the variable holds without checking it
    // is a subscription token. An account pointed at an API key therefore faces
    // no client-shape gate upstream, and must keep its body unrewritten — this
    // is the invariant the per-candidate `bearer_is_subscription_oauth` check
    // enforces, and it is only reachable through `forward_claude_oauth`.
    let token = ["sk-ant-api03-", "pool-classifier"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_CLASSIFIER_APIKEY", &token);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token.clone()))
        .and(BodyLacksIdentity)
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config_accounts(
        &upstream.uri(),
        vec![account(
            "api-key-account",
            "SHUNT_TEST_MULTI_CLASSIFIER_APIKEY",
            "uuid-api-key",
        )],
    ))
    .await;

    assert_eq!(
        post_classifier_request(&gateway).await.status(),
        StatusCode::OK
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_CLASSIFIER_APIKEY");
}

#[tokio::test]
async fn classifier_gate_is_re_evaluated_for_each_candidate_during_rotation() {
    if !can_bind_loopback() {
        return;
    }
    // The gate lives inside the candidate loop precisely because a pool can mix
    // token shapes. A single-account pool only proves it runs; this proves it
    // runs *per candidate* — the api-key account must relay unrewritten, and the
    // oauth account it rotates to must carry the identity block, in one request.
    let api_key = ["sk-ant-api03-", "rotate-a"].concat();
    let oauth = ["sk-ant-oat01-", "rotate-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_CLASSIFIER_ROT_A", &api_key);
    std::env::set_var("SHUNT_TEST_MULTI_CLASSIFIER_ROT_B", &oauth);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(api_key.clone()))
        .and(BodyLacksIdentity)
        .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"error":"rotate"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(oauth.clone()))
        .and(BodyCarriesIdentity)
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account(
            "api-key-account",
            "SHUNT_TEST_MULTI_CLASSIFIER_ROT_A",
            "uuid-rot-a",
        ),
        account(
            "oauth-account",
            "SHUNT_TEST_MULTI_CLASSIFIER_ROT_B",
            "uuid-rot-b",
        ),
    ))
    .await;

    let response = post_classifier_request(&gateway).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "oauth-account"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_MULTI_CLASSIFIER_ROT_A");
    std::env::remove_var("SHUNT_TEST_MULTI_CLASSIFIER_ROT_B");
}

/// A dead refresh token (`invalid_grant`) must leave a **durable** mark on the
/// account, not just a 5-minute cooldown. Without it, the account cycles
/// forever: select → 401 → refresh rejected → 5-minute cooldown → expiry →
/// select again, at one 401 plus one already-rejected refresh POST every five
/// minutes, and the admin dashboard reports only "cooling", which is
/// indistinguishable from a quota pause that will clear on its own.
#[tokio::test]
async fn terminal_invalid_grant_marks_the_account_as_needing_relogin() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "terminal-stale"].concat();
    let token_b = ["fake-oauth-", "terminal-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_TERMINAL_B", &token_b);

    let accounts_dir = unique_temp_dir("terminalgrant");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "dead-refresh-token",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":"invalid_grant","error_description":"revoked"}"#),
        )
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let dead = store_account("account-a");
    let live = account("account-b", "SHUNT_TEST_MULTI_TERMINAL_B", "uuid-b");
    let (gateway, state) =
        start_gateway_with_state(test_config(&upstream.uri(), dead.clone(), live.clone())).await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    // `store_account` carries no uuid, so pool health keys it by store entry —
    // resolve it the same way the request path did.
    let dead_resolved = resolved_store_account("account-a");
    assert_pool_observed(&state, &dead_resolved);
    assert!(
        state.accounts.needs_relogin("anthropic", &dead_resolved),
        "a terminal invalid_grant must set the needs-re-login mark"
    );
    assert!(
        !state.accounts.needs_relogin("anthropic", &live),
        "the account that served the request must stay unmarked"
    );

    // Survival across a config re-snapshot is pinned separately, in
    // `server::tests::needs_relogin_mark_survives_a_config_re_snapshot`
    // (`AppState::refreshed` is crate-private).

    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_TERMINAL_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// The mirror of the test above, and the one that constrains the
/// implementation: a **transient** refresh failure (503) cools the account down
/// exactly as before but must leave no mark. Marking here would report a
/// perfectly healthy account as permanently dead after a momentary provider
/// outage — the failure mode that makes the whole signal untrustworthy.
#[tokio::test]
async fn transient_refresh_failure_does_not_mark_the_account() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "transient-stale"].concat();
    let token_b = ["fake-oauth-", "transient-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_TRANSIENT_B", &token_b);

    let accounts_dir = unique_temp_dir("transientgrant");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "live-refresh-token",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_TRANSIENT_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);

    let cooled = resolved_store_account("account-a");
    assert_pool_observed(&state, &cooled);
    assert!(
        !state.accounts.needs_relogin("anthropic", &cooled),
        "a 503 is transient: the account must cool down without being condemned"
    );
    // The cooldown itself is unchanged — this change adds a signal, it does not
    // alter routing. `snapshot` reports the same cooling account it always did.
    let snapshots = state
        .accounts
        .snapshot("anthropic", std::slice::from_ref(&cooled), None, None);
    assert!(
        snapshots[0].cooldown_secs_remaining.is_some(),
        "the transient failure must still cool the account down"
    );
    assert!(!snapshots[0].needs_relogin);

    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_TRANSIENT_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// A 401 on a credential that cannot be refreshed at all (a long-lived setup
/// token) is terminal by definition: there is no grant left to retry, so the
/// five-minute cooldown can only repeat the same 401 forever.
#[tokio::test]
async fn unrefreshable_setup_token_401_marks_the_account_as_needing_relogin() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let setup = ["fake-oauth-", "markstatic"].concat();
    let token_b = ["fake-oauth-", "markstatic-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_MARKSTATIC_B", &token_b);

    let accounts_dir = unique_temp_dir("markstatic");
    write_setup_token_account(&accounts_dir, "account-a", &setup);
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(setup.clone()))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":"revoked setup token"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_MARKSTATIC_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);

    let dead = resolved_store_account("account-a");
    assert_pool_observed(&state, &dead);
    assert!(
        state.accounts.needs_relogin("anthropic", &dead),
        "an unrefreshable credential's 401 is terminal and must be marked"
    );
    let snapshots = state
        .accounts
        .snapshot("anthropic", std::slice::from_ref(&dead), None, None);
    assert!(
        snapshots[0].needs_relogin,
        "the mark must reach the admin snapshot"
    );

    upstream.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_MARKSTATIC_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// The mark is cleared by `mark_healthy_scoped` — the exact call the relay
/// makes on a served response (`adapters/anthropic/mod.rs`) — so the flag
/// tracks the credential's current liveness rather than the fact that it once
/// failed. The 401 that sets the mark is driven end to end; the clear is
/// asserted at that seam rather than through a second served request, because
/// the failing account is left in a five-minute auth cooldown and the pool
/// would route the follow-up request to the healthy account instead.
#[tokio::test]
async fn mark_healthy_clears_the_needs_relogin_mark() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let setup = ["fake-oauth-", "clearmark"].concat();
    let token_b = ["fake-oauth-", "clearmark-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_CLEARMARK_B", &token_b);

    let accounts_dir = unique_temp_dir("clearmark");
    write_setup_token_account(&accounts_dir, "account-a", &setup);
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(setup.clone()))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":"revoked setup token"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_CLEARMARK_B", "uuid-b"),
    ))
    .await;

    assert_eq!(post_messages(&gateway, None).await.status(), StatusCode::OK);
    let dead = resolved_store_account("account-a");
    assert_pool_observed(&state, &dead);
    assert!(state.accounts.needs_relogin("anthropic", &dead));

    // The account answers a request again (an operator re-logged in out of
    // band). This is the relay's own success call, arguments included.
    state
        .accounts
        .mark_healthy_scoped("anthropic", &dead, true, false);
    assert!(
        !state.accounts.needs_relogin("anthropic", &dead),
        "a served response proves the credential is alive again"
    );

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_CLEARMARK_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// Write a refreshable store account whose access token is already **expired**,
/// so credential resolution refreshes it on read and the upstream is never
/// reached with it. This is the steady state of a dead account: its access
/// token outlives its usefulness by hours at most, after which every request
/// fails during resolution rather than on a 401.
fn write_expired_store_account(
    dir: &std::path::Path,
    name: &str,
    access: &str,
    refresh: &str,
    uuid: &str,
) {
    let body = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":1000}},"shuntAccountUuid":"{uuid}"}}"#
    );
    fs::write(dir.join(format!("{name}.json")), body).unwrap();
}

/// The 401 → force-refresh path is only reachable while the dead account's
/// *access* token is still inside its validity window. Once it expires — within
/// hours, and permanently thereafter — the refresh is rejected during
/// credential resolution instead, and the upstream is never called at all. That
/// path must mark the account too, or the dominant real-world shape of a dead
/// credential still cycles through the five-minute cooldown forever with
/// nothing durable on the dashboard.
#[tokio::test]
async fn terminal_invalid_grant_during_resolution_marks_the_account_as_needing_relogin() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let expired = ["fake-oauth-", "resolve-expired"].concat();
    let token_b = ["fake-oauth-", "resolve-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_RESOLVE_B", &token_b);

    let accounts_dir = unique_temp_dir("resolvegrant");
    write_expired_store_account(
        &accounts_dir,
        "account-a",
        &expired,
        "dead-refresh-token",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":"invalid_grant","error_description":"revoked"}"#),
        )
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    // The expired token never reaches the upstream: resolution fails first.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(expired.clone()))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_RESOLVE_B", "uuid-b"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    let dead = resolved_store_account("account-a");
    assert_pool_observed(&state, &dead);
    assert!(
        state.accounts.needs_relogin("anthropic", &dead),
        "a terminal invalid_grant seen during resolution must set the needs-re-login mark"
    );
    assert!(
        state
            .accounts
            .snapshot("anthropic", std::slice::from_ref(&dead), None, None)[0]
            .needs_relogin,
        "the mark must reach the /admin/pool snapshot"
    );

    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_RESOLVE_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// The mirror that constrains the implementation: a transient failure during
/// resolution (503) cools the account down exactly as before and must leave no
/// mark, or a momentary provider outage would condemn every healthy account
/// whose access token happened to be due for a refresh.
#[tokio::test]
async fn transient_resolution_failure_does_not_mark_the_account() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let expired = ["fake-oauth-", "resolve-transient"].concat();
    let token_b = ["fake-oauth-", "resolve-transient-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_RESOLVE_TRANSIENT_B", &token_b);

    let accounts_dir = unique_temp_dir("resolvetransient");
    write_expired_store_account(
        &accounts_dir,
        "account-a",
        &expired,
        "live-refresh-token",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account(
            "account-b",
            "SHUNT_TEST_MULTI_RESOLVE_TRANSIENT_B",
            "uuid-b",
        ),
    ))
    .await;

    assert_eq!(post_messages(&gateway, None).await.status(), StatusCode::OK);

    let cooled = resolved_store_account("account-a");
    assert_pool_observed(&state, &cooled);
    assert!(
        !state.accounts.needs_relogin("anthropic", &cooled),
        "a 503 during resolution is transient: the account must cool down without being condemned"
    );
    let snapshots = state
        .accounts
        .snapshot("anthropic", std::slice::from_ref(&cooled), None, None);
    assert!(
        snapshots[0].cooldown_secs_remaining.is_some(),
        "the transient failure must still cool the account down"
    );

    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_RESOLVE_TRANSIENT_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// The refresh grant succeeds but the bearer it produces is *still* rejected by
/// `/v1/messages`. The account is de-authorized upstream, not momentarily
/// unlucky, and the adapter already treats it as broken (a five-minute cooldown
/// plus rotation) — so it must carry the durable mark too. Without it the
/// account cycles through that cooldown forever, reported as plain `cooling`.
#[tokio::test]
async fn a_post_refresh_401_marks_the_account_as_needing_relogin() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "postrefresh-stale"].concat();
    let rotated = ["fake-oauth-", "postrefresh-rotated"].concat();
    let token_b = ["fake-oauth-", "postrefresh-b"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_POSTREFRESH_B", &token_b);

    let accounts_dir = unique_temp_dir("postrefresh");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "live-refresh-token",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    // The grant itself is healthy: it hands back a fresh access token.
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": rotated,
            "refresh_token": "live-refresh-token",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    // The rotated bearer is rejected too — this is the branch under test.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(rotated.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"account disabled"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"b"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_POSTREFRESH_B", "uuid-b"),
    ))
    .await;

    // The client is served by the healthy account: routing is unchanged.
    assert_eq!(post_messages(&gateway, None).await.status(), StatusCode::OK);

    let dead = resolved_store_account("account-a");
    assert_pool_observed(&state, &dead);
    assert!(
        state.accounts.needs_relogin("anthropic", &dead),
        "a refresh that succeeds into a still-rejected bearer must condemn the \
         account — otherwise it cools down and retries forever with no signal"
    );
    // The cooldown is still there and still independent of the mark.
    let snapshots = state
        .accounts
        .snapshot("anthropic", std::slice::from_ref(&dead), None, None);
    assert!(
        snapshots[0].cooldown_secs_remaining.is_some(),
        "the mark must be additive: the auth cooldown is unchanged"
    );
    assert!(snapshots[0].needs_relogin);

    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_POSTREFRESH_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

/// A refreshed retry that relays a non-401 4xx proves the bearer was accepted:
/// auth failures come back 401, so reaching a validation error means the
/// credential authenticated. A mark left from an earlier failure is stale and
/// must go — otherwise a working account stays branded dead until it happens to
/// serve a 2xx. The cooldown is deliberately left alone.
#[tokio::test]
async fn a_relayed_client_error_after_refresh_clears_a_stale_mark() {
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = ["fake-oauth-", "relayclear-stale"].concat();
    let rotated = ["fake-oauth-", "relayclear-rotated"].concat();
    std::env::set_var("SHUNT_TEST_MULTI_RELAYCLEAR_B", "unused");

    let accounts_dir = unique_temp_dir("relayclear");
    write_store_account(
        &accounts_dir,
        "account-a",
        &stale,
        "live-refresh-token",
        "uuid-a",
    );
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": rotated,
            "refresh_token": "rotated-refresh-token",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/oauth/token", auth.uri()),
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    // The refreshed bearer is accepted; the *request* is what the API rejects.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(rotated.clone()))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"bad request"}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let (gateway, state) = start_gateway_with_state(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_MULTI_RELAYCLEAR_B", "uuid-b"),
    ))
    .await;

    let live = resolved_store_account("account-a");
    // Stand in for an earlier failure that condemned the account.
    state.accounts.mark_needs_relogin("anthropic", &live);
    assert!(state.accounts.needs_relogin("anthropic", &live));

    // The 400 is relayed straight back — a client error is not the pool's to retry.
    assert_eq!(
        post_messages(&gateway, None).await.status(),
        StatusCode::BAD_REQUEST
    );

    assert!(
        !state.accounts.needs_relogin("anthropic", &live),
        "a relayed 400 proves the refreshed bearer authenticated, so the stale \
         needs-re-login mark must be cleared"
    );

    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_MULTI_RELAYCLEAR_B");
    fs::remove_dir_all(&accounts_dir).ok();
}
