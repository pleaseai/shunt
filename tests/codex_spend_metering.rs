//! `[server.spend]` metering of streamed inbound Codex endpoint turns
//! (codex-spend-caps, T004) — the Responses-protocol companion to
//! `tests/spend_metering.rs`.
//!
//! Every test drives a real gateway against mock upstreams and reads the
//! principal's counters back from the meter. Expected costs are computed here
//! from the configured override rates through the public pricing types, never
//! by asking the code under test.
//!
//! Each priced model also has decoy override rows priced far above the real
//! one: keyed on the client model where the route rewrites it, and on the
//! wrong provider. Pricing by the wrong key lands on a decoy and fails the
//! exact-equality assertions.
//!
//! Non-vacuity: pass `None` instead of the tap to `record_served_outcome` in
//! `codex_endpoint::forward_turn` and every billing test goes red on a zero
//! counter; bill the Responses `input_tokens` without subtracting the cached
//! tokens and the cached-split assertions go red.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::json;
use shunt::{
    config::{
        AccountConfig, AdminConfig, AuthMode, CodexEndpointConfig, CodexRouteConfig, Config,
        InboundAuthConfig, PricingConfig, PricingOverride, SpendConfig,
    },
    gateway::spend::{
        meter::{window, PERIODS},
        pricing::{Rates, Usage},
    },
    server::{self, AppState},
};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tungstenite::client::IntoClientRequest;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

const TOKENS_ENV: &str = "SHUNT_TEST_CODEX_METER_TOKENS";
const ACCOUNT_ENV: &str = "SHUNT_TEST_CODEX_METER_ACCOUNT";
const OPENAI_KEY_ENV: &str = "SHUNT_TEST_CODEX_METER_OPENAI_KEY";
const ANTHROPIC_KEY_ENV: &str = "SHUNT_TEST_CODEX_METER_ANTHROPIC_KEY";
const ADMIN_ENV: &str = "SHUNT_TEST_CODEX_METER_ADMIN";
const TOKEN: &str = "tok-a";
const PRINCIPAL: &str = "alice";

/// The model the default pool serves, the routed client model and the
/// upstream model its route rewrites it to, and a model no row prices.
const POOL_MODEL: &str = "gpt-5.6-sol";
const ROUTED_MODEL: &str = "gpt-routed";
const VENDOR_MODEL: &str = "vendor-model";
const UNPRICED_MODEL: &str = "gpt-unpriced";
const CLAUDE_MODEL: &str = "claude-sonnet-4-6";

/// `(upstream, model, input, output, cache_read, cache_write)` USD per million.
type Row = (&'static str, &'static str, f64, f64, f64, f64);

const POOL: Row = ("codex", POOL_MODEL, 2.0, 8.0, 0.2, 2.5);
const ROUTED: Row = ("openai", VENDOR_MODEL, 1.5, 6.0, 0.15, 1.875);
const CLAUDE: Row = ("anthropic", CLAUDE_MODEL, 3.0, 15.0, 0.3, 3.75);
const DECOYS: [Row; 3] = [
    // The pool model under the wrong provider.
    ("openai", POOL_MODEL, 100.0, 100.0, 100.0, 100.0),
    // The routed turn priced by its client model instead of the upstream one.
    ("openai", ROUTED_MODEL, 100.0, 100.0, 100.0, 100.0),
    // The routed upstream model under the endpoint's default provider.
    ("codex", VENDOR_MODEL, 100.0, 100.0, 100.0, 100.0),
];

/// The unknown-model rate (`$5 / $25 / $0.50 / $6.25` per million).
fn unknown_rates() -> Rates {
    Rates::from_usd_per_million(5.0, 25.0, 0.5, 6.25)
}

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

fn rates(row: Row) -> Rates {
    Rates::from_usd_per_million(row.2, row.3, row.4, row.5)
}

fn row(row: Row) -> PricingOverride {
    PricingOverride {
        upstream: row.0.to_string(),
        model: row.1.to_string(),
        input: row.2,
        output: row.3,
        cache_read: row.4,
        cache_write: row.5,
    }
}

fn chatgpt_token() -> String {
    let payload = json!({
        "exp": now() + 3_600,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-meter"}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// The built-in `codex` pool behind the endpoint, a route from
/// [`ROUTED_MODEL`] to the injecting `openai` provider's [`VENDOR_MODEL`], and
/// an injecting `anthropic` provider for `/v1/messages`, all on `upstream`.
fn config(upstream: &str, metered: bool) -> Config {
    let mut config = Config::default();
    let codex = config.providers.get_mut("codex").unwrap();
    codex.base_url = upstream.to_string();
    codex.accounts = vec![AccountConfig {
        name: "account".to_string(),
        token_env: Some(ACCOUNT_ENV.to_string()),
        ..Default::default()
    }];
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = upstream.to_string();
    openai.api_key_env = Some(OPENAI_KEY_ENV.to_string());
    let anthropic = config.providers.get_mut("anthropic").unwrap();
    anthropic.base_url = upstream.to_string();
    anthropic.auth = AuthMode::ApiKey;
    anthropic.api_key_env = Some(ANTHROPIC_KEY_ENV.to_string());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: ROUTED_MODEL.to_string(),
            provider: "openai".to_string(),
            upstream_model: Some(VENDOR_MODEL.to_string()),
        }],
    });
    config.server.auth = Some(InboundAuthConfig {
        jwt: Vec::new(),
        header: "x-shunt-token".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
        ..Default::default()
    });
    // `[server.spend]` requires the admin API that manages its caps.
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
    if metered {
        config.server.spend = Some(SpendConfig {
            state_path: Some(std::path::PathBuf::new()),
            pricing: Some(PricingConfig {
                multiplier: 1.0,
                overrides: [POOL, ROUTED, CLAUDE]
                    .into_iter()
                    .chain(DECOYS)
                    .map(row)
                    .collect(),
            }),
            ..SpendConfig::default()
        });
    }
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
        (TOKENS_ENV, &format!("{PRINCIPAL}:{TOKEN}")),
        (ACCOUNT_ENV, &chatgpt_token()),
        (OPENAI_KEY_ENV, "openai-key"),
        (ANTHROPIC_KEY_ENV, "anthropic-key"),
        (ADMIN_ENV, "admin:admin-secret"),
    ])
    .await
}

/// The principal's spend in each of the three periods, in femto-USD.
fn spent(gateway: &Gateway, principal: &str) -> [u64; 3] {
    let meter = gateway.state.gateway_stores.spend.meter();
    let now = now();
    // Each charge lives in exactly one window per period. If a boundary fell
    // since the charges were recorded they are split over the two windows, so
    // sum them; in one window, count it once.
    PERIODS.map(|period| {
        let current = meter.spent(principal, period, now);
        if window(period, now).start == window(period, now - 120).start {
            current
        } else {
            current + meter.spent(principal, period, now - 120)
        }
    })
}

/// A complete Responses stream: 12 chars of output text, then a
/// `response.completed` reporting 1000 input tokens of which 600 were cached,
/// and 50 output tokens (20 of them reasoning, already inside the 50).
fn completed_stream(input: u64, cached: u64, output: u64) -> String {
    [
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "Hello, world"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {
                "input_tokens": input,
                "input_tokens_details": {"cached_tokens": cached},
                "output_tokens": output,
                "output_tokens_details": {"reasoning_tokens": 20},
                "total_tokens": input + output}}}),
        ),
    ]
    .iter()
    .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
    .collect()
}

/// [`completed_stream`]'s usage split for pricing: the cached tokens out of
/// the input.
fn completed_usage() -> Usage {
    Usage {
        input_tokens: 400,
        output_tokens: 50,
        cache_read_input_tokens: 600,
        cache_creation_input_tokens: 0,
    }
}

fn standard_stream() -> String {
    completed_stream(1000, 600, 50)
}

/// A mock upstream answering `route` with `body` as SSE, `expect` times.
async fn mount(server: &MockServer, route: &str, body: String, expect: u64) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(expect)
        .mount(server)
        .await;
}

async fn post_turn(gateway: &Gateway, model: &str) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/backend-api/codex/responses", gateway.base_url))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("session-id", "sess-meter")
        .body(
            json!({"model": model, "instructions": "be brief", "stream": true,
                   "store": false,
                   "input": [{"type": "message", "role": "user",
                              "content": [{"type": "input_text", "text": "hi"}]}]})
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_completed_pool_turn_bills_cached_tokens_at_the_cache_read_rate_in_every_period() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    let body = standard_stream();
    mount(&upstream, "/codex/responses", body.clone(), 1).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = post_turn(&gateway, POOL_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    let relayed = response.bytes().await.unwrap();
    assert_eq!(relayed, body.as_bytes(), "the relayed bytes are unchanged");

    let expected = rates(POOL).cost_femto_usd(&completed_usage());
    let uncached = rates(POOL).cost_femto_usd(&Usage {
        input_tokens: 1000,
        output_tokens: 50,
        ..Usage::default()
    });
    assert_ne!(expected, uncached, "the cached split must change the price");
    assert_ne!(
        expected,
        rates(DECOYS[0]).cost_femto_usd(&completed_usage()),
        "the decoy row must price differently for this test to mean anything"
    );
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

#[tokio::test]
async fn more_cached_than_input_tokens_bills_zero_uncached_input() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    mount(
        &upstream,
        "/codex/responses",
        completed_stream(10, 25, 50),
        1,
    )
    .await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = post_turn(&gateway, POOL_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();

    let expected = rates(POOL).cost_femto_usd(&Usage {
        input_tokens: 0,
        output_tokens: 50,
        cache_read_input_tokens: 25,
        cache_creation_input_tokens: 0,
    });
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

#[tokio::test]
async fn a_routed_turn_is_priced_by_its_provider_and_upstream_model() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    let body = standard_stream();
    mount(&upstream, "/responses", body.clone(), 1).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = post_turn(&gateway, ROUTED_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), body.as_bytes());

    let expected = rates(ROUTED).cost_femto_usd(&completed_usage());
    for decoy in &DECOYS[1..] {
        assert_ne!(expected, rates(*decoy).cost_femto_usd(&completed_usage()));
    }
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

#[tokio::test]
async fn a_gpt_model_with_no_override_is_billed_at_the_unknown_model_rate() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    mount(&upstream, "/codex/responses", standard_stream(), 1).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = post_turn(&gateway, UNPRICED_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();

    let expected = unknown_rates().cost_femto_usd(&completed_usage());
    assert_ne!(expected, rates(POOL).cost_femto_usd(&completed_usage()));
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

/// A complete Anthropic stream reporting 100 input and 40 output tokens.
fn anthropic_stream() -> String {
    [
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1",
                "model": CLAUDE_MODEL,
                "usage": {"input_tokens": 100, "output_tokens": 1}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Hi"}}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 40}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]
    .iter()
    .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
    .collect()
}

#[tokio::test]
async fn messages_and_codex_spend_land_in_one_counter() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    mount(&upstream, "/v1/messages", anthropic_stream(), 1).await;
    mount(&upstream, "/codex/responses", standard_stream(), 1).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-shunt-token", TOKEN)
        .body(
            json!({"model": CLAUDE_MODEL, "max_tokens": 16, "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]})
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let messages = rates(CLAUDE).cost_femto_usd(&Usage {
        input_tokens: 100,
        output_tokens: 40,
        ..Usage::default()
    });
    assert_eq!(spent(&gateway, PRINCIPAL), [messages; 3]);

    let response = post_turn(&gateway, POOL_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let codex = rates(POOL).cost_femto_usd(&completed_usage());
    assert_eq!(spent(&gateway, PRINCIPAL), [messages + codex; 3]);
}

#[tokio::test]
async fn without_spend_config_a_turn_relays_unchanged_and_records_nothing() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    let body = standard_stream();
    mount(&upstream, "/codex/responses", body.clone(), 1).await;
    let gateway = start(config(&upstream.uri(), false)).await;

    let response = post_turn(&gateway, POOL_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), body.as_bytes());
    assert_eq!(spent(&gateway, PRINCIPAL), [0; 3]);
}

/// A WebSocket turn is metered through the same served-response observer:
/// one completed turn adds its priced usage to the principal's counter.
#[tokio::test]
async fn a_completed_websocket_turn_is_metered() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    mount(&upstream, "/codex/responses", standard_stream(), 1).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let mut request = format!("ws://{}/v1/responses", gateway.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let mut socket = connect_async(request).await.unwrap().0;
    socket
        .send(Message::Text(
            json!({"type": "response.create", "generate": true, "model": POOL_MODEL,
                   "input": []})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("timed out waiting for a websocket frame")
            .expect("socket ended")
            .expect("websocket read failed");
        let frame: serde_json::Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert_ne!(frame["type"], "error", "the turn failed: {frame}");
        if frame["type"] == "response.completed" {
            break;
        }
    }

    // The relay settles the bill when it drains the upstream body, which may
    // trail the last frame the client reads.
    let expected = rates(POOL).cost_femto_usd(&completed_usage());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while spent(&gateway, PRINCIPAL) != [expected; 3] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the websocket turn was not metered: {:?}",
            spent(&gateway, PRINCIPAL)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The floor's delivered text: 13 characters, so a 4-token output floor.
const CUT_TEXT: [&str; 2] = ["Hello, ", "world!"];

/// An upstream that sends a Responses stream's opening and [`CUT_TEXT`] as
/// output deltas, then breaks the connection mid-turn: no terminal frame,
/// so no usage.
async fn broken_upstream() -> (SocketAddr, JoinHandle<()>) {
    async fn reply() -> axum::response::Response {
        let mut frames = vec![format!(
            "event: response.created\ndata: {}\n\n",
            json!({"type": "response.created", "response": {"id": "resp_cut"}})
        )];
        frames.extend(CUT_TEXT.map(|text| {
            format!(
                "event: response.output_text.delta\ndata: {}\n\n",
                json!({"type": "response.output_text.delta", "delta": text})
            )
        }));
        let frames = futures_util::stream::iter(
            frames
                .into_iter()
                .map(|frame| Ok(axum::body::Bytes::from(frame))),
        );
        // The break trails the deltas, so they are relayed before it lands.
        let cut = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(std::io::Error::other("upstream broke mid-turn"))
        });
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(frames.chain(cut)))
            .unwrap()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new().route("/codex/responses", axum::routing::post(reply));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, task)
}

/// Waits, bounded, for the principal's counters to read `expected`: the
/// floor's input estimate settles on the blocking pool after the stream ends.
async fn await_spent(gateway: &Gateway, expected: u64, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while spent(gateway, PRINCIPAL) != [expected; 3] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: expected {expected} femto-USD in every period, got {:?}",
            spent(gateway, PRINCIPAL)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A 2xx Responses stream broken before `response.completed` reported no
/// usage, so it bills the request's estimated input plus the delivered-text
/// floor: Responses reports input only at completion, so there is no earlier
/// count to bill.
///
/// Non-vacuity: bill zero input in `StreamSpend::settle` and this goes red on
/// the output-only cost.
#[tokio::test]
async fn an_http_stream_broken_mid_turn_bills_estimated_input_plus_delivered_output() {
    let _env = env().await;
    let (upstream, _upstream_task) = broken_upstream().await;
    let gateway = start(config(&format!("http://{upstream}"), true)).await;
    let body = json!({
        "model": POOL_MODEL,
        "instructions": "You are a coding agent. Keep answers short and cite files.",
        "stream": true,
        "store": false,
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Explain the spend floor."}]},
            {"type": "function_call", "name": "shell", "call_id": "c1",
             "arguments": "{\"cmd\":[\"cat\",\"README.md\"]}"},
            {"type": "function_call_output", "call_id": "c1", "output": "# shunt"}
        ],
        "tools": [{"type": "function", "name": "shell",
                   "parameters": {"type": "object", "properties": {"cmd": {"type": "array"}}}}]
    })
    .to_string();

    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/backend-api/codex/responses", gateway.base_url))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("session-id", "sess-meter")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The relay ends with the upstream's break; how the client sees the cut
    // is not what this test pins.
    let _ = response.bytes().await;

    let estimate = shunt::count_tokens::count_responses_input_tokens(body.as_bytes());
    let delivered: usize = CUT_TEXT.iter().map(|text| text.chars().count()).sum();
    let output = (delivered as u64).div_ceil(4);
    assert_eq!(output, 4);
    let expected = rates(POOL).cost_femto_usd(&Usage {
        input_tokens: estimate,
        output_tokens: output,
        ..Usage::default()
    });
    let output_only = rates(POOL).cost_femto_usd(&Usage {
        output_tokens: output,
        ..Usage::default()
    });
    assert!(
        estimate > 40,
        "the request must estimate a real input: {estimate}"
    );
    assert!(expected > output_only);
    await_spent(&gateway, expected, "the broken stream's floor").await;
}

/// A non-streamed Responses reply: the same usage as [`standard_stream`], in
/// the `application/json` body a `stream:false` turn gets from a stock
/// Responses upstream.
fn json_response() -> String {
    json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "Hello, world"}]}],
        "usage": {
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 600},
            "output_tokens": 50,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 1050},
    })
    .to_string()
}

async fn mount_json(server: &MockServer, route: &str, body: String) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_non_streamed_routed_turn_is_billed_from_its_usage_with_the_cached_split() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    let body = json_response();
    mount_json(&upstream, "/responses", body.clone()).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = post_turn(&gateway, ROUTED_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/json",
        "the reply is not SSE"
    );
    assert_eq!(
        response.bytes().await.unwrap(),
        body.as_bytes(),
        "the relayed bytes are unchanged"
    );

    let expected = rates(ROUTED).cost_femto_usd(&completed_usage());
    let uncached = rates(ROUTED).cost_femto_usd(&Usage {
        input_tokens: 1000,
        output_tokens: 50,
        ..Usage::default()
    });
    assert_ne!(expected, uncached, "the cached split must change the price");
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

#[tokio::test]
async fn a_non_streamed_pool_turn_is_billed_from_its_usage_with_the_cached_split() {
    let _env = env().await;
    let upstream = MockServer::start().await;
    let body = json_response();
    mount_json(&upstream, "/codex/responses", body.clone()).await;
    let gateway = start(config(&upstream.uri(), true)).await;

    let response = post_turn(&gateway, POOL_MODEL).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), body.as_bytes());

    let expected = rates(POOL).cost_femto_usd(&completed_usage());
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}
