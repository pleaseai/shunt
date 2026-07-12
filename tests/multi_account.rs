use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::ErrorKind,
    net::SocketAddr,
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
        credentials: None,
        token_env: Some(token_env.to_string()),
        uuid: Some(uuid.to_string()),
    }
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
    let (app, _shared) = server::build_router(config).unwrap();
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
