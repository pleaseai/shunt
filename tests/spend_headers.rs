//! `anthropic-ratelimit-unified-*` headers for a spend-capped principal
//! (stage 2, T006).
//!
//! Every upstream mock answers with its own pool-level rate-limit headers
//! (`unified-status: rejected`, a `5h` window, a token limit). A capped
//! principal must see none of them on any 2xx path — the ordered-chain relay,
//! the committed stream, the gated replay — and must see its own binding cap
//! instead. An uncapped principal and an all-passthrough chain see the
//! upstream's headers untouched, which is also the proof that the mocks really
//! send them on the relay path.
//!
//! Non-vacuity: delete the `rate_limit.apply` on the `Ok` arm of
//! `failover::forward` and every 2xx test goes red; return
//! `spend_headers::Plan::Unchanged` for an allowed principal in
//! `spend_gate::enforce` and the same tests go red on the missing status.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::{header::HeaderMap, StatusCode};
use serde_json::{json, Value};
use shunt::{
    config::{
        AdminConfig, ApiKeyHeader, AuthMap, AuthMode, Config, InboundAuthConfig, ModelConfig,
        ProviderKind, RouterConfig, SpendConfig, UpstreamAuth,
    },
    gateway::spend::{
        meter::{window, FEMTO_USD_PER_CENT},
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

use judge_harness::{alias, can_bind_loopback, upstream_with, ModelIs};

const KEY_ENV: &str = "SHUNT_TEST_RL_KEY";
const TOKENS_ENV: &str = "SHUNT_TEST_RL_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_RL_ADMIN";
const ADMIN_TOKEN: &str = "admin-secret";
const ALICE: &str = "tok-a";
const BOB: &str = "tok-b";
const ROUTER_ID: &str = "claude-auto";
const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

const MAIN_MODEL: &str = "upstream-main";
const OWN_MODEL: &str = "upstream-own";
const CHAIN_MODEL: &str = "upstream-chain-main";
const STRONG_MODEL: &str = "upstream-strong";
const WEAK_MODEL: &str = "upstream-weak";
const JUDGE_MODEL: &str = "upstream-judge";

const ESCALATION_ROUTER: &str = r#"
type = "llm_classifier"
mode = "escalation"
classifier_target = "judge-alias"
strong_target = "strong-alias"
weak_target = "weak-alias"
judge_timeout_ms = 2000

[escalation]
confirmations = 1
"#;

/// The upstream's own pool-level values, which a capped principal must never
/// see.
const UPSTREAM_HEADERS: [(&str, &str); 3] = [
    ("anthropic-ratelimit-unified-status", "rejected"),
    ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
    ("anthropic-ratelimit-tokens-limit", "1000"),
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

fn injecting(name: &str, base_url: String, kind: ProviderKind) -> shunt::config::UpstreamConfig {
    let mut upstream = upstream_with(
        name,
        base_url,
        UpstreamAuth::Map(AuthMap::ApiKey {
            env: Some(KEY_ENV.to_string()),
            header: Some(ApiKeyHeader::XApiKey),
        }),
    );
    upstream.kind = Some(kind);
    upstream
}

/// All Anthropic upstreams share `anth`, told apart by the body's `model`;
/// `resp` is the Responses primary of `chain-alias`.
fn config(anth: &str, resp: &str, fail_closed: bool) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    let anthropic = |name: &str| injecting(name, anth.to_string(), ProviderKind::Anthropic);
    config.upstreams = vec![
        injecting("resp", resp.to_string(), ProviderKind::Responses),
        anthropic("main"),
        anthropic("strong"),
        anthropic("weak"),
        anthropic("judge"),
        upstream_with(
            "own",
            anth.to_string(),
            UpstreamAuth::Shorthand(AuthMode::Passthrough),
        ),
    ];
    config.server.default_provider = "own".to_string();
    config.models = vec![
        alias("main-alias", "main", MAIN_MODEL),
        alias("own-alias", "own", OWN_MODEL),
        alias("strong-alias", "strong", STRONG_MODEL),
        alias("weak-alias", "weak", WEAK_MODEL),
        alias("judge-alias", "judge", JUDGE_MODEL),
        ModelConfig {
            subagents: None,
            id: "chain-alias".to_string(),
            display_name: None,
            upstream_model: Some(
                [
                    ("resp".to_string(), "upstream-chain-resp".to_string()),
                    ("main".to_string(), CHAIN_MODEL.to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            router: None,
            stage_router: None,
        },
        ModelConfig {
            subagents: None,
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(toml::from_str::<RouterConfig>(ESCALATION_ROUTER).unwrap()),
            stage_router: None,
        },
    ];
    config.server.auth = Some(InboundAuthConfig {
        jwt: Vec::new(),
        header: "x-shunt-token".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
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
    let mut spend = SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        ..SpendConfig::default()
    };
    spend.enforcement.fail_closed_on_error = fail_closed;
    config.server.spend = Some(spend);
    config
        .validate()
        .expect("the header test config is well formed")
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
        (TOKENS_ENV, &format!("alice:{ALICE},bob:{BOB}")),
        (ADMIN_ENV, &format!("admin:{ADMIN_TOKEN}")),
    ])
    .await
}

fn with_upstream_headers(mut reply: ResponseTemplate) -> ResponseTemplate {
    for (name, value) in UPSTREAM_HEADERS {
        reply = reply.append_header(name, value);
    }
    reply
}

fn json_reply(model: &str, text: &str) -> ResponseTemplate {
    with_upstream_headers(ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": model,
        "content": [{"type": "text", "text": text}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })))
}

fn sse_reply(model: &str, text: &str) -> ResponseTemplate {
    let frames = [
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": model,
                "content": [], "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": text}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 5}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    let body: String = frames
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect();
    with_upstream_headers(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
}

async fn mount(server: &MockServer, model: &'static str, reply: ResponseTemplate, expect: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(ModelIs(model))
        .respond_with(reply)
        .expect(expect)
        .mount(server)
        .await;
}

/// A Responses primary that always fails, so a streamed `chain-alias` turn
/// takes the committed-stream path and is won by `main`.
async fn failing_responses() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    server
}

async fn set_cap(gateway: &Gateway, user: &str, period: &str, cents: u64) {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/organizations/spend_limits",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_TOKEN)
        .json(&json!({"amount": cents.to_string(), "period": period,
                      "scope": {"type": "user", "user_id": user}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "cap must be created");
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

async fn send(
    gateway: &Gateway,
    model: &str,
    stream: bool,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .body(
            json!({"model": model, "max_tokens": 16, "stream": stream,
                   "messages": [{"role": "user", "content": [
                       {"type": "text", "text": "add a --json flag"}]}]})
            .to_string(),
        );
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).map(|value| value.to_str().unwrap())
}

/// Every `anthropic-ratelimit-*` header, each repetition its own entry.
fn rate_limit(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("anthropic-ratelimit-"))
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
        .collect();
    found.sort();
    found
}

fn expected(entries: &[(&str, String)]) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = entries
        .iter()
        .map(|(name, value)| (format!("anthropic-ratelimit-unified-{name}"), value.clone()))
        .collect();
    entries.sort();
    entries
}

/// The full set a 2xx to `alice` at 82% of a weekly cap carries — and
/// nothing else of the family, so any upstream value would break equality.
fn assert_weekly_warning_at_82(headers: &HeaderMap) {
    let reset = window(Period::Weekly, now()).end.to_string();
    assert_eq!(
        rate_limit(headers),
        expected(&[
            ("status", "allowed_warning".into()),
            ("reset", reset.clone()),
            ("overage-reset", reset),
            ("overage-utilization", "0.82".into()),
            ("overage-surpassed-threshold", "0.75".into()),
            ("representative-claim", "overage".into()),
            ("overage-status", "allowed_warning".into()),
        ])
    );
}

fn assert_upstream_values_pass_through(headers: &HeaderMap) {
    let mut sent: Vec<(String, String)> = UPSTREAM_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    sent.sort();
    assert_eq!(rate_limit(headers), sent);
}

async fn capped_alice(gateway: &Gateway, cents: u64) {
    set_cap(gateway, "alice", "weekly", 100).await;
    seed(gateway, "alice", cents);
}

#[tokio::test]
async fn a_relayed_2xx_reports_the_principals_own_cap_not_the_upstreams() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, MAIN_MODEL, json_reply(MAIN_MODEL, "MAIN"), 1).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 82).await;

    let response = send(&gateway, "main-alias", false, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_weekly_warning_at_82(response.headers());
    assert!(response.text().await.unwrap().contains("MAIN"));
    anth.verify().await;
}

#[tokio::test]
async fn a_committed_stream_reports_the_principals_own_cap() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, CHAIN_MODEL, sse_reply(CHAIN_MODEL, "CHAINED"), 1).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 82).await;

    let response = send(&gateway, "chain-alias", true, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    // The committed path omits the upstream-naming headers; this is how the
    // test knows it did not take the ordered loop.
    assert!(response.headers().get("x-gateway-upstream-model").is_none());
    assert_weekly_warning_at_82(response.headers());
    assert!(response.text().await.unwrap().contains("CHAINED"));
    anth.verify().await;
}

#[tokio::test]
async fn a_replayed_gated_turn_reports_the_principals_own_cap() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, WEAK_MODEL, sse_reply(WEAK_MODEL, "WEAK-ANSWER"), 1).await;
    let verdict = json!({"escalate": false, "reason": "progressing"}).to_string();
    mount(&anth, JUDGE_MODEL, json_reply(JUDGE_MODEL, &verdict), 1).await;
    mount(&anth, STRONG_MODEL, sse_reply(STRONG_MODEL, "STRONG"), 0).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 82).await;

    let response = send(&gateway, ROUTER_ID, true, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(response.headers(), "x-gateway-route-source"),
        Some("escalation_weak")
    );
    assert_weekly_warning_at_82(response.headers());
    assert!(response.text().await.unwrap().contains("WEAK-ANSWER"));
    anth.verify().await;
}

#[tokio::test]
async fn below_the_first_threshold_is_plain_allowed() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, MAIN_MODEL, json_reply(MAIN_MODEL, "MAIN"), 1).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    // Exactly 75% is not over the 0.75 threshold.
    capped_alice(&gateway, 75).await;

    let response = send(&gateway, "main-alias", false, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    let reset = window(Period::Weekly, now()).end.to_string();
    assert_eq!(
        rate_limit(response.headers()),
        expected(&[
            ("status", "allowed".into()),
            ("reset", reset.clone()),
            ("overage-reset", reset),
            ("overage-utilization", "0.75".into()),
            ("representative-claim", "overage".into()),
            ("overage-status", "allowed".into()),
        ])
    );
}

/// The positive twin of the relay test: same chain, a principal with no cap.
#[tokio::test]
async fn an_uncapped_principal_sees_the_upstream_headers_unchanged() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, MAIN_MODEL, json_reply(MAIN_MODEL, "MAIN"), 1).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 82).await;

    let response = send(&gateway, "main-alias", false, &[("x-shunt-token", BOB)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_upstream_values_pass_through(response.headers());
}

#[tokio::test]
async fn an_all_passthrough_chain_is_left_unchanged_for_a_capped_principal() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, OWN_MODEL, json_reply(OWN_MODEL, "OWN"), 1).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 82).await;

    let response = send(
        &gateway,
        "own-alias",
        false,
        &[
            ("x-shunt-token", ALICE),
            ("x-api-key", "sk-ant-caller-own-key"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_upstream_values_pass_through(response.headers());
}

#[tokio::test]
async fn the_over_cap_refusal_carries_the_exceeded_set() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, MAIN_MODEL, json_reply(MAIN_MODEL, "MAIN"), 0).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 120).await;

    let response = send(&gateway, "main-alias", false, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let headers = response.headers();
    let reset = window(Period::Weekly, now()).end;
    assert_eq!(
        rate_limit(headers),
        expected(&[
            ("status", "rejected".into()),
            ("reset", reset.to_string()),
            ("overage-reset", reset.to_string()),
            ("overage-utilization", "1.2".into()),
            ("overage-surpassed-threshold", "1".into()),
            ("overage-period", "weekly".into()),
            ("overage-disabled-reason", "org_spend_cap_reached".into()),
        ])
    );
    assert_eq!(header(headers, "x-should-retry"), Some("false"));
    assert!(headers.contains_key("retry-after"));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "billing_error");
    anth.verify().await;
}

#[tokio::test]
async fn the_fail_closed_refusal_carries_only_the_fetch_error_reason() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, MAIN_MODEL, json_reply(MAIN_MODEL, "MAIN"), 0).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;
    capped_alice(&gateway, 10).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = send(&gateway, "main-alias", false, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        rate_limit(response.headers()),
        expected(&[("overage-disabled-reason", "fetch_error".into())])
    );
    assert_eq!(header(response.headers(), "x-should-retry"), Some("false"));
    assert!(!response.headers().contains_key("retry-after"));
    anth.verify().await;
}

#[tokio::test]
async fn a_fail_open_forward_carries_no_rate_limit_headers_for_a_capped_principal() {
    let _env = env().await;
    let anth = MockServer::start().await;
    mount(&anth, MAIN_MODEL, json_reply(MAIN_MODEL, "MAIN"), 1).await;
    let resp = failing_responses().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), false)).await;
    capped_alice(&gateway, 10).await;
    gateway
        .state
        .gateway_stores
        .spend
        .meter()
        .mark_unavailable("alice");

    let response = send(&gateway, "main-alias", false, &[("x-shunt-token", ALICE)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(rate_limit(response.headers()), Vec::new());
    anth.verify().await;
}
