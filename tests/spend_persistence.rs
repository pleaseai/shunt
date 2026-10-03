//! Spend counters survive a restart (stage 2, T005).
//!
//! A "restart" is a second router built from the same `[server.spend]
//! state_path`, restored the way `serve` restores it. Non-vacuity: delete the
//! `persist::restore` call in `boot` and `a_restart_enforces_pre_restart_spend`
//! goes red on a `200`; point `counters_path` at the caps file and
//! `counter_flushes_leave_the_caps_file_byte_unchanged` goes red.

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
        meter::{persist, window, FEMTO_USD_PER_CENT},
        persist as caps_persist,
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

const KEY_ENV: &str = "SHUNT_TEST_SPEND_KEY";
const TOKENS_ENV: &str = "SHUNT_TEST_SPEND_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_SPEND_ADMIN";
const ADMIN_TOKEN: &str = "admin-secret";

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
    config.server.spend = Some(SpendConfig { ..spend });
    config
}

fn spend(state_path: &std::path::Path, fail_closed: bool) -> SpendConfig {
    let mut spend = SpendConfig {
        state_path: Some(state_path.to_path_buf()),
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

fn seed(gateway: &Gateway, principal: &str, cents: u64) {
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

/// A fresh directory; `gateway-spend.json` inside it is the caps file.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-spend-restart-{}-{}-{label}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds the gateway and restores its state the way `serve` does.
async fn boot(upstream: &MockServer, state_path: &std::path::Path, fail_closed: bool) -> Gateway {
    let gateway = start(config(
        &upstream.uri(),
        true,
        spend(state_path, fail_closed),
    ))
    .await;
    caps_persist::restore(&gateway.state).await.unwrap();
    persist::restore(&gateway.state).await.unwrap();
    gateway
}

#[tokio::test]
async fn a_restart_enforces_pre_restart_spend() {
    let _env = env().await;
    let dir = temp_dir("enforce");
    let state_path = dir.join("gateway-spend.json");
    let first_upstream = upstream("/v1/messages", 0).await;
    let first = boot(&first_upstream, &state_path, false).await;
    set_cap(&first, user("alice"), "daily", 100).await;
    seed(&first, "alice", 100);
    seed(&first, "bob", 1);
    persist::flush(&first.state).await;
    drop(first);

    let upstream = upstream("/v1/messages", 1).await;
    let second = boot(&upstream, &state_path, false).await;

    let alice = messages(&second, "mapped-model", Some("tok-a")).await;
    assert_eq!(alice.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refusal_message(alice)
        .await
        .starts_with("spend limit reached (daily;"));
    let meter = second.state.gateway_stores.spend.meter();
    assert_eq!(
        meter.spent("alice", Period::Monthly, now()),
        100 * FEMTO_USD_PER_CENT as u64
    );
    assert_eq!(
        meter.spent("bob", Period::Daily, now()),
        FEMTO_USD_PER_CENT as u64
    );
    // Bob has no user cap and no org cap: still served.
    assert_eq!(
        messages(&second, "mapped-model", Some("tok-b"))
            .await
            .status(),
        StatusCode::OK
    );
    upstream.verify().await;
}

#[tokio::test]
async fn counter_flushes_leave_the_caps_file_byte_unchanged() {
    let _env = env().await;
    let dir = temp_dir("caps-bytes");
    let state_path = dir.join("gateway-spend.json");
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = boot(&upstream, &state_path, false).await;
    set_cap(&gateway, user("alice"), "daily", 100).await;
    let caps_before = std::fs::read(&state_path).unwrap();
    let counters_path = dir.join("gateway-spend.counters.json");
    assert!(!counters_path.exists(), "no spend yet, nothing to write");

    seed(&gateway, "alice", 3);
    persist::flush(&gateway.state).await;
    seed(&gateway, "alice", 4);
    persist::flush(&gateway.state).await;

    assert_eq!(std::fs::read(&state_path).unwrap(), caps_before);
    let counters: Value = serde_json::from_slice(&std::fs::read(&counters_path).unwrap()).unwrap();
    assert_eq!(counters["version"], 1);
    let daily = window(Period::Daily, now()).start;
    assert!(counters["counters"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| {
            record["principal"] == "alice"
                && record["period"] == "daily"
                && record["start"] == daily
                && record["femto"] == 7 * FEMTO_USD_PER_CENT as u64
        }));
}

#[tokio::test]
async fn an_unreadable_counter_record_fails_only_its_principal_after_restart() {
    let _env = env().await;
    let dir = temp_dir("malformed");
    let state_path = dir.join("gateway-spend.json");
    let daily = window(Period::Daily, now()).start;
    std::fs::write(
        dir.join("gateway-spend.counters.json"),
        json!({"version": 1, "counters": [
            {"principal": "alice", "period": "daily", "start": daily, "femto": "garbage"},
            {"principal": "bob", "period": "daily", "start": daily, "femto": 1},
        ]})
        .to_string(),
    )
    .unwrap();
    let upstream = upstream("/v1/messages", 1).await;
    let gateway = boot(&upstream, &state_path, true).await;
    set_cap(&gateway, org(), "daily", 100).await;

    let alice = messages(&gateway, "mapped-model", Some("tok-a")).await;
    assert_eq!(alice.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refusal_message(alice).await, "spend limit unavailable");
    assert_eq!(
        messages(&gateway, "mapped-model", Some("tok-b"))
            .await
            .status(),
        StatusCode::OK
    );
    upstream.verify().await;
}

#[tokio::test]
async fn an_unreadable_counters_envelope_aborts_the_restore() {
    let _env = env().await;
    let dir = temp_dir("envelope");
    let state_path = dir.join("gateway-spend.json");
    std::fs::write(dir.join("gateway-spend.counters.json"), b"{not json").unwrap();
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = start(config(&upstream.uri(), true, spend(&state_path, false))).await;

    assert!(persist::restore(&gateway.state).await.is_err());
}

#[tokio::test]
async fn empty_state_path_keeps_counters_in_memory() {
    let _env = env().await;
    let dir = temp_dir("memory-only");
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = boot(&upstream, std::path::Path::new(""), false).await;
    seed(&gateway, "alice", 5);

    persist::flush(&gateway.state).await;
    persist::flush_final(&gateway.state, std::time::Duration::from_secs(1)).await;

    assert_eq!(
        gateway
            .state
            .gateway_stores
            .spend
            .meter()
            .spent("alice", Period::Daily, now()),
        5 * FEMTO_USD_PER_CENT as u64
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn the_final_flush_persists_what_the_periodic_tick_has_not() {
    let _env = env().await;
    let dir = temp_dir("final");
    let state_path = dir.join("gateway-spend.json");
    let upstream = upstream("/v1/messages", 0).await;
    let gateway = boot(&upstream, &state_path, false).await;
    seed(&gateway, "alice", 9);

    persist::flush_final(&gateway.state, std::time::Duration::from_secs(5)).await;

    let counters = dir.join("gateway-spend.counters.json");
    assert!(counters.exists(), "final flush wrote the counters file");
}
