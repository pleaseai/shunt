//! The sign-out against a loopback issuer. The mock is plain http, so the
//! tests turn `allow_http` on; the https/host rules are unit-tested directly.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use wiremock::{
    matchers::{body_string_contains, method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{accept_endpoint, run_with, Behavior, Endpoints, Outcome};
use crate::auth::chatgpt_plan::auth::ChatGptPlanAuthStore;
use crate::auth::chatgpt_plan::record::{AccountRecord, TokenBlock, RECORD_VERSION};
use crate::auth::chatgpt_plan::store;
use crate::auth::shared::EnvVarGuard;

const NAME: &str = "primary";
const CLIENT_ID: &str = "oaiapp_issued";

struct Env {
    // Drop order: the env var is restored before the lock is released.
    _env: EnvVarGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
    server: MockServer,
}

impl Env {
    async fn new(tag: &str) -> Self {
        let lock = crate::config::test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "shunt-siwc-logout-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let env = EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
        Self {
            _env: env,
            _lock: lock,
            dir,
            server: MockServer::start().await,
        }
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints {
            issuer: self.server.uri(),
        }
    }

    async fn discovery(&self, revocation_endpoint: &str) {
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"issuer": self.server.uri(), "revocation_endpoint": revocation_endpoint}),
            ))
            .mount(&self.server)
            .await;
    }

    async fn revoke_posts(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .count()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn behavior() -> Behavior {
    Behavior {
        attempts: 3,
        backoff: Duration::from_millis(1),
        allow_http: true,
        attempt_timeout: Duration::from_secs(10),
    }
}

fn signed_in() -> AccountRecord {
    AccountRecord {
        version: RECORD_VERSION,
        client_id: CLIENT_ID.into(),
        sub: Some("user-1".into()),
        email: Some("a@example.com".into()),
        scopes: vec!["openid".into()],
        tokens: Some(TokenBlock {
            access_token: "ACCESS-SECRET".into(),
            refresh_token: "REFRESH-SECRET".into(),
            id_token: Some("ID-SECRET".into()),
            expires_at: 4_000_000_000,
            earliest_refresh_at: None,
            last_refresh: Some(1),
        }),
    }
}

fn file_value() -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(store::account_path(NAME)).unwrap()).unwrap()
}

fn assert_cleared_locally() {
    assert_eq!(file_value(), json!({"version": 1, "client_id": CLIENT_ID}));
}

#[tokio::test]
async fn a_200_confirms_clears_tokens_and_keeps_registration_and_host_id() {
    let env = Env::new("ok").await;
    store::write_account(NAME, &signed_in()).unwrap();
    let host_id = store::host_id().unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(body_string_contains("token=REFRESH-SECRET"))
        .and(body_string_contains("token_type_hint=refresh_token"))
        .and(body_string_contains("client_id=oaiapp_issued"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&env.server)
        .await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::Confirmed { name: NAME.into() });
    assert!(outcome.render().contains("revocation confirmed"));
    assert_cleared_locally();
    assert_eq!(store::host_id().unwrap(), host_id);
    let secrets = outcome.render();
    assert!(!secrets.contains("SECRET"));
}

#[tokio::test]
async fn a_signed_out_gateway_view_needs_sign_in_after_logout() {
    let env = Env::new("gateway").await;
    store::write_account(NAME, &signed_in()).unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&env.server)
        .await;

    run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    // A running gateway reads the file per use; the next resolution fails.
    let error = ChatGptPlanAuthStore::new()
        .get_valid(NAME)
        .await
        .unwrap_err();
    assert!(error.detail.contains("signed out"), "{}", error.detail);
    assert!(error.terminal.is_some());
}

#[tokio::test]
async fn server_errors_retry_then_clear_locally_unconfirmed() {
    let env = Env::new("5xx").await;
    store::write_account(NAME, &signed_in()).unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&env.server)
        .await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    let text = outcome.render();
    assert!(text.contains("remote revocation not confirmed"), "{text}");
    assert!(text.contains("finish in ChatGPT settings"), "{text}");
    assert_eq!(env.revoke_posts().await, 3);
    assert_cleared_locally();
}

#[tokio::test]
async fn a_transient_5xx_then_200_is_confirmed() {
    let env = Env::new("flaky").await;
    store::write_account(NAME, &signed_in()).unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&env.server)
        .await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::Confirmed { name: NAME.into() });
    assert_eq!(env.revoke_posts().await, 2);
}

#[tokio::test]
async fn a_network_failure_clears_locally_unconfirmed() {
    let _env = Env::new("net").await;
    store::write_account(NAME, &signed_in()).unwrap();
    // Nothing listens on the issuer: take the mock down first.
    let dead = MockServer::start().await;
    let endpoints = Endpoints { issuer: dead.uri() };
    drop(dead);

    let outcome = run_with(NAME, &endpoints, &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    assert_cleared_locally();
}

#[tokio::test]
async fn a_4xx_is_not_retried_and_not_confirmed() {
    let env = Env::new("4xx").await;
    store::write_account(NAME, &signed_in()).unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&env.server)
        .await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    assert_eq!(env.revoke_posts().await, 1);
    assert_cleared_locally();
}

#[tokio::test]
async fn an_endpoint_off_the_issuer_host_is_never_posted_to() {
    let env = Env::new("offhost").await;
    store::write_account(NAME, &signed_in()).unwrap();
    // Same server, different host name: `localhost` vs the issuer's 127.0.0.1.
    let port = env.server.address().port();
    env.discovery(&format!("http://localhost:{port}/revoke"))
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&env.server)
        .await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    assert_eq!(env.revoke_posts().await, 0);
    assert_cleared_locally();
}

#[tokio::test]
async fn an_http_endpoint_is_refused_in_production_behavior() {
    let env = Env::new("http").await;
    store::write_account(NAME, &signed_in()).unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&env.server)
        .await;
    let strict = Behavior {
        allow_http: false,
        ..behavior()
    };

    let outcome = run_with(NAME, &env.endpoints(), &strict).await.unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    assert_eq!(env.revoke_posts().await, 0);
    assert_cleared_locally();
}

#[test]
fn endpoint_rules_require_https_on_the_issuer_host() {
    let issuer = "https://auth.openai.com";
    let ok = |endpoint: &str| accept_endpoint(endpoint, issuer, false).is_some();
    assert!(ok("https://auth.openai.com/api/accounts/oauth/revoke"));
    assert!(!ok("http://auth.openai.com/revoke"));
    assert!(!ok("https://evil.example/revoke"));
    assert!(!ok("https://auth.openai.com.evil.example/revoke"));
    assert!(!ok("https://user:pw@auth.openai.com/revoke"));
    assert!(!ok("not a url"));
}

#[tokio::test]
async fn an_already_signed_out_account_skips_revocation() {
    let env = Env::new("already").await;
    store::write_account(NAME, &AccountRecord::signed_out(CLIENT_ID)).unwrap();

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::AlreadySignedOut { name: NAME.into() });
    assert!(outcome.render().contains("already signed out"));
    assert!(env.server.received_requests().await.unwrap().is_empty());
    assert_cleared_locally();
}

#[tokio::test]
async fn a_missing_account_is_an_error_naming_it() {
    let env = Env::new("missing").await;

    let error = run_with("ghost", &env.endpoints(), &behavior())
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("ghost"), "{error:#}");
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

/// Rewrites the account file as the revocation arrives, as a gateway refresh
/// or a second sign-out landing between logout's read and its clear would.
struct WriteThenOk(fn() -> AccountRecord);

impl wiremock::Respond for WriteThenOk {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        store::write_account(NAME, &(self.0)()).unwrap();
        ResponseTemplate::new(200)
    }
}

fn rotated() -> AccountRecord {
    let mut record = signed_in();
    record.tokens.as_mut().unwrap().refresh_token = "REFRESH-ROTATED".into();
    record
}

async fn rotate_on_first_revocation(env: &Env, rotated_status: u16) {
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(body_string_contains("token=REFRESH-SECRET"))
        .respond_with(WriteThenOk(rotated))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(body_string_contains("token=REFRESH-ROTATED"))
        .and(body_string_contains("client_id=oaiapp_issued"))
        .respond_with(ResponseTemplate::new(rotated_status))
        .expect(1)
        .mount(&env.server)
        .await;
}

#[tokio::test]
async fn a_token_rotated_during_revocation_is_revoked_too() {
    let env = Env::new("rotated").await;
    store::write_account(NAME, &signed_in()).unwrap();
    rotate_on_first_revocation(&env, 200).await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::Confirmed { name: NAME.into() });
    assert_cleared_locally();
}

#[tokio::test]
async fn a_rotated_token_whose_revocation_fails_is_not_reported_confirmed() {
    let env = Env::new("rotated-fail").await;
    store::write_account(NAME, &signed_in()).unwrap();
    rotate_on_first_revocation(&env, 400).await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    assert_cleared_locally();
}

#[tokio::test]
async fn an_account_signed_out_concurrently_still_confirms() {
    let env = Env::new("concurrent").await;
    store::write_account(NAME, &signed_in()).unwrap();
    env.discovery(&format!("{}/revoke", env.server.uri())).await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .respond_with(WriteThenOk(|| AccountRecord::signed_out(CLIENT_ID)))
        .expect(1)
        .mount(&env.server)
        .await;

    let outcome = run_with(NAME, &env.endpoints(), &behavior()).await.unwrap();

    assert_eq!(outcome, Outcome::Confirmed { name: NAME.into() });
    assert_cleared_locally();
}

#[tokio::test]
async fn a_stalled_issuer_times_out_and_still_clears_locally() {
    let env = Env::new("stall").await;
    store::write_account(NAME, &signed_in()).unwrap();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&env.server)
        .await;
    let quick = Behavior {
        attempts: 2,
        attempt_timeout: Duration::from_millis(100),
        ..behavior()
    };

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        run_with(NAME, &env.endpoints(), &quick),
    )
    .await
    .expect("logout is bounded")
    .unwrap();

    assert_eq!(outcome, Outcome::NotConfirmed { name: NAME.into() });
    assert_cleared_locally();
}
