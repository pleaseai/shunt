//! `[server.spend]` metering of served `/v1/messages` responses (stage 2,
//! T003).
//!
//! Every test drives a real gateway against wiremock upstreams and reads the
//! principal's counters back from the meter. Expected costs are computed here
//! from the configured override rates through the public pricing types, never
//! by asking the code under test.
//!
//! Each alias also has a decoy override row keyed on its *client* id, priced
//! far above the real one: billing on the alias instead of the upstream model
//! would land on the decoy and fail the exact-equality assertions.
//!
//! Non-vacuity: drop the `SpendTap::for_request` call in `failover::forward`
//! (or pass `None` to the observer) and every billing test goes red on a zero
//! counter; point the committed stream's `set_target` at the first route and
//! the chain test goes red on the failed primary's rates.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::StatusCode;
use serde_json::json;
use shunt::{
    config::{
        AdminConfig, ApiKeyHeader, AuthMap, AuthMode, Config, InboundAuthConfig, ModelConfig,
        PricingConfig, PricingOverride, ProviderKind, SpendConfig, UpstreamAuth,
    },
    gateway::spend::{
        meter::{window, ANONYMOUS_PRINCIPAL, PERIODS},
        pricing::{Rates, Usage, WEB_SEARCH_LIST_PRICE_FEMTO_USD},
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

use judge_harness::{alias, upstream_with};

const KEY_ENV: &str = "SHUNT_TEST_METER_KEY";
const TOKENS_ENV: &str = "SHUNT_TEST_METER_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_METER_ADMIN";
const TOKEN: &str = "tok-a";
const PRINCIPAL: &str = "alice";

/// `(upstream, model, input, output, cache_read, cache_write)` USD per million.
type Row = (&'static str, &'static str, f64, f64, f64, f64);

const BILLED: Row = ("anth", "upstream-billed", 3.0, 15.0, 0.3, 3.75);
const TRANSLATED: Row = ("resp", "upstream-translated", 1.0, 2.0, 0.1, 1.25);
const CHAIN_WINNER: Row = ("anth", "upstream-chain-anth", 7.0, 11.0, 0.7, 9.0);
const CLASSIFIER: Row = ("anth", "upstream-classifier", 2.0, 4.0, 0.2, 2.5);
const DECOYS: [Row; 4] = [
    ("anth", "billed-alias", 100.0, 100.0, 100.0, 100.0),
    ("resp", "translated-alias", 100.0, 100.0, 100.0, 100.0),
    ("anth", "chain-alias", 100.0, 100.0, 100.0, 100.0),
    ("resp", "upstream-chain-resp", 100.0, 100.0, 100.0, 100.0),
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

fn injecting(name: &str, base_url: String, kind: ProviderKind) -> shunt::config::UpstreamConfig {
    let mut upstream = upstream_with(
        name,
        base_url,
        UpstreamAuth::Map(AuthMap::ApiKey {
            env: Some(KEY_ENV.to_string()),
            header: Some(ApiKeyHeader::Bearer),
        }),
    );
    upstream.kind = Some(kind);
    upstream
}

/// An injecting Anthropic upstream `anth`, an injecting Responses upstream
/// `resp`, and a passthrough Anthropic upstream `own` sharing `anth`'s URL.
/// `chain-alias` maps both injecting upstreams, `resp` first.
fn config(anth: &str, resp: &str, metered: bool) -> Config {
    config_with(anth, resp, metered, false)
}

/// [`config`], optionally pinning `anth`'s auto-mode classifier requests to
/// `upstream-classifier` (which has its own override row).
fn config_with(anth: &str, resp: &str, metered: bool, classifier: bool) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![
        injecting("resp", resp.to_string(), ProviderKind::Responses),
        injecting("anth", anth.to_string(), ProviderKind::Anthropic),
        upstream_with(
            "own",
            anth.to_string(),
            UpstreamAuth::Shorthand(AuthMode::Passthrough),
        ),
    ];
    if classifier {
        config.upstreams[1].classifier_model = Some("upstream-classifier".to_string());
    }
    config.server.default_provider = "own".to_string();
    config.models = vec![
        alias("billed-alias", "anth", "upstream-billed"),
        alias("translated-alias", "resp", "upstream-translated"),
        alias("own-alias", "own", "upstream-own"),
        ModelConfig {
            subagents: None,
            id: "chain-alias".to_string(),
            display_name: None,
            upstream_model: Some(
                [
                    ("resp".to_string(), "upstream-chain-resp".to_string()),
                    ("anth".to_string(), "upstream-chain-anth".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            router: None,
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
    if metered {
        let overrides = [BILLED, TRANSLATED, CHAIN_WINNER, CLASSIFIER]
            .into_iter()
            .chain(DECOYS)
            .map(row)
            .collect();
        config.server.spend = Some(SpendConfig {
            state_path: Some(std::path::PathBuf::new()),
            pricing: Some(PricingConfig {
                multiplier: 1.0,
                overrides,
            }),
            ..SpendConfig::default()
        });
    }
    config
        .validate()
        .expect("the metering config is well formed")
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
        (TOKENS_ENV, &format!("{PRINCIPAL}:{TOKEN}")),
        (ADMIN_ENV, "admin:admin-secret"),
    ])
    .await
}

/// The principal's spend in each of the three periods, in femto-USD.
fn spent(gateway: &Gateway, principal: &str) -> [u64; 3] {
    let meter = gateway.state.gateway_stores.spend.meter();
    let now = now();
    // Each charge lives in exactly one window per period. If a day, week or
    // month boundary fell since the charges were recorded they are split over
    // the two windows, so sum them; in one window, count it once.
    PERIODS.map(|period| {
        let current = meter.spent(principal, period, now);
        if window(period, now).start == window(period, now - 120).start {
            current
        } else {
            current + meter.spent(principal, period, now - 120)
        }
    })
}

async fn post(
    gateway: &Gateway,
    route: &str,
    model: &str,
    stream: bool,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}{route}", gateway.base_url))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(
            json!({"model": model, "max_tokens": 16, "stream": stream,
                   "messages": [{"role": "user", "content": "hi"}]})
            .to_string(),
        );
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

async fn messages(gateway: &Gateway, model: &str, stream: bool) -> reqwest::Response {
    post(
        gateway,
        "/v1/messages",
        model,
        stream,
        &[("x-shunt-token", TOKEN)],
    )
    .await
}

fn sse(frames: &[(&str, serde_json::Value)]) -> String {
    frames
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

/// A complete Anthropic stream: message_start reports the prompt, deltas
/// carry 14 chars of text, message_delta the final output and two web
/// searches.
fn completed_stream(model: &str) -> String {
    sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "model": model,
                "usage": {"input_tokens": 1000, "output_tokens": 1,
                          "cache_read_input_tokens": 200,
                          "cache_creation_input_tokens": 300}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Hello, world!!"}}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 500,
                             "server_tool_use": {"web_search_requests": 2}}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ])
}

fn completed_usage() -> Usage {
    Usage {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_input_tokens: 200,
        cache_creation_input_tokens: 300,
    }
}

fn sse_reply(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

async fn anthropic_upstream(reply: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(reply)
        .mount(&server)
        .await;
    server
}

/// An upstream that must never be called.
async fn unused_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_completed_stream_bills_its_usage_on_the_upstream_model_in_every_period() {
    let _env = env().await;
    let anth = anthropic_upstream(sse_reply(completed_stream("upstream-billed"))).await;
    let resp = unused_upstream().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    let response = messages(&gateway, "billed-alias", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();

    let expected =
        rates(BILLED).cost_femto_usd(&completed_usage()) + 2 * WEB_SEARCH_LIST_PRICE_FEMTO_USD;
    assert_ne!(
        expected,
        rates(DECOYS[0]).cost_femto_usd(&completed_usage()) + 2 * WEB_SEARCH_LIST_PRICE_FEMTO_USD,
        "the decoy row must price differently for this test to mean anything"
    );
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

/// A request carrying the auto-mode classifier's system prompt.
async fn classifier_request(gateway: &Gateway, stream: bool) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-shunt-token", TOKEN)
        .body(
            json!({"model": "billed-alias", "max_tokens": 16, "stream": stream,
                   "system": [{"type": "text",
                               "text": "You are a security monitor for autonomous AI coding agents."}],
                   "messages": [{"role": "user", "content": "hi"}]})
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_classifier_request_is_billed_on_the_classifier_model_it_was_sent_to() {
    let _env = env().await;
    let expected = |rates_row: Row| {
        rates(rates_row).cost_femto_usd(&completed_usage()) + 2 * WEB_SEARCH_LIST_PRICE_FEMTO_USD
    };
    assert_ne!(expected(CLASSIFIER), expected(BILLED), "rows must differ");

    // Streamed: the committed chain stream's winner is priced on the pin.
    let anth = anthropic_upstream(sse_reply(completed_stream("upstream-classifier"))).await;
    let resp = unused_upstream().await;
    let gateway = start(config_with(&anth.uri(), &resp.uri(), true, true)).await;
    let response = classifier_request(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_eq!(spent(&gateway, PRINCIPAL), [expected(CLASSIFIER); 3]);

    // Non-streamed: the same pin prices the buffered reply.
    let reply = json!({"id": "msg_1", "type": "message", "model": "upstream-classifier",
        "content": [{"type": "text", "text": "Hello"}],
        "usage": {"input_tokens": 1000, "output_tokens": 500,
                  "cache_read_input_tokens": 200, "cache_creation_input_tokens": 300,
                  "server_tool_use": {"web_search_requests": 2}}})
    .to_string();
    let anth =
        anthropic_upstream(ResponseTemplate::new(200).set_body_raw(reply, "application/json"))
            .await;
    let gateway = start(config_with(&anth.uri(), &resp.uri(), true, true)).await;
    let response = classifier_request(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_eq!(spent(&gateway, PRINCIPAL), [expected(CLASSIFIER); 3]);
}

#[tokio::test]
async fn a_non_streamed_reply_bills_its_usage_and_keeps_its_framing() {
    let _env = env().await;
    let reply = json!({"id": "msg_1", "type": "message", "model": "upstream-billed",
        "content": [{"type": "text", "text": "Hello"}],
        "usage": {"input_tokens": 1000, "output_tokens": 500,
                  "cache_read_input_tokens": 200, "cache_creation_input_tokens": 300,
                  "server_tool_use": {"web_search_requests": 2}}})
    .to_string();
    let anth = anthropic_upstream(
        ResponseTemplate::new(200).set_body_raw(reply.clone(), "application/json"),
    )
    .await;
    let resp = unused_upstream().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    let response = messages(&gateway, "billed-alias", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let length = response.headers()["content-length"].clone();
    let body = response.bytes().await.unwrap();
    assert_eq!(
        length,
        body.len().to_string().as_str(),
        "the tee keeps the body sized"
    );
    // The gateway echoes the client's id back in place of the upstream's.
    assert_eq!(
        body,
        reply.replace("upstream-billed", "billed-alias").as_bytes()
    );

    let expected =
        rates(BILLED).cost_femto_usd(&completed_usage()) + 2 * WEB_SEARCH_LIST_PRICE_FEMTO_USD;
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

/// Responses SSE: 1000 prompt tokens of which 200 cached, 40 output.
const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"output_tokens\":0}}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"delta\":\"translated text\"}\n\n",
    "event: response.output_text.done\n",
    "data: {}\n\n",
    "event: response.completed\n",
    "data: {\"response\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":40,",
    "\"input_tokens_details\":{\"cached_tokens\":200}}}}\n\n",
    "data: [DONE]\n\n"
);

fn translated_usage() -> Usage {
    Usage {
        input_tokens: 800,
        output_tokens: 40,
        cache_read_input_tokens: 200,
        cache_creation_input_tokens: 0,
    }
}

#[tokio::test]
async fn a_translated_adapter_stream_is_billed_on_its_upstream_model() {
    let _env = env().await;
    let anth = unused_upstream().await;
    let resp = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse_reply(RESPONSES_SSE.to_string()))
        .mount(&resp)
        .await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    let response = messages(&gateway, "translated-alias", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("translated text"), "{body}");

    let expected = rates(TRANSLATED).cost_femto_usd(&translated_usage());
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

#[tokio::test]
async fn a_committed_stream_is_billed_on_the_winner_not_the_failed_primary() {
    let _env = env().await;
    let anth = anthropic_upstream(sse_reply(completed_stream("upstream-chain-anth"))).await;
    let resp = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&resp)
        .await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    let response = messages(&gateway, "chain-alias", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    // The committed path omits the upstream-naming headers; this is how a
    // test knows it took that path rather than the ordered loop.
    assert!(response.headers().get("x-gateway-upstream-model").is_none());
    let body = response.text().await.unwrap();
    assert!(body.contains("Hello, world!!"), "{body}");

    let expected = rates(CHAIN_WINNER).cost_femto_usd(&completed_usage())
        + 2 * WEB_SEARCH_LIST_PRICE_FEMTO_USD;
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
    resp.verify().await;
}

#[tokio::test]
async fn a_stream_ending_without_its_final_usage_bills_a_floor_from_delivered_text() {
    let _env = env().await;
    // message_start and 15 chars of text, then the stream ends without a message_delta.
    let cut = sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "usage": {"input_tokens": 300, "output_tokens": 1}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "0123456789"}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "abcde"}}),
        ),
    ]);
    let anth = anthropic_upstream(sse_reply(cut)).await;
    let resp = unused_upstream().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    messages(&gateway, "billed-alias", true)
        .await
        .bytes()
        .await
        .unwrap();

    let floor = Usage {
        input_tokens: 300,
        output_tokens: 4,
        ..Usage::default()
    };
    let expected = rates(BILLED).cost_femto_usd(&floor);
    assert!(
        expected
            > rates(BILLED).cost_femto_usd(&Usage {
                input_tokens: 300,
                output_tokens: 1,
                ..Usage::default()
            })
    );
    assert_eq!(spent(&gateway, PRINCIPAL), [expected; 3]);
}

/// The client receives exactly the same bytes whether or not the meter is
/// on, and — for a first-party relay — exactly what the upstream sent. The
/// upstream already names the client's id, so the gateway's model echo
/// rewrite is the identity here.
#[tokio::test]
async fn metering_never_changes_the_bytes_a_client_receives() {
    let _env = env().await;
    let stream = completed_stream("billed-alias");
    let reply = r#"{"id":"msg_1","type":"message","usage":{"input_tokens":3,"output_tokens":2}}"#;
    let mut bodies = Vec::new();
    for metered in [true, false] {
        let anth = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::body_string_contains("\"stream\":true"))
            .respond_with(sse_reply(stream.clone()))
            .mount(&anth)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(reply, "application/json"))
            .mount(&anth)
            .await;
        let resp = unused_upstream().await;
        let gateway = start(config(&anth.uri(), &resp.uri(), metered)).await;
        let streamed = messages(&gateway, "billed-alias", true).await;
        let streamed = streamed.bytes().await.unwrap();
        let whole = messages(&gateway, "billed-alias", false).await;
        let whole = whole.bytes().await.unwrap();
        let billed = spent(&gateway, PRINCIPAL)[0];
        assert_eq!(billed > 0, metered, "metered = {metered}");
        bodies.push((streamed, whole));
    }

    assert_eq!(bodies[0], bodies[1], "meter on and off differ");
    assert_eq!(bodies[0].0, stream.as_bytes(), "the stream was altered");
    assert_eq!(bodies[0].1, reply.as_bytes(), "the JSON body was altered");
}

/// A usage block the meter cannot read is a metering failure, not a response
/// failure: the reply is served whole, and a byte floor is billed, not zero.
#[tokio::test]
async fn an_unreadable_usage_block_still_serves_the_reply_and_bills_a_floor() {
    let _env = env().await;
    let reply = r#"{"id":"msg_1","type":"message","usage":{"input_tokens":"lots"}}"#;
    let anth =
        anthropic_upstream(ResponseTemplate::new(200).set_body_raw(reply, "application/json"))
            .await;
    let resp = unused_upstream().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    let response = messages(&gateway, "billed-alias", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), reply.as_bytes());

    let floor = Usage {
        output_tokens: (reply.len() as u64).div_ceil(4),
        ..Usage::default()
    };
    assert_eq!(
        spent(&gateway, PRINCIPAL),
        [rates(BILLED).cost_femto_usd(&floor); 3]
    );
}

#[tokio::test]
async fn count_tokens_and_an_all_passthrough_chain_bill_nothing() {
    let _env = env().await;
    let anth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 10})))
        .expect(1)
        .mount(&anth)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_reply(completed_stream("upstream-own")))
        .expect(2)
        .mount(&anth)
        .await;
    let resp = unused_upstream().await;
    let gateway = start(config(&anth.uri(), &resp.uri(), true)).await;

    let counted = post(
        &gateway,
        "/v1/messages/count_tokens",
        "billed-alias",
        false,
        &[("x-shunt-token", TOKEN)],
    )
    .await;
    assert_eq!(counted.status(), StatusCode::OK);
    counted.bytes().await.unwrap();

    // The caller's own key: with and without an identity, nothing is metered.
    let own_key = ("x-api-key", "sk-ant-caller-own-key");
    for headers in [vec![own_key], vec![own_key, ("x-shunt-token", TOKEN)]] {
        let served = post(&gateway, "/v1/messages", "own-alias", true, &headers).await;
        assert_eq!(served.status(), StatusCode::OK);
        served.bytes().await.unwrap();
    }

    assert_eq!(spent(&gateway, PRINCIPAL), [0; 3]);
    assert_eq!(spent(&gateway, ANONYMOUS_PRINCIPAL), [0; 3]);
    anth.verify().await;
}
