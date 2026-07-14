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
    let mut config = Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.base_url = upstream_base_url.to_string();
    provider.auth = AuthMode::ClaudeOauth;
    provider.accounts = vec![first, second];
    config.routes.push(RouteConfig {
        model: "pooled-model".to_string(),
        provider: "anthropic".to_string(),
        upstream_model: None,
        effort: None,
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
