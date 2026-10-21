//! `[server.spend]` enforcement on the inbound Codex endpoint's Responses
//! WebSocket (codex-spend-caps, T003).
//!
//! Caps are created through the admin API and spend is seeded straight into
//! the meter, as in `tests/codex_spend_enforcement.rs`. These pin what a Codex
//! CLI sees when a `response.create` turn arrives at a cap: one terminal
//! `error` frame (`status` 429, `usage_limit_reached`, `resets_at`, the promo
//! header), no upstream request, and a socket that stays open and re-assesses
//! every later turn.
//!
//! The meter reads the wall clock and exposes no injection seam, so "the
//! period resets" is stood in for by lifting the cap through the admin API on
//! the same open socket: both prove a refused turn leaves no state behind and
//! the next turn is assessed afresh.
//!
//! Non-vacuity: delete the `spend::admit` call in
//! `codex_endpoint::websocket::turn::run_turn` and the refusal tests go red on
//! a forwarded turn.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
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
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tungstenite::client::IntoClientRequest;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

const TOKENS_ENV: &str = "SHUNT_TEST_CODEX_SPEND_WS_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_CODEX_SPEND_WS_ADMIN";
const ACCOUNT_ENV: &str = "SHUNT_TEST_CODEX_SPEND_WS_ACCOUNT";
const ADMIN_TOKEN: &str = "admin-secret";
const BLOCKED: &str = "Ask FinOps.";
const MODEL: &str = "gpt-5.6-sol";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Gateway {
    base_url: String,
    addr: SocketAddr,
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
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-spend-ws"}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn config(upstream: &str) -> Config {
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
    config.server.spend = Some(SpendConfig {
        blocked_message: Some(BLOCKED.to_string()),
        state_path: Some(std::path::PathBuf::new()),
        ..SpendConfig::default()
    });
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
        addr,
        state,
        task,
    }
}

async fn env() -> common::EnvVars {
    common::set_env(&[
        (TOKENS_ENV, "alice:tok-a"),
        (ADMIN_ENV, &format!("admin:{ADMIN_TOKEN}")),
        (ACCOUNT_ENV, &chatgpt_token()),
    ])
    .await
}

/// A mock upstream answering every turn with one completed SSE response.
async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    let body = format!(
        "event: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed",
               "response": {"id": "resp_1", "usage": {
                   "input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}})
    );
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    server
}

async fn upstream_requests(upstream: &MockServer) -> usize {
    upstream.received_requests().await.unwrap().len()
}

/// Creates a daily cap on `alice` and returns its id.
async fn cap_alice(gateway: &Gateway, cents: u64) -> String {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/organizations/spend_limits",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_TOKEN)
        .json(&json!({"amount": cents.to_string(),
                      "scope": {"type": "user", "user_id": "alice"},
                      "period": "daily"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "cap must be created");
    let body: Value = response.json().await.unwrap();
    body["id"].as_str().expect("the cap has an id").to_string()
}

async fn lift_cap(gateway: &Gateway, id: &str) {
    let response = reqwest::Client::new()
        .delete(format!(
            "{}/v1/organizations/spend_limits/{id}",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "cap must be lifted");
}

/// Seeds `cents` of spend for alice, first waiting out a UTC day edge under
/// 5s away so the seed and a later turn share windows.
fn seed_alice(gateway: &Gateway, cents: u64) {
    let remaining = window(Period::Daily, now()).end.saturating_sub(now());
    if remaining < 5 {
        std::thread::sleep(Duration::from_secs(remaining) + Duration::from_millis(50));
    }
    gateway.state.gateway_stores.spend.meter().record(
        "alice",
        now(),
        cents * FEMTO_USD_PER_CENT as u64,
    );
}

async fn connect(gateway: &Gateway) -> Socket {
    let mut request = format!("ws://{}/v1/responses", gateway.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer tok-a".parse().unwrap());
    connect_async(request).await.unwrap().0
}

async fn send_create(socket: &mut Socket, generate: bool) {
    socket
        .send(Message::Text(
            json!({"type": "response.create", "generate": generate, "model": MODEL,
                   "input": []})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn next_json(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for a websocket frame")
        .expect("socket ended")
        .expect("websocket read failed");
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

/// Asserts `frame` is the usage-limit refusal for a daily cap on alice.
fn assert_refusal(frame: &Value) {
    let reset = window(Period::Daily, now()).end;
    assert_eq!(frame["type"], "error", "{frame}");
    assert_eq!(frame["status"], 429, "{frame}");
    assert_eq!(frame["error"]["type"], "usage_limit_reached");
    assert_eq!(frame["error"]["code"], "usage_limit_reached");
    assert_eq!(frame["error"]["resets_at"], reset);
    assert_eq!(
        frame["error"]["message"],
        format!(
            "spend limit reached (daily; resets {}) \u{2014} {BLOCKED}",
            reset_label(reset)
        )
    );
    assert_eq!(frame["headers"]["x-codex-promo-message"], BLOCKED);
    let retry_after: u64 = frame["headers"]["retry-after"]
        .as_str()
        .expect("retry-after is projected")
        .parse()
        .unwrap();
    assert!(retry_after.abs_diff(reset - now()) <= 2, "{retry_after}");
}

/// Reads frames until a `response.completed`, failing on an `error` frame.
async fn expect_completed(socket: &mut Socket) {
    loop {
        let frame = next_json(socket).await;
        assert_ne!(frame["type"], "error", "the turn failed: {frame}");
        if frame["type"] == "response.completed" {
            return;
        }
    }
}

/// Asserts no further frame arrives within a short window.
async fn assert_quiet(socket: &mut Socket) {
    let extra = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
    assert!(extra.is_err(), "unexpected extra frame: {extra:?}");
}

#[tokio::test]
async fn an_over_cap_turn_gets_one_error_frame_and_never_reaches_the_upstream() {
    let _env = env().await;
    let upstream = upstream().await;
    let gateway = start(config(&upstream.uri())).await;
    cap_alice(&gateway, 100).await;
    seed_alice(&gateway, 100);
    let mut socket = connect(&gateway).await;

    send_create(&mut socket, true).await;

    assert_refusal(&next_json(&mut socket).await);
    assert_quiet(&mut socket).await;
    assert_eq!(upstream_requests(&upstream).await, 0);
}

#[tokio::test]
async fn the_socket_stays_open_and_refuses_each_later_turn() {
    let _env = env().await;
    let upstream = upstream().await;
    let gateway = start(config(&upstream.uri())).await;
    cap_alice(&gateway, 100).await;
    seed_alice(&gateway, 100);
    let mut socket = connect(&gateway).await;

    for _ in 0..3 {
        send_create(&mut socket, true).await;
        assert_refusal(&next_json(&mut socket).await);
    }

    assert_eq!(upstream_requests(&upstream).await, 0);
}

#[tokio::test]
async fn a_turn_after_the_cap_is_lifted_is_forwarded_on_the_same_socket() {
    let _env = env().await;
    let upstream = upstream().await;
    let gateway = start(config(&upstream.uri())).await;
    let cap = cap_alice(&gateway, 100).await;
    seed_alice(&gateway, 100);
    let mut socket = connect(&gateway).await;

    send_create(&mut socket, true).await;
    assert_refusal(&next_json(&mut socket).await);
    assert_eq!(upstream_requests(&upstream).await, 0);

    lift_cap(&gateway, &cap).await;
    send_create(&mut socket, true).await;
    expect_completed(&mut socket).await;
    assert_eq!(upstream_requests(&upstream).await, 1);

    // Re-imposing the cap refuses the next turn on that same socket again.
    cap_alice(&gateway, 100).await;
    send_create(&mut socket, true).await;
    assert_refusal(&next_json(&mut socket).await);
    assert_eq!(upstream_requests(&upstream).await, 1);
}

#[tokio::test]
async fn an_over_cap_warmup_is_still_answered_locally() {
    let _env = env().await;
    let upstream = upstream().await;
    let gateway = start(config(&upstream.uri())).await;
    cap_alice(&gateway, 100).await;
    seed_alice(&gateway, 100);
    let mut socket = connect(&gateway).await;

    send_create(&mut socket, false).await;

    let created = next_json(&mut socket).await;
    let completed = next_json(&mut socket).await;
    assert_eq!(created["type"], "response.created");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(upstream_requests(&upstream).await, 0);

    // The warmup left the gate armed for the next real turn.
    send_create(&mut socket, true).await;
    assert_refusal(&next_json(&mut socket).await);
}
