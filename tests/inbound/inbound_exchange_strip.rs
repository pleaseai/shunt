//! A shunt-issued workload exchange token never reaches an upstream, whether
//! it is valid, expired, signed under a rotated-out secret, orphaned by a
//! reload, or presented to a deployment that has no `[server.auth]` at all.
//!
//! Every token comes from the real `POST /v1/oauth/token`. The upstream is a
//! mock that records what it receives, and each case asserts the mock *did*
//! receive the request (and the caller's own `x-api-key`), so an absent
//! credential cannot be an artefact of the request never leaving.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{AuthMode, Config, ExchangeConfig, InboundAuthConfig},
    reload::{RuntimeState, SharedState},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::common::{self, codex_jwt, jwt};

const GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const BETA: &str = "oauth-2025-04-20";
const STATIC_ENV: &str = "SHUNT_TEST_EXCHANGE_STRIP_STATIC";
const SECRET: &str = "exchange-strip-secret-0123456789abcdef";
const OTHER_SECRET: &str = "exchange-strip-other-9876543210fedcba";
const GENUINE_KEY: &str = "sk-ant-genuine-caller-key";
const BODY: &str =
    r#"{"model":"upstream-billed","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;

struct Server {
    base_url: String,
    shared: SharedState,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(mut config: Config) -> Server {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        base_url: format!("http://{addr}"),
        shared,
        task,
    }
}

async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "model": "upstream-billed",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}})))
        .mount(&server)
        .await;
    server
}

/// `exchange = None` leaves the entry plain (no opt-in), which is what an
/// orphaning reload looks like.
fn auth(issuer: &str, secret: Option<&str>, max_ttl: u64) -> InboundAuthConfig {
    let mut auth = codex_jwt::auth(issuer, &format!("{issuer}/jwks"), None, None);
    auth.tokens_env = STATIC_ENV.to_string();
    auth.jwt[0].exchange = secret.is_some();
    auth.exchange = secret.map(|secret| {
        serde_json::from_value::<ExchangeConfig>(
            json!({"signing_secret": secret, "max_ttl_seconds": max_ttl}),
        )
        .unwrap()
    });
    auth
}

fn config(upstream: &MockServer, auth: Option<InboundAuthConfig>) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.base_url = upstream.uri();
    provider.auth = AuthMode::Passthrough;
    config.server.auth = auth;
    config
}

async fn issue(server: &Server, assertion: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/oauth/token", server.base_url))
        .header("anthropic-beta", BETA)
        .json(&json!({"grant_type": GRANT, "assertion": assertion}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

/// Send `/v1/messages` carrying `token` as the bearer and the caller's own key
/// in `x-api-key`, then assert on what the upstream mock received.
async fn assert_token_not_forwarded(
    server: &Server,
    upstream: &MockServer,
    token: &str,
    extra: &[(&str, &str)],
) {
    let before = upstream.received_requests().await.unwrap().len();
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", server.base_url))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("authorization", format!("Bearer {token}"))
        .header("x-api-key", GENUINE_KEY);
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = request.body(BODY).send().await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        response.text().await
    );

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        before + 1,
        "the request must reach the mock"
    );
    let request = received.last().unwrap();
    for (name, value) in request.headers.iter() {
        assert!(
            !value
                .as_bytes()
                .windows(token.len())
                .any(|w| w == token.as_bytes()),
            "the exchange token leaked upstream in `{name}`"
        );
    }
    assert!(request.headers.get("authorization").is_none());
    assert_eq!(
        request.headers.get("x-api-key").map(|v| v.as_bytes()),
        Some(GENUINE_KEY.as_bytes()),
        "the caller's own credential in the other slot must still arrive"
    );
}

struct Fixture {
    issuer: MockServer,
    upstream: MockServer,
    issuing: Server,
    _env: common::EnvVars,
}

async fn fixture(secret: &str, max_ttl: u64) -> Fixture {
    let env = common::set_env(&[(STATIC_ENV, "bob:static-token")]).await;
    let issuer = jwt::issuer().await;
    let upstream = upstream().await;
    let issuing = start(config(
        &upstream,
        Some(auth(&issuer.uri(), Some(secret), max_ttl)),
    ))
    .await;
    Fixture {
        issuer,
        upstream,
        issuing,
        _env: env,
    }
}

impl Fixture {
    async fn token(&self) -> String {
        let assertion = codex_jwt::jwt_for(&self.issuer.uri(), "alice", 300);
        issue(&self.issuing, &assertion).await
    }
}

#[tokio::test]
async fn a_valid_token_authenticating_a_gated_route_is_not_forwarded() {
    let f = fixture(SECRET, 600).await;
    let token = f.token().await;
    assert_token_not_forwarded(&f.issuing, &f.upstream, &token, &[("anthropic-beta", BETA)]).await;
}

#[tokio::test]
async fn a_token_from_another_instance_is_not_forwarded_when_no_auth_is_configured() {
    let f = fixture(SECRET, 600).await;
    let token = f.token().await;
    // The deployment the token is presented to has no `[server.auth]` at all.
    let bare = start(config(&f.upstream, None)).await;
    assert_token_not_forwarded(&bare, &f.upstream, &token, &[]).await;

    // Non-vacuity: a genuine bearer in the same slot does reach the upstream.
    let before = f.upstream.received_requests().await.unwrap().len();
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", bare.base_url))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("authorization", "Bearer sk-ant-oat-genuine-bearer")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let received = f.upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), before + 1);
    assert_eq!(
        received
            .last()
            .unwrap()
            .headers
            .get("authorization")
            .unwrap()
            .as_bytes(),
        b"Bearer sk-ant-oat-genuine-bearer"
    );
}

#[tokio::test]
async fn a_token_signed_under_a_rotated_out_secret_is_not_forwarded() {
    let f = fixture(SECRET, 600).await;
    let token = f.token().await;
    let rotated = start(config(
        &f.upstream,
        Some(auth(&f.issuer.uri(), Some(OTHER_SECRET), 600)),
    ))
    .await;
    // The static token authenticates the request; the stale exchange token
    // rides along in `Authorization`.
    assert_token_not_forwarded(
        &rotated,
        &f.upstream,
        &token,
        &[("x-shunt-token", "static-token")],
    )
    .await;
}

#[tokio::test]
async fn a_token_orphaned_by_a_reload_is_not_forwarded() {
    let f = fixture(SECRET, 600).await;
    let token = f.token().await;
    f.issuing.shared.store(Arc::new(
        RuntimeState::from_config(config(&f.upstream, Some(auth(&f.issuer.uri(), None, 600))))
            .unwrap(),
    ));
    assert_token_not_forwarded(
        &f.issuing,
        &f.upstream,
        &token,
        &[("x-shunt-token", "static-token")],
    )
    .await;
}

#[tokio::test]
async fn an_expired_token_is_not_forwarded() {
    let f = fixture(SECRET, 1).await;
    let token = f.token().await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_token_not_forwarded(
        &f.issuing,
        &f.upstream,
        &token,
        &[("x-shunt-token", "static-token")],
    )
    .await;
}
