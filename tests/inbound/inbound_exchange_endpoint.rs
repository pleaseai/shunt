//! `POST /v1/oauth/token` (workload token exchange), end to end: a real router,
//! a loopback JWKS issuer and GitHub Actions-shaped assertions.
//!
//! The request body is the one the AWF api-proxy sidecar sends. These tests pin
//! what the sidecar sees (`200` with a token, or one of the FR-10 OAuth
//! errors) and the properties the operator relies on: failures are
//! indistinguishable, the answer is never cacheable, and the route is gone
//! when the table is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{Config, ExchangeConfig, InboundAuthConfig, InboundJwtConfig, StringList},
    reload::{RuntimeState, SharedState},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::common::{self, jwt};

const GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const BETA: &str = "oauth-2025-04-20,oidc-federation-2026-04-01";
/// Differs from the gateway secret used elsewhere in the suite (FR-12).
const SECRET: &str = "exchange-signing-secret-0123456789abcdef";
const MAX_TTL: u64 = 900;
const REPOSITORY: &str = "pleaseai/shunt";

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

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

impl Server {
    async fn post_raw(&self, content_type: Option<&str>, body: &str) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/oauth/token", self.base_url))
            .header("anthropic-beta", BETA);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        request.body(body.to_string()).send().await.unwrap()
    }

    async fn post(&self, body: &Value) -> reqwest::Response {
        self.post_raw(Some("application/json"), &body.to_string())
            .await
    }

    async fn exchange(&self, assertion: &str) -> reqwest::Response {
        self.post(&sidecar_body(assertion)).await
    }
}

/// The exact body the AWF sidecar posts.
fn sidecar_body(assertion: &str) -> Value {
    json!({
        "grant_type": GRANT,
        "assertion": assertion,
        "federation_rule_id": "fdrl_0123",
        "organization_id": "00000000-0000-0000-0000-000000000000",
        "service_account_id": "svac_0123",
        "workspace_id": "wrkspc_0123",
    })
}

fn github_claims(issuer: &str, lifetime: u64) -> Value {
    let mut claims = jwt::claims(issuer, "unused", lifetime);
    let map = claims.as_object_mut().unwrap();
    map.remove("preferred_username");
    map.remove("tid");
    map.insert(
        "sub".into(),
        json!(format!("repo:{REPOSITORY}:ref:refs/heads/main")),
    );
    map.insert("repository".into(), json!(REPOSITORY));
    map.insert("repository_owner".into(), json!("pleaseai"));
    claims
}

fn entry(issuer: &str, audience: &str, exchange: bool) -> InboundJwtConfig {
    InboundJwtConfig {
        issuer: issuer.to_string(),
        audience: StringList::One(audience.to_string()),
        email_domains: Vec::new(),
        allowed_emails: Vec::new(),
        require: [(
            "repository_owner".to_string(),
            StringList::One("pleaseai".to_string()),
        )]
        .into(),
        subject_prefix: StringList::One(format!("repo:{REPOSITORY}:")),
        identity_claim: Some("repository".to_string()),
        algorithms: vec!["RS256".to_string()],
        authorized_parties: Vec::new(),
        clock_skew_seconds: 0,
        max_token_age_seconds: 3600,
        jwks_url: Some(format!("{issuer}/jwks")),
        exchange,
    }
}

fn exchange_config() -> ExchangeConfig {
    serde_json::from_value(json!({"signing_secret": SECRET, "max_ttl_seconds": MAX_TTL})).unwrap()
}

fn config_with(entries: Vec<InboundJwtConfig>, exchange: Option<ExchangeConfig>) -> Config {
    let mut config = Config::default();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_EXCHANGE_NO_TOKENS".to_string(),
        jwt: entries,
        exchange,
        ..Default::default()
    });
    config
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

/// One opted-in entry on a live issuer.
async fn standard() -> (Server, MockServer, common::EnvVars) {
    let env = common::env_lock().await;
    let issuer = jwt::issuer().await;
    let uri = issuer.uri();
    let server = start(config_with(
        vec![entry(&uri, jwt::AUDIENCE, true)],
        Some(exchange_config()),
    ))
    .await;
    (server, issuer, env)
}

async fn json_of(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    assert_eq!(
        response.headers()["cache-control"],
        "no-store",
        "every answer from the endpoint is uncacheable"
    );
    (status, response.json().await.unwrap())
}

async fn error_of(response: reqwest::Response) -> (StatusCode, String, Vec<u8>) {
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.bytes().await.unwrap().to_vec();
    let error = serde_json::from_slice::<Value>(&bytes).unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    (status, error, bytes)
}

#[tokio::test]
async fn the_sidecar_body_yields_a_bearer_token_for_the_full_max_ttl() {
    let (server, issuer, _env) = standard().await;
    let assertion = jwt::token(&github_claims(&issuer.uri(), 300));

    let (status, body) = json_of(server.exchange(&assertion).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], MAX_TTL);
    let token = body["access_token"].as_str().unwrap();
    assert!(!token.is_empty());
    assert_ne!(
        token, assertion,
        "the issued token is shunt's, not the assertion"
    );
}

#[tokio::test]
async fn an_assertion_close_to_expiry_still_gets_the_full_max_ttl() {
    let (server, issuer, _env) = standard().await;
    let assertion = jwt::token(&github_claims(&issuer.uri(), 30));

    let (status, body) = json_of(server.exchange(&assertion).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expires_in"], MAX_TTL);
}

#[tokio::test]
async fn the_federation_ids_are_ignored_whether_present_or_absent() {
    let (server, issuer, _env) = standard().await;
    let assertion = jwt::token(&github_claims(&issuer.uri(), 300));

    let (status, body) = json_of(
        server
            .post(&json!({"grant_type": GRANT, "assertion": assertion}))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expires_in"], MAX_TTL);

    // Unknown fields, and federation ids of any JSON type, change nothing.
    let (status, _) = json_of(
        server
            .post(&json!({
                "grant_type": GRANT,
                "assertion": assertion,
                "federation_rule_id": 7,
                "workspace_id": null,
                "extra": {"nested": true},
            }))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn surrounding_whitespace_on_the_assertion_is_trimmed() {
    let (server, issuer, _env) = standard().await;
    let assertion = jwt::token(&github_claims(&issuer.uri(), 300));

    let (status, _) = json_of(server.exchange(&format!("  {assertion}\n")).await).await;

    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_content_type_with_parameters_or_a_json_suffix_is_accepted() {
    let (server, issuer, _env) = standard().await;
    let body = sidecar_body(&jwt::token(&github_claims(&issuer.uri(), 300))).to_string();

    for content_type in [
        "application/json; charset=utf-8",
        "application/vnd.api+json",
    ] {
        let (status, _) = json_of(server.post_raw(Some(content_type), &body).await).await;
        assert_eq!(status, StatusCode::OK, "{content_type}");
    }
}

#[tokio::test]
async fn every_verification_failure_returns_one_byte_identical_401() {
    let (server, issuer, _env) = standard().await;
    let uri = issuer.uri();
    let mut wrong_audience = github_claims(&uri, 300);
    wrong_audience["aud"] = json!("someone-else");
    let mut expired = github_claims(&uri, 300);
    expired["exp"] = json!(1_000);
    expired["iat"] = json!(500);
    let mut too_old = github_claims(&uri, 300);
    too_old["iat"] = json!(unix_now().saturating_sub(7_200));
    let mut missing_claim = github_claims(&uri, 300);
    missing_claim
        .as_object_mut()
        .unwrap()
        .remove("repository_owner");
    let mut wrong_subject = github_claims(&uri, 300);
    wrong_subject["sub"] = json!("repo:other/repo:ref:refs/heads/main");
    let mut unknown_issuer = github_claims(&uri, 300);
    unknown_issuer["iss"] = json!("https://unknown.example");
    let mut bad_signature = jwt::token(&github_claims(&uri, 300));
    bad_signature.push_str("AAAA");

    let mut bodies = Vec::new();
    for assertion in [
        jwt::token(&wrong_audience),
        jwt::token(&expired),
        jwt::token(&too_old),
        jwt::token(&missing_claim),
        jwt::token(&wrong_subject),
        jwt::token(&unknown_issuer),
        bad_signature,
        "not-a-jwt".to_string(),
    ] {
        let (status, error, bytes) = error_of(server.exchange(&assertion).await).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(error, "invalid_grant");
        bodies.push(bytes);
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "the 401 body must not reveal which check failed: {:?}",
        bodies
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_entry_without_exchange_does_not_exchange() {
    let env = common::env_lock().await;
    let issuer = jwt::issuer().await;
    let uri = issuer.uri();
    // The only verifying entry has not opted in; a second, unrelated one has
    // (FR-12 needs at least one) but sits on another issuer.
    let other = jwt::issuer().await;
    let server = start(config_with(
        vec![
            entry(&uri, jwt::AUDIENCE, false),
            entry(&other.uri(), jwt::AUDIENCE, true),
        ],
        Some(exchange_config()),
    ))
    .await;
    let assertion = jwt::token(&github_claims(&uri, 300));

    let (status, error, _) = error_of(server.exchange(&assertion).await).await;

    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNAUTHORIZED, "invalid_grant")
    );
    drop(env);
}

#[tokio::test]
async fn only_the_opted_in_entry_of_a_shared_issuer_is_tried() {
    let env = common::env_lock().await;
    let issuer = jwt::issuer().await;
    let uri = issuer.uri();
    // Two entries on one issuer with different audiences; only the second opts
    // in and it is the one that verifies the assertion.
    let server = start(config_with(
        vec![
            entry(&uri, "first-audience", false),
            entry(&uri, jwt::AUDIENCE, true),
        ],
        Some(exchange_config()),
    ))
    .await;
    let assertion = jwt::token(&github_claims(&uri, 300));

    let (status, body) = json_of(server.exchange(&assertion).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expires_in"], MAX_TTL);
    drop(env);
}

#[tokio::test]
async fn a_shared_issuer_entry_that_did_not_opt_in_is_not_a_second_chance() {
    let env = common::env_lock().await;
    let issuer = jwt::issuer().await;
    let uri = issuer.uri();
    // The assertion verifies against the first (not opted in) entry only.
    let server = start(config_with(
        vec![
            entry(&uri, jwt::AUDIENCE, false),
            entry(&uri, "second-audience", true),
        ],
        Some(exchange_config()),
    ))
    .await;
    let assertion = jwt::token(&github_claims(&uri, 300));

    let (status, error, _) = error_of(server.exchange(&assertion).await).await;

    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNAUTHORIZED, "invalid_grant")
    );
    drop(env);
}

#[tokio::test]
async fn a_static_client_token_and_an_issued_token_are_not_assertions() {
    let mut env = common::env_lock().await;
    env.set(
        "SHUNT_TEST_EXCHANGE_STATIC_TOKENS",
        "ci:static-client-token",
    );
    let issuer = jwt::issuer().await;
    let uri = issuer.uri();
    let mut config = config_with(
        vec![entry(&uri, jwt::AUDIENCE, true)],
        Some(exchange_config()),
    );
    config.server.auth.as_mut().unwrap().tokens_env =
        "SHUNT_TEST_EXCHANGE_STATIC_TOKENS".to_string();
    let server = start(config).await;

    let (status, error, _) = error_of(server.exchange("static-client-token").await).await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNAUTHORIZED, "invalid_grant")
    );

    let (_, issued) = json_of(
        server
            .exchange(&jwt::token(&github_claims(&uri, 300)))
            .await,
    )
    .await;
    let issued = issued["access_token"].as_str().unwrap().to_string();
    let (status, error, _) = error_of(server.exchange(&issued).await).await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNAUTHORIZED, "invalid_grant")
    );
}

#[tokio::test]
async fn any_other_grant_type_is_unsupported() {
    let (server, issuer, _env) = standard().await;
    let assertion = jwt::token(&github_claims(&issuer.uri(), 300));

    for body in [
        json!({"grant_type": "client_credentials", "assertion": assertion}),
        json!({"grant_type": "", "assertion": assertion}),
        json!({"assertion": assertion}),
        json!({"grant_type": null, "assertion": assertion}),
    ] {
        let (status, error, _) = error_of(server.post(&body).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::BAD_REQUEST, "unsupported_grant_type"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_malformed_or_incomplete_body_is_an_invalid_request() {
    let (server, issuer, _env) = standard().await;
    let assertion = jwt::token(&github_claims(&issuer.uri(), 300));
    let valid = sidecar_body(&assertion).to_string();

    // Not JSON, not an object, or a missing content type.
    for (content_type, body) in [
        (Some("application/json"), "not json".to_string()),
        (Some("application/json"), "[]".to_string()),
        (Some("application/json"), String::new()),
        (None, valid.clone()),
        (Some("application/x-www-form-urlencoded"), valid.clone()),
        (Some("text/plain"), valid),
    ] {
        let (status, error, _) = error_of(server.post_raw(content_type, &body).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{content_type:?} {body}"
        );
    }

    // A missing, empty, blank, or wrongly typed assertion or grant type.
    for body in [
        json!({"grant_type": GRANT}),
        json!({"grant_type": GRANT, "assertion": ""}),
        json!({"grant_type": GRANT, "assertion": "   "}),
        json!({"grant_type": GRANT, "assertion": null}),
        json!({"grant_type": GRANT, "assertion": 42}),
        json!({"grant_type": 42, "assertion": assertion}),
    ] {
        let (status, error, _) = error_of(server.post(&body).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn an_unreachable_key_set_is_a_503_not_a_401() {
    let env = common::env_lock().await;
    let issuer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&issuer)
        .await;
    let uri = issuer.uri();
    let server = start(config_with(
        vec![entry(&uri, jwt::AUDIENCE, true)],
        Some(exchange_config()),
    ))
    .await;
    let assertion = jwt::token(&github_claims(&uri, 300));

    let (status, error, _) = error_of(server.exchange(&assertion).await).await;

    assert_eq!(
        (status, error.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
    );
    drop(env);
}

#[tokio::test]
async fn error_descriptions_never_echo_the_assertion_or_name_a_check() {
    let (server, issuer, _env) = standard().await;
    let mut claims = github_claims(&issuer.uri(), 300);
    claims["aud"] = json!("someone-else");
    let assertion = jwt::token(&claims);

    let response = server.exchange(&assertion).await;
    let text = response.text().await.unwrap();

    assert!(!text.contains(&assertion));
    let description = serde_json::from_str::<Value>(&text).unwrap()["error_description"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    for word in [
        "audience",
        "expired",
        "signature",
        "issuer",
        "claim",
        "subject",
    ] {
        assert!(!description.contains(word), "{description}");
    }
}

#[tokio::test]
async fn the_route_does_not_exist_without_the_table() {
    let env = common::env_lock().await;
    let issuer = jwt::issuer().await;
    let uri = issuer.uri();
    let server = start(config_with(vec![entry(&uri, jwt::AUDIENCE, false)], None)).await;
    let assertion = jwt::token(&github_claims(&uri, 300));

    let response = server.exchange(&assertion).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    drop(env);
}

#[tokio::test]
async fn a_reload_that_removes_the_table_makes_the_route_a_404() {
    let (server, issuer, _env) = standard().await;
    let uri = issuer.uri();
    let assertion = jwt::token(&github_claims(&uri, 300));
    assert_eq!(server.exchange(&assertion).await.status(), StatusCode::OK);

    let reloaded = config_with(vec![entry(&uri, jwt::AUDIENCE, false)], None);
    server
        .shared
        .store(Arc::new(RuntimeState::from_config(reloaded).unwrap()));

    let response = server.exchange(&assertion).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["cache-control"], "no-store");
}
