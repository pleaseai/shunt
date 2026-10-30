//! `[server.spend]` enforcement on the inbound Codex endpoint's HTTP response
//! routes (codex-spend-caps, T001).
//!
//! Caps are created through the admin API and spend is seeded straight into
//! the meter, as in `tests/spend_enforcement.rs`. These pin what a Codex CLI
//! sees once its principal is at a cap: a `429` carrying the
//! `usage_limit_reached` type and code, `resets_at`, `retry-after` and the
//! promo header, with no request reaching the upstream.
//!
//! Non-vacuity: `codex_endpoint::forward` admits turns and web search through
//! two separate `spend::admit` calls. Delete the turn call and the
//! `/responses` refusal tests go red on a `200`; delete the search call and
//! the web-search refusal test does.

use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        AccountConfig, AdminConfig, CodexEndpointConfig, Config, InboundAuthConfig, SpendConfig,
    },
    gateway::spend::{
        meter::{reset_label, window, FEMTO_USD_PER_CENT},
        store::Period,
    },
    server::{self, AppState},
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

const TOKENS_ENV: &str = "SHUNT_TEST_CODEX_SPEND_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_CODEX_SPEND_ADMIN";
const ACCOUNT_ENV: &str = "SHUNT_TEST_CODEX_SPEND_ACCOUNT";
const ADMIN_TOKEN: &str = "admin-secret";
const BLOCKED: &str = "Ask FinOps.";
const BODY: &str = r#"{"model":"gpt-5.6-sol","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":false,"store":false}"#;
const SEARCH_PATHS: [&str; 3] = [
    "/backend-api/codex/alpha/search",
    "/alpha/search",
    "/v1/alpha/search",
];
const SEARCH_BODY: &str =
    r#"{"model":"gpt-5.6-sol","search_query":[{"q":"spend caps"}],"response_length":"short"}"#;
const PATHS: [&str; 3] = [
    "/backend-api/codex/responses",
    "/responses",
    "/v1/responses",
];

struct Gateway {
    base_url: String,
    state: AppState,
    task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn chatgpt_token() -> String {
    let payload = json!({
        "exp": now() + 3_600,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-spend"}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn spend(blocked_message: Option<&str>, fail_closed: bool) -> SpendConfig {
    let mut spend = SpendConfig {
        blocked_message: blocked_message.map(ToOwned::to_owned),
        state_path: Some(std::path::PathBuf::new()),
        ..SpendConfig::default()
    };
    spend.enforcement.fail_closed_on_error = fail_closed;
    spend
}

fn config(upstream: &str, spend: Option<SpendConfig>) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = upstream.to_string();
    provider.accounts = vec![AccountConfig {
        name: "account".to_string(),
        token_env: Some(ACCOUNT_ENV.to_string()),
        ..Default::default()
    }];
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        ..Default::default()
    });
    config.server.auth = Some(InboundAuthConfig {
        jwt: Vec::new(),
        header: "x-shunt-token".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
        ..Default::default()
    });
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: ADMIN_ENV.to_string(),
        tokens_file: None,
        write_keys: Vec::new(),
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        hide_observed: false,
        oidc: None,
    });
    config.server.spend = spend;
    config
}

async fn start(mut config: Config) -> Gateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gateway {
        base_url: format!("http://{addr}"),
        state,
        task,
    }
}

async fn env() -> common::EnvVars {
    common::set_env(&[
        (TOKENS_ENV, "alice:tok-a,bob:tok-b"),
        (ADMIN_ENV, &format!("admin:{ADMIN_TOKEN}")),
        (ACCOUNT_ENV, &chatgpt_token()),
    ])
    .await
}

/// A mock upstream that must receive exactly `expect` POSTs.
async fn upstream(expect: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(expect)
        .mount(&server)
        .await;
    server
}

async fn set_cap(gateway: &Gateway, scope: Value, period: &str, cents: u64) {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/organizations/spend_limits",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_TOKEN)
        .json(&json!({"amount": cents.to_string(), "scope": scope, "period": period}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "cap must be created");
}

fn user(id: &str) -> Value {
    json!({"type": "user", "user_id": id})
}

fn org() -> Value {
    json!({"type": "organization"})
}

/// Waits out a UTC day edge under 5s away (the weekly and monthly edges
/// coincide with a daily one) so a seed and a later request share windows.
fn clear_of_window_edge() {
    let remaining = window(Period::Daily, now()).end.saturating_sub(now());
    if remaining < 5 {
        std::thread::sleep(
            std::time::Duration::from_secs(remaining) + std::time::Duration::from_millis(50),
        );
    }
}

fn seed(gateway: &Gateway, principal: &str, cents: u64) {
    clear_of_window_edge();
    gateway.state.gateway_stores.spend.meter().record(
        principal,
        now(),
        cents * FEMTO_USD_PER_CENT as u64,
    );
}

async fn post(gateway: &Gateway, route: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{route}", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", token)
        .body(BODY)
        .send()
        .await
        .unwrap()
}

fn header_u64(response: &reqwest::Response, name: &str) -> u64 {
    response.headers()[name].to_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn an_over_cap_turn_is_refused_on_every_response_path() {
    let _env = env().await;
    let upstream = upstream(0).await;
    let gateway = start(config(&upstream.uri(), Some(spend(Some(BLOCKED), false)))).await;
    set_cap(&gateway, user("alice"), "daily", 100).await;
    seed(&gateway, "alice", 100);
    let reset = window(Period::Daily, now()).end;

    for route in PATHS {
        let response = post(&gateway, route, "tok-a").await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{route}");
        assert_eq!(response.headers()["x-should-retry"], "false", "{route}");
        assert_eq!(response.headers()["x-codex-promo-message"], BLOCKED);
        let retry_after = header_u64(&response, "retry-after");
        assert!(
            retry_after.abs_diff(reset - now()) <= 2,
            "retry-after is the seconds until the reset, got {retry_after}"
        );
        let body: Value = response.json().await.unwrap();
        let error = &body["error"];
        assert_eq!(error["type"], "usage_limit_reached", "{route}");
        assert_eq!(error["code"], "usage_limit_reached", "{route}");
        assert_eq!(error["resets_at"], reset, "{route}");
        assert_eq!(
            error["message"],
            format!(
                "spend limit reached (daily; resets {}) \u{2014} {BLOCKED}",
                reset_label(reset)
            )
        );
    }
    upstream.verify().await;
}

#[tokio::test]
async fn the_cap_that_resets_last_is_the_one_named() {
    let _env = env().await;
    let upstream = upstream(0).await;
    let gateway = start(config(&upstream.uri(), Some(spend(None, false)))).await;
    set_cap(&gateway, user("alice"), "daily", 100).await;
    set_cap(&gateway, user("alice"), "monthly", 100).await;
    seed(&gateway, "alice", 100);
    let reset = window(Period::Monthly, now()).end;

    let response = post(&gateway, "/responses", "tok-a").await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!response.headers().contains_key("x-codex-promo-message"));
    let retry_after = header_u64(&response, "retry-after");
    assert!(retry_after.abs_diff(reset - now()) <= 2, "{retry_after}");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["resets_at"], reset);
    assert_eq!(
        body["error"]["message"],
        format!(
            "spend limit reached (monthly; resets {})",
            reset_label(reset)
        )
    );
    upstream.verify().await;
}

#[tokio::test]
async fn a_blocked_message_that_is_not_a_header_value_stays_in_the_message_only() {
    let _env = env().await;
    let upstream = upstream(0).await;
    let promo = "Fragen Sie die Finanzabteilung \u{2014} bitte";
    let gateway = start(config(&upstream.uri(), Some(spend(Some(promo), false)))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    seed(&gateway, "alice", 100);

    let response = post(&gateway, "/v1/responses", "tok-a").await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!response.headers().contains_key("x-codex-promo-message"));
    let body: Value = response.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().ends_with(promo));
    upstream.verify().await;
}

#[tokio::test]
async fn fail_closed_refuses_a_capped_principal_without_reset_or_retry_after() {
    let _env = env().await;
    let upstream = upstream(0).await;
    let gateway = start(config(&upstream.uri(), Some(spend(Some(BLOCKED), true)))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = post(&gateway, "/backend-api/codex/responses", "tok-a").await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!response.headers().contains_key("retry-after"));
    assert_eq!(response.headers()["x-codex-promo-message"], BLOCKED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "usage_limit_reached");
    assert_eq!(body["error"]["code"], "usage_limit_reached");
    assert!(body["error"].get("resets_at").is_none());
    assert_eq!(
        body["error"]["message"],
        format!("spend limit unavailable \u{2014} {BLOCKED}")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn fail_open_and_uncapped_principals_are_forwarded_when_state_is_unavailable() {
    let _env = env().await;
    let upstream = upstream(2).await;
    let open = start(config(&upstream.uri(), Some(spend(None, false)))).await;
    set_cap(&open, org(), "daily", 100).await;
    open.state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");
    let closed = start(config(&upstream.uri(), Some(spend(None, true)))).await;
    closed
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    // Fail-open forwards a capped principal; fail-closed forwards an uncapped one.
    assert_eq!(
        post(&open, "/responses", "tok-a").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        post(&closed, "/responses", "tok-a").await.status(),
        StatusCode::OK
    );
    upstream.verify().await;
}

#[tokio::test]
async fn without_server_spend_the_request_is_forwarded() {
    let _env = env().await;
    let upstream = upstream(1).await;
    let gateway = start(config(&upstream.uri(), None)).await;

    let response = post(&gateway, "/responses", "tok-a").await;

    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn an_under_cap_principal_is_forwarded_while_an_over_cap_one_is_refused() {
    let _env = env().await;
    let upstream = upstream(1).await;
    let gateway = start(config(&upstream.uri(), Some(spend(None, false)))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    seed(&gateway, "alice", 100);
    seed(&gateway, "bob", 50);

    let alice = post(&gateway, "/responses", "tok-a").await;
    let bob = post(&gateway, "/responses", "tok-b").await;

    assert_eq!(alice.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(bob.status(), StatusCode::OK);
    upstream.verify().await;
}

async fn search_upstream(expect: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"results":[]}"#))
        .expect(expect)
        .mount(&server)
        .await;
    server
}

async fn post_search(gateway: &Gateway, route: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{route}", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", token)
        .body(SEARCH_BODY)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_over_cap_search_is_refused_on_every_alias_and_never_forwarded() {
    let _env = env().await;
    let upstream = search_upstream(0).await;
    let gateway = start(config(&upstream.uri(), Some(spend(Some(BLOCKED), false)))).await;
    set_cap(&gateway, user("alice"), "daily", 100).await;
    seed(&gateway, "alice", 100);
    let reset = window(Period::Daily, now()).end;

    for route in SEARCH_PATHS {
        let response = post_search(&gateway, route, "tok-a").await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{route}");
        assert_eq!(response.headers()["x-should-retry"], "false", "{route}");
        assert_eq!(response.headers()["x-codex-promo-message"], BLOCKED);
        assert!(response.headers().contains_key("retry-after"), "{route}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "usage_limit_reached", "{route}");
        assert_eq!(body["error"]["code"], "usage_limit_reached", "{route}");
        assert_eq!(body["error"]["resets_at"], reset, "{route}");
    }
    upstream.verify().await;
}

#[tokio::test]
async fn an_under_cap_search_is_forwarded_and_leaves_every_counter_unchanged() {
    let _env = env().await;
    let upstream = search_upstream(3).await;
    let gateway = start(config(&upstream.uri(), Some(spend(None, false)))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    seed(&gateway, "bob", 50);
    let meter = gateway.state.gateway_stores.spend.meter();
    let counters = || {
        [Period::Daily, Period::Weekly, Period::Monthly]
            .map(|period| meter.spent("bob", period, now()))
    };
    let before = counters();
    assert!(
        before.iter().all(|spent| *spent > 0),
        "seed must be visible"
    );

    for route in SEARCH_PATHS {
        let response = post_search(&gateway, route, "tok-b").await;
        assert_eq!(response.status(), StatusCode::OK, "{route}");
    }

    assert_eq!(counters(), before);
    assert_eq!(meter.principals(), vec!["bob".to_string()]);
    upstream.verify().await;
}
