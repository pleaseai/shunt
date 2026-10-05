//! `[server.spend]` enforcement at `/v1/messages` admission (stage 2, T002).
//!
//! Caps are created through the admin API (the same path an operator uses) and
//! spend is seeded straight into the meter, so these pin what a request
//! experiences once a principal is at or over a cap: the refusal shape, which
//! principal is charged, and that a refused request reaches no upstream.
//!
//! Non-vacuity: delete the `spend_gate::enforce` call in `failover::forward`
//! and every refusal test goes red on a `200`; move it below the judge consult
//! and `an_over_cap_turn_never_reaches_the_judge_or_any_tier` goes red on the
//! judge's call count.

use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        AdminConfig, ApiKeyHeader, AuthMode, Config, CountTokens, InboundAuthConfig,
        ProviderConfig, ProviderKind, RouteConfig, SpendConfig,
    },
    gateway::spend::{
        meter::{reset_label, window, ANONYMOUS_PRINCIPAL, FEMTO_USD_PER_CENT},
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
mod judge_harness;

const KEY_ENV: &str = "SHUNT_TEST_SPEND_KEY";
const TOKENS_ENV: &str = "SHUNT_TEST_SPEND_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_SPEND_ADMIN";
const ADMIN_TOKEN: &str = "admin-secret";
const BLOCKED: &str = "Ask FinOps.";

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

fn provider(base_url: &str, auth: AuthMode, api_key_env: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: base_url.to_string(),
        auth,
        api_key_env: api_key_env.map(ToOwned::to_owned),
        api_key_header: ApiKeyHeader::Bearer,
        effort: None,
        service_tier: None,
        classifier_model: None,
        count_tokens: CountTokens::default(),
        websocket: false,
        tool_search: None,
        request_compression: true,
        accounts: Vec::new(),
        account_scope: Vec::new(),
        retry: shunt::config::RetryConfig::default(),
        workspace_roots: Vec::new(),
        sandbox: true,
        profile_dir: None,
    }
}

fn admin() -> AdminConfig {
    AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: ADMIN_ENV.to_string(),
        tokens_file: None,
        write_keys: Vec::new(),
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        hide_observed: false,
        oidc: None,
    }
}

/// `mapped-model` injects a gateway-held key; any other model rides the default
/// `anthropic` passthrough. Both point at the same upstream.
fn config(upstream: &str, inbound_auth: bool, spend: SpendConfig) -> Config {
    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream.to_string();
    config.providers.insert(
        "mapped".to_string(),
        provider(upstream, AuthMode::ApiKey, Some(KEY_ENV)),
    );
    config.routes.push(RouteConfig {
        model: "mapped-model".to_string(),
        provider: "mapped".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    if inbound_auth {
        config.server.auth = Some(InboundAuthConfig {
            jwt: Vec::new(),
            header: "x-shunt-token".to_string(),
            tokens_env: TOKENS_ENV.to_string(),
        });
    }
    config.server.admin = Some(admin());
    config.server.spend = Some(SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        ..spend
    });
    config
}

fn spend(blocked_message: Option<&str>, fail_closed: bool) -> SpendConfig {
    let mut spend = SpendConfig {
        blocked_message: blocked_message.map(ToOwned::to_owned),
        ..SpendConfig::default()
    };
    spend.enforcement.fail_closed_on_error = fail_closed;
    spend
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
        (KEY_ENV, "upstream-key"),
        (TOKENS_ENV, "alice:tok-a,bob:tok-b"),
        (ADMIN_ENV, &format!("admin:{ADMIN_TOKEN}")),
    ])
    .await
}

fn ok_upstream() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(r#"{"id":"msg_1","type":"message"}"#)
}

/// A mock upstream that must receive exactly `expect` POSTs to `route`.
async fn upstream(route: &str, expect: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ok_upstream())
        .expect(expect)
        .mount(&server)
        .await;
    server
}

/// Creates a cap through the admin API, in whole US cents.
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

/// Waits out a UTC day edge that is under 5s away. The weekly and monthly
/// edges coincide with a daily one, so this covers all three periods: a seed
/// recorded now and a request served moments later then share their windows.
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

async fn post_to(
    gateway: &Gateway,
    route: &str,
    model: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{route}", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            json!({"model": model, "max_tokens": 16,
                   "messages": [{"role": "user", "content": "hi"}]})
            .to_string(),
        );
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

async fn messages(gateway: &Gateway, model: &str, token: Option<&str>) -> reqwest::Response {
    let headers = token.map(|token| ("x-shunt-token", token));
    post_to(gateway, "/v1/messages", model, headers.as_slice()).await
}

async fn refusal_message(response: reqwest::Response) -> String {
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "billing_error");
    body["error"]["message"].as_str().unwrap().to_string()
}

/// A `type = "noop"` entry never reaches an upstream, so it is a free turn: an
/// over-cap principal is served it rather than refused with a spend `429`.
#[tokio::test]
async fn an_over_cap_principal_is_not_refused_a_noop_turn() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 0).await;
    let mut cfg = config(&upstream.uri(), true, spend(None, false));
    cfg.models.push(shunt::config::ModelConfig {
        id: "quiet".to_string(),
        display_name: None,
        upstream_model: None,
        router: Some(shunt::config::RouterConfig::Noop {}),
        stage_router: None,
        subagents: None,
    });
    let gateway = start(cfg.validate().expect("a noop entry is well formed")).await;
    set_cap(&gateway, user("alice"), "daily", 100).await;
    seed(&gateway, "alice", 100);

    let noop = messages(&gateway, "quiet", Some("tok-a")).await;
    assert_eq!(noop.status(), StatusCode::OK, "a noop turn is free");
    // The same principal is still refused on a metered route.
    let metered = messages(&gateway, "mapped-model", Some("tok-a")).await;
    assert_eq!(metered.status(), StatusCode::TOO_MANY_REQUESTS);
    upstream.verify().await;
}

#[tokio::test]
async fn user_cap_refuses_with_period_reset_and_blocked_message() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = start(config(&upstream.uri(), true, spend(Some(BLOCKED), false))).await;
    set_cap(&gateway, user("alice"), "daily", 100).await;
    seed(&gateway, "alice", 100);

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["x-should-retry"], "false");
    let retry_after: u64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let reset = window(Period::Daily, now()).end;
    assert!(
        (1..=86_400).contains(&retry_after) && retry_after.abs_diff(reset - now()) <= 2,
        "retry-after is the seconds until the daily reset, got {retry_after}"
    );
    assert_eq!(
        refusal_message(response).await,
        // The separator is a literal em dash between single spaces.
        format!(
            "spend limit reached (daily; resets {}) \u{2014} {BLOCKED}",
            reset_label(reset)
        )
    );
    assert!(reset_label(reset).ends_with(" 00:00 UTC"));
    upstream.verify().await;
}

#[tokio::test]
async fn refusal_without_blocked_message_is_the_bare_text() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, false))).await;
    set_cap(&gateway, user("alice"), "weekly", 5).await;
    seed(&gateway, "alice", 5);

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let reset = window(Period::Weekly, now()).end;
    assert_eq!(
        refusal_message(response).await,
        format!(
            "spend limit reached (weekly; resets {})",
            reset_label(reset)
        )
    );
    upstream.verify().await;
}

#[tokio::test]
async fn org_cap_refuses_a_principal_with_no_user_cap_and_names_the_period() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, false))).await;
    set_cap(&gateway, org(), "monthly", 200).await;
    seed(&gateway, "alice", 200);

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let reset = window(Period::Monthly, now()).end;
    assert_eq!(
        refusal_message(response).await,
        format!(
            "spend limit reached (monthly; resets {})",
            reset_label(reset)
        )
    );
    upstream.verify().await;
}

#[tokio::test]
async fn org_cap_is_per_seat_so_another_principal_under_it_passes() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 1).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, false))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    seed(&gateway, "alice", 100);
    seed(&gateway, "bob", 50);

    let alice = messages(&gateway, "mapped-model", Some("tok-a")).await;
    let bob = messages(&gateway, "mapped-model", Some("tok-b")).await;

    assert_eq!(alice.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(bob.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn count_tokens_is_never_refused() {
    let _env = env().await;
    let upstream = upstream("/v1/messages/count_tokens", 1).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, false))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    seed(&gateway, "alice", 100);

    let response = post_to(
        &gateway,
        "/v1/messages/count_tokens",
        "mapped-model",
        &[("x-shunt-token", "tok-a")],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn an_all_passthrough_chain_is_never_refused() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 2).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, false))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    seed(&gateway, "alice", 100);
    seed(&gateway, ANONYMOUS_PRINCIPAL, 100);

    // With and without an inbound token: the caller pays with their own key.
    let own_key = ("x-api-key", "sk-ant-caller-own-key");
    let anonymous = post_to(&gateway, "/v1/messages", "claude-sonnet-4-5", &[own_key]).await;
    let identified = post_to(
        &gateway,
        "/v1/messages",
        "claude-sonnet-4-5",
        &[own_key, ("x-shunt-token", "tok-a")],
    )
    .await;

    assert_eq!(anonymous.status(), StatusCode::OK);
    assert_eq!(identified.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn an_unauthenticated_injecting_request_is_enforced_as_anonymous() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 1).await;
    let gateway = start(config(&upstream.uri(), false, spend(None, false))).await;
    set_cap(&gateway, org(), "daily", 100).await;

    let under = messages(&gateway, "mapped-model", None).await;
    assert_eq!(under.status(), StatusCode::OK, "under the cap it is served");
    seed(&gateway, ANONYMOUS_PRINCIPAL, 100);
    let over = messages(&gateway, "mapped-model", None).await;

    assert_eq!(over.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refusal_message(over)
        .await
        .starts_with("spend limit reached (daily;"));
    upstream.verify().await;
}

#[tokio::test]
async fn unavailable_meter_forwards_by_default() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 1).await;
    let gateway = start(config(&upstream.uri(), true, spend(Some(BLOCKED), false))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn unavailable_meter_refuses_without_retry_after_when_fail_closed() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, true))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["x-should-retry"], "false");
    assert!(!response.headers().contains_key("retry-after"));
    assert_eq!(refusal_message(response).await, "spend limit unavailable");
    upstream.verify().await;
}

#[tokio::test]
async fn fail_closed_refusal_carries_the_blocked_message() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = start(config(&upstream.uri(), true, spend(Some(BLOCKED), true))).await;
    set_cap(&gateway, org(), "daily", 100).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refusal_message(response).await,
        format!("spend limit unavailable \u{2014} {BLOCKED}")
    );
}

#[tokio::test]
async fn fail_closed_forwards_an_uncapped_principal_whose_meter_is_unavailable() {
    let _env = env().await;
    let upstream = upstream("/v1/messages", 1).await;
    let gateway = start(config(&upstream.uri(), true, spend(None, true))).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = messages(&gateway, "mapped-model", Some("tok-a")).await;

    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}

/// The refusal precedes the router judge, so an over-cap caller spends no judge
/// call (or tier call) either.
#[tokio::test]
async fn an_over_cap_turn_never_reaches_the_judge_or_any_tier() {
    use judge_harness::{
        driven_config, env as judge_env, judge_mock, tier_mock, undecided_messages,
        CAPABLE_UPSTREAM_MODEL, CLIENT_TOKEN, EFFICIENT_UPSTREAM_MODEL, ROUTER_ID, SESSION,
    };
    let mut vars = judge_env().await;
    vars.set(ADMIN_ENV, format!("admin:{ADMIN_TOKEN}"));
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    judge_mock(0.1, 0).mount(&judge).await;
    let mut config = driven_config(&capable, &efficient, judge.uri());
    config.server.admin = Some(admin());
    config.server.spend = Some(SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        ..SpendConfig::default()
    });
    let gateway = start(config).await;
    set_cap(&gateway, user("client"), "daily", 100).await;
    seed(&gateway, "client", 100);

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .header("x-shunt-token", CLIENT_TOKEN)
        .body(
            json!({"model": ROUTER_ID, "max_tokens": 16, "messages": undecided_messages()})
                .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}
