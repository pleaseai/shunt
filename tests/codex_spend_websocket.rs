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
//! A turn the client closes or replaces before `response.completed` bills
//! the estimated-input floor (T006; see the section at the end).
//!
//! Non-vacuity: delete the `spend::admit` call in
//! `codex_endpoint::websocket::turn::run_turn` and the refusal tests go red on
//! a forwarded turn.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        AccountConfig, AdminConfig, CodexEndpointConfig, Config, InboundAuthConfig, PricingConfig,
        PricingOverride, SpendConfig,
    },
    gateway::spend::{
        meter::{reset_label, window, FEMTO_USD_PER_CENT},
        pricing::{Rates, Usage},
        store::Period,
    },
    server::{self, AppState},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, Notify},
    task::JoinHandle,
};
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

// ---- The estimated-input floor of an incomplete turn (T006) ----
//
// A WebSocket turn the client abandons before `response.completed` never
// learns its usage, so it bills the request's estimated input plus the
// delivered-text floor. The upstream holds the turn open after its deltas,
// as in `inbound_codex_websocket.rs`'s
// `replacement_and_disconnect_drop_active_upstream_bodies`.
//
// Non-vacuity: bill zero input in `StreamSpend::settle` and both floor tests
// go red on the output-only cost.

/// `(input, output, cache_read, cache_write)` USD per million for [`MODEL`].
const RATES: (f64, f64, f64, f64) = (2.0, 8.0, 0.2, 2.5);
/// The held turn's delivered text: 13 characters, a 4-token output floor.
const CUT_TEXT: [&str; 2] = ["Hello, ", "world!"];

enum UpstreamReply {
    /// The turn's opening and [`CUT_TEXT`] deltas, then held open until the
    /// gateway drops the body.
    Held,
    /// A completed turn reporting 10 input and 5 output tokens.
    Completed,
}

#[derive(Clone)]
struct HeldUpstream {
    replies: Arc<Mutex<VecDeque<UpstreamReply>>>,
    requests: Arc<Mutex<Vec<Bytes>>>,
    dropped: Arc<Notify>,
}

async fn held_reply(State(upstream): State<HeldUpstream>, body: Bytes) -> Response {
    upstream.requests.lock().unwrap().push(body);
    let reply = upstream.replies.lock().unwrap().pop_front().unwrap();
    let sse = |frames: Vec<Value>| -> String {
        frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect()
    };
    let body = match reply {
        UpstreamReply::Completed => Body::from(sse(vec![json!({
            "type": "response.completed",
            "response": {"id": "resp_2", "usage": {
                "input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}})])),
        UpstreamReply::Held => {
            let mut frames = vec![json!({"type": "response.created",
                                         "response": {"id": "resp_held"}})];
            frames.extend(
                CUT_TEXT.map(|text| json!({"type": "response.output_text.delta", "delta": text})),
            );
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
            tx.send(Ok(Bytes::from(sse(frames)))).await.unwrap();
            let dropped = upstream.dropped.clone();
            tokio::spawn(async move {
                tx.closed().await;
                dropped.notify_one();
            });
            Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }))
        }
    };
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn held_upstream(replies: Vec<UpstreamReply>) -> (String, HeldUpstream, JoinHandle<()>) {
    let upstream = HeldUpstream {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Arc::default(),
        dropped: Arc::new(Notify::new()),
    };
    let router = Router::new()
        .route("/codex/responses", post(held_reply))
        .with_state(upstream.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), upstream, task)
}

/// [`config`] with an override pricing [`MODEL`] at [`RATES`].
fn priced_config(upstream: &str) -> Config {
    let mut config = config(upstream);
    config.server.spend.as_mut().unwrap().pricing = Some(PricingConfig {
        multiplier: 1.0,
        overrides: vec![PricingOverride {
            upstream: "codex".to_string(),
            model: MODEL.to_string(),
            input: RATES.0,
            output: RATES.1,
            cache_read: RATES.2,
            cache_write: RATES.3,
        }],
    });
    config
}

fn cost(usage: Usage) -> u64 {
    Rates::from_usd_per_million(RATES.0, RATES.1, RATES.2, RATES.3).cost_femto_usd(&usage)
}

/// A turn with a real prompt to estimate.
async fn send_prompted_create(socket: &mut Socket, prompt: &str) {
    socket
        .send(Message::Text(
            json!({"type": "response.create", "generate": true, "model": MODEL,
                   "instructions": "You are a coding agent. Keep answers short and cite files.",
                   "input": [{"type": "message", "role": "user",
                              "content": [{"type": "input_text", "text": prompt}]}],
                   "tools": [{"type": "function", "name": "shell",
                              "parameters": {"type": "object"}}]})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

/// Reads the held turn's frames through its last delta, so the client has
/// been sent all of [`CUT_TEXT`].
async fn read_held_deltas(socket: &mut Socket) {
    let mut deltas = 0;
    while deltas < CUT_TEXT.len() {
        let frame = next_json(socket).await;
        assert_ne!(frame["type"], "error", "the turn failed: {frame}");
        if frame["type"] == "response.output_text.delta" {
            deltas += 1;
        }
    }
}

/// What the held turn's floor bills: the estimate of the request body the
/// gateway forwarded (and so retained), plus the delivered-text floor.
fn held_floor(request: &[u8]) -> u64 {
    let estimate = shunt::count_tokens::count_responses_input_tokens(request);
    let delivered: usize = CUT_TEXT.iter().map(|text| text.chars().count()).sum();
    let output = (delivered as u64).div_ceil(4);
    assert_eq!(output, 4);
    assert!(
        estimate > 30,
        "the request must estimate a real input: {estimate}"
    );
    let floor = cost(Usage {
        input_tokens: estimate,
        output_tokens: output,
        ..Usage::default()
    });
    assert!(
        floor
            > cost(Usage {
                output_tokens: output,
                ..Usage::default()
            })
    );
    floor
}

fn alice_spent(gateway: &Gateway) -> u64 {
    let now = now();
    let meter = gateway.state.gateway_stores.spend.meter();
    let current = meter.spent("alice", Period::Daily, now);
    if window(Period::Daily, now).start == window(Period::Daily, now - 120).start {
        current
    } else {
        current + meter.spent("alice", Period::Daily, now - 120)
    }
}

/// Waits, bounded, for alice's daily spend to read `expected`: the estimate
/// settles on the blocking pool after the gateway drops the upstream body.
async fn await_alice_spent(gateway: &Gateway, expected: u64, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while alice_spent(gateway) != expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: expected {expected} femto-USD, got {}",
            alice_spent(gateway)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_turn_the_client_closes_before_completion_bills_estimated_input_and_delivered_output() {
    let _env = env().await;
    let (uri, upstream, _task) = held_upstream(vec![UpstreamReply::Held]).await;
    let gateway = start(priced_config(&uri)).await;
    let mut socket = connect(&gateway).await;

    send_prompted_create(&mut socket, "Explain the spend floor.").await;
    read_held_deltas(&mut socket).await;
    drop(socket);
    tokio::time::timeout(Duration::from_secs(5), upstream.dropped.notified())
        .await
        .expect("the client's close did not drop the upstream body");

    let request = upstream.requests.lock().unwrap()[0].clone();
    await_alice_spent(&gateway, held_floor(&request), "the closed turn's floor").await;
}

#[tokio::test]
async fn a_turn_replaced_by_a_new_response_create_bills_its_floor() {
    let _env = env().await;
    let (uri, upstream, _task) =
        held_upstream(vec![UpstreamReply::Held, UpstreamReply::Completed]).await;
    let gateway = start(priced_config(&uri)).await;
    let mut socket = connect(&gateway).await;

    send_prompted_create(&mut socket, "Explain the spend floor.").await;
    read_held_deltas(&mut socket).await;
    send_prompted_create(&mut socket, "Now be briefer.").await;
    tokio::time::timeout(Duration::from_secs(5), upstream.dropped.notified())
        .await
        .expect("the replacement did not drop the first upstream body");
    expect_completed(&mut socket).await;

    let first = upstream.requests.lock().unwrap()[0].clone();
    let completed = cost(Usage {
        input_tokens: 10,
        output_tokens: 5,
        ..Usage::default()
    });
    await_alice_spent(
        &gateway,
        held_floor(&first) + completed,
        "the replaced turn's floor plus the completed turn",
    )
    .await;
}
