//! Multi-account pool and failover tests for `auth = "kimi_oauth"`, mirroring
//! `tests/multi_account.rs`'s Claude coverage but scoped to the Kimi path:
//! no account-UUID rewrite, no `anthropic-beta` header, and every non-`Relay`
//! classification (401, quota 429, 5xx) collapses to cooldown-and-rotate
//! rather than Claude's fuller Rotate/PauseSame/RefreshRetry split (see
//! `forward_kimi_oauth`'s doc comment for why).

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use sha2::{Digest, Sha256};
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
            == Some(format!("Bearer {}", self.0).as_str())
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

fn account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        // Kimi has no verified upstream account identifier (see
        // `kimi::store::scan_accounts`), so pool identity always falls back
        // to `name` here — no `uuid` is set.
        ..Default::default()
    }
}

fn disabled_account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        disabled: true,
        ..account(name, token_env)
    }
}

fn test_config(upstream_base_url: &str, accounts: Vec<AccountConfig>) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.base_url = upstream_base_url.to_string();
    provider.auth = AuthMode::KimiOauth;
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

/// Brute-force a session id that `accounts::stable_session_index` (SHA-256
/// prefix mod account count) maps to the requested pool slot, so a test can
/// deliberately target a specific account via sticky routing.
fn session_id_for_account(index: usize, account_count: usize) -> String {
    (0..1000)
        .map(|candidate| format!("session-{candidate}"))
        .find(|session_id| {
            let digest = Sha256::digest(session_id.as_bytes());
            let prefix = u64::from_be_bytes(digest[..8].try_into().unwrap());
            (prefix % account_count as u64) as usize == index
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

/// Pool selection across two enabled accounts: two sequential requests with
/// no session id advance the provider's round-robin counter, so the first
/// request lands on account-a and the second on account-b — each carrying
/// only its own account's bearer token — proving `select_order` actually
/// spreads load across the Kimi pool rather than always picking the first
/// entry.
#[tokio::test]
async fn pool_selects_across_two_accounts() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-kimi-", "select-a"].concat();
    let token_b = ["fake-kimi-", "select-b"].concat();
    std::env::set_var("SHUNT_TEST_KIMI_SELECT_A", &token_a);
    std::env::set_var("SHUNT_TEST_KIMI_SELECT_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"a"}"#))
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
        vec![
            account("account-a", "SHUNT_TEST_KIMI_SELECT_A"),
            account("account-b", "SHUNT_TEST_KIMI_SELECT_B"),
        ],
    ))
    .await;

    // No session id: each request advances the provider's round-robin
    // counter, so the first lands on account-a and the second on account-b.
    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_KIMI_SELECT_A");
    std::env::remove_var("SHUNT_TEST_KIMI_SELECT_B");
}

/// A 401 from the first account rotates to the next candidate rather than
/// relaying the 401 to the client: `KimiAuthStore` has no forced-refresh
/// entry point, so `forward_kimi_oauth` treats `RefreshRetry` the same as
/// `Rotate` (see its doc comment) — cool the account down and try the next.
#[tokio::test]
async fn pool_rotates_to_next_account_on_401() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-kimi-", "unauth-a"].concat();
    let token_b = ["fake-kimi-", "unauth-b"].concat();
    std::env::set_var("SHUNT_TEST_KIMI_UNAUTH_A", &token_a);
    std::env::set_var("SHUNT_TEST_KIMI_UNAUTH_B", &token_b);

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
        vec![
            account("account-a", "SHUNT_TEST_KIMI_UNAUTH_A"),
            account("account-b", "SHUNT_TEST_KIMI_UNAUTH_B"),
        ],
    ))
    .await;

    // First request rotates off the 401'd account to the healthy one.
    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    // A session that hashes to account-a still lands on account-b, because
    // account-a is now cooling down (so the upstream never sees a second
    // account-a call at all).
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_KIMI_UNAUTH_A");
    std::env::remove_var("SHUNT_TEST_KIMI_UNAUTH_B");
}

/// Every configured account `disabled = true` must error clearly rather than
/// falling through to the generic "all accounts failed" upstream-exhaustion
/// message, which would misdirect an operator who disabled every account by
/// mistake — the upstream mock expects zero calls, proving the error is
/// raised before any account is even selected.
#[tokio::test]
async fn all_disabled_pool_errors_clearly() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-kimi-", "alldis-a"].concat();
    let token_b = ["fake-kimi-", "alldis-b"].concat();
    std::env::set_var("SHUNT_TEST_KIMI_ALLDIS_A", &token_a);
    std::env::set_var("SHUNT_TEST_KIMI_ALLDIS_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"account":"unexpected"}"#))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            disabled_account("account-a", "SHUNT_TEST_KIMI_ALLDIS_A"),
            disabled_account("account-b", "SHUNT_TEST_KIMI_ALLDIS_B"),
        ],
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("disabled"),
        "expected the error to explain every account is disabled, got: {body}"
    );

    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_KIMI_ALLDIS_A");
    std::env::remove_var("SHUNT_TEST_KIMI_ALLDIS_B");
}

/// A `disabled = true` account is skipped by `select_order`: every request,
/// including one whose session id hashes to the disabled account's slot,
/// lands on the one enabled account, and the upstream mock for the disabled
/// account's token expects zero calls.
#[tokio::test]
async fn disabled_account_is_skipped_in_favor_of_the_enabled_one() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = ["fake-kimi-", "skip-a"].concat();
    let token_b = ["fake-kimi-", "skip-b"].concat();
    std::env::set_var("SHUNT_TEST_KIMI_SKIP_A", &token_a);
    std::env::set_var("SHUNT_TEST_KIMI_SKIP_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"account":"a-should-not-be-called"}"#),
        )
        .expect(0)
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
        vec![
            disabled_account("account-a", "SHUNT_TEST_KIMI_SKIP_A"),
            account("account-b", "SHUNT_TEST_KIMI_SKIP_B"),
        ],
    ))
    .await;

    // A session id that would hash to account-a's slot in a fully-enabled
    // two-account pool still lands on account-b, since account-a never
    // enters the selectable order at all.
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_KIMI_SKIP_A");
    std::env::remove_var("SHUNT_TEST_KIMI_SKIP_B");
}

/// An account whose `token_env` is unset cannot be resolved by
/// `resolve_kimi_account`: `forward_kimi_oauth`'s `Err` arm at the
/// credential-resolve step must arm a cooldown and `continue` to the next
/// candidate rather than failing the request, mirroring
/// `unresolvable_account_cools_down_and_rotates` in `tests/multi_account.rs`.
#[tokio::test]
async fn unresolvable_account_cools_down_and_rotates() {
    if !can_bind_loopback() {
        return;
    }
    // account-a points at an env var that is never set; account-b is healthy.
    std::env::remove_var("SHUNT_TEST_KIMI_MISSING_A");
    let token_b = ["fake-kimi-", "resolve-b"].concat();
    std::env::set_var("SHUNT_TEST_KIMI_RESOLVE_B", &token_b);

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
        vec![
            account("account-a", "SHUNT_TEST_KIMI_MISSING_A"),
            account("account-b", "SHUNT_TEST_KIMI_RESOLVE_B"),
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

    std::env::remove_var("SHUNT_TEST_KIMI_RESOLVE_B");
}

/// When every account fails to resolve, the pool never reaches an upstream:
/// `last_response` stays `None` and `forward_kimi_oauth` surfaces a 502
/// without ever calling the mock, exercising the currently-unreached
/// `last_response == None` arm. Mirrors
/// `all_accounts_unresolvable_returns_bad_gateway` in `tests/multi_account.rs`.
#[tokio::test]
async fn all_accounts_unresolvable_returns_bad_gateway() {
    if !can_bind_loopback() {
        return;
    }
    std::env::remove_var("SHUNT_TEST_KIMI_MISSING_ALL_A");
    std::env::remove_var("SHUNT_TEST_KIMI_MISSING_ALL_B");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"unexpected":true}"#))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            account("account-a", "SHUNT_TEST_KIMI_MISSING_ALL_A"),
            account("account-b", "SHUNT_TEST_KIMI_MISSING_ALL_B"),
        ],
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    upstream.verify().await;
}
