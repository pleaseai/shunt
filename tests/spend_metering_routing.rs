//! `[server.spend]` metering of routing side calls and gated turns (stage 2,
//! T004).
//!
//! One wiremock server stands in for three credential-injecting upstreams —
//! `strong`, `weak` and `judge` — told apart by the body's `model`, each with
//! its own usage and its own override rates. Every test asserts that the
//! requesting principal's counters equal the sum of the mocked usages, each
//! priced on the upstream model that produced it, so a call billed twice, not
//! at all, or to the wrong principal or model fails an exact equality.
//!
//! Non-vacuity: drop the `bill_json` call in `routing::serve::dispatch` and
//! every test goes red by the judge's cost; drop the capture billing in
//! `routing::serve::gated` and both gated tests go red by the weak turn's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        AdminConfig, ApiKeyHeader, AuthMap, AuthMode, Config, InboundAuthConfig, ModelConfig,
        PricingConfig, PricingOverride, RouterConfig, SpendConfig, UpstreamAuth,
    },
    gateway::spend::{
        meter::PERIODS,
        pricing::{Rates, Usage},
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

const KEY_ENV: &str = "SHUNT_TEST_ROUTED_METER_KEY";
const TOKENS_ENV: &str = "SHUNT_TEST_ROUTED_METER_TOKENS";
const ADMIN_ENV: &str = "SHUNT_TEST_ROUTED_METER_ADMIN";
const TOKEN: &str = "tok-a";
const PRINCIPAL: &str = "alice";
const ROUTER_ID: &str = "claude-auto";
const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

/// One upstream tier: its upstream model, override rates (USD per million:
/// input, output, cache read, cache write) and the usage its mock reports.
struct Tier {
    upstream: &'static str,
    model: &'static str,
    rates: (f64, f64, f64, f64),
    input: u64,
    output: u64,
}

const STRONG: Tier = Tier {
    upstream: "strong",
    model: "upstream-strong",
    rates: (6.0, 30.0, 0.6, 7.5),
    input: 300,
    output: 70,
};
const WEAK: Tier = Tier {
    upstream: "weak",
    model: "upstream-weak",
    rates: (2.0, 4.0, 0.2, 2.5),
    input: 50,
    output: 20,
};
const JUDGE: Tier = Tier {
    upstream: "judge",
    model: "upstream-judge",
    rates: (1.0, 5.0, 0.1, 1.25),
    input: 1200,
    output: 91,
};

impl Tier {
    fn cost(&self) -> u64 {
        let (input, output, cache_read, cache_write) = self.rates;
        Rates::from_usd_per_million(input, output, cache_read, cache_write).cost_femto_usd(&Usage {
            input_tokens: self.input,
            output_tokens: self.output,
            ..Usage::default()
        })
    }

    fn usage(&self) -> Value {
        json!({"input_tokens": self.input, "output_tokens": self.output})
    }

    fn pricing_row(&self) -> PricingOverride {
        let (input, output, cache_read, cache_write) = self.rates;
        PricingOverride {
            upstream: self.upstream.to_string(),
            model: self.model.to_string(),
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    /// A whole non-streamed message from this tier.
    fn json_reply(&self, text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": format!("msg_{}", self.upstream),
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": self.usage(),
        }))
    }

    /// A whole stream from this tier: prompt usage on `message_start`, the
    /// final output on `message_delta`.
    fn sse_reply(&self, text: &str) -> ResponseTemplate {
        let frames = [
            (
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": format!("msg_{}", self.upstream), "type": "message",
                    "role": "assistant", "model": self.model, "content": [],
                    "usage": {"input_tokens": self.input, "output_tokens": 1}}}),
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
                       "usage": {"output_tokens": self.output}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let body: String = frames
            .iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect();
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    /// This tier's mock, answering `expect` times with `reply`.
    fn mock(&self, reply: ResponseTemplate, expect: u64) -> Mock {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(ModelIs(self.model))
            .respond_with(reply)
            .expect(expect)
    }
}

/// The judge's text answer, as a whole message carrying [`JUDGE`]'s usage.
fn verdict(verdict: Value) -> ResponseTemplate {
    JUDGE.json_reply(&verdict.to_string())
}

const CAPABILITY_ROUTER: &str = r#"
type = "llm_classifier"
mode = "capability"
classifier_target = "judge-alias"
strong_target = "strong-alias"
weak_target = "weak-alias"
base_threshold = 0.5
judge_timeout_ms = 2000
"#;

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

fn config(upstream: &MockServer, router: &str) -> Config {
    config_with(upstream, router, false)
}

/// [`config`], with the weak tier passthrough when `weak_passthrough`: a
/// mixed envelope, where the injecting judge makes the request metered but a
/// turn the weak tier serves is paid with the caller's own credential.
fn config_with(upstream: &MockServer, router: &str, weak_passthrough: bool) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = [STRONG, WEAK, JUDGE]
        .iter()
        .map(|tier| {
            let auth = if weak_passthrough && tier.upstream == WEAK.upstream {
                UpstreamAuth::Shorthand(AuthMode::Passthrough)
            } else {
                UpstreamAuth::Map(AuthMap::ApiKey {
                    env: Some(KEY_ENV.to_string()),
                    header: Some(ApiKeyHeader::XApiKey),
                })
            };
            upstream_with(tier.upstream, upstream.uri(), auth)
        })
        .collect();
    config.server.default_provider = "strong".to_string();
    config.models = vec![
        ModelConfig {
            subagents: None,
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(toml::from_str::<RouterConfig>(router).expect("the router parses")),
            stage_router: None,
        },
        alias("strong-alias", "strong", STRONG.model),
        alias("weak-alias", "weak", WEAK.model),
        alias("judge-alias", "judge", JUDGE.model),
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
    config.server.spend = Some(SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        pricing: Some(PricingConfig {
            multiplier: 1.0,
            overrides: [STRONG, WEAK, JUDGE]
                .iter()
                .map(Tier::pricing_row)
                .collect(),
        }),
        ..SpendConfig::default()
    });
    config
        .validate()
        .expect("the routed metering config is well formed")
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

/// One turn on the router id, as `alice`.
async fn turn(gateway: &Gateway, stream: bool) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", TOKEN)
        .header("x-claude-code-session-id", SESSION)
        .body(
            json!({"model": ROUTER_ID, "max_tokens": 16, "stream": stream,
                   "messages": [{"role": "user", "content": [
                       {"type": "text", "text": "add a --json flag"}]}]})
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

/// The principal's spend in each of the three periods, in femto-USD.
fn spent(gateway: &Gateway) -> [u64; 3] {
    let meter = gateway.state.gateway_stores.spend.meter();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // A charge lives in exactly one window per period; across a day, week or
    // month boundary since it was recorded it sits in the earlier one.
    PERIODS.map(|period| {
        meter
            .spent(PRINCIPAL, period, now)
            .max(meter.spent(PRINCIPAL, period, now - 120))
    })
}

fn source(response: &reqwest::Response) -> String {
    response.headers()["x-gateway-route-source"]
        .to_str()
        .unwrap()
        .to_string()
}

/// The judge's call is billed to the caller who asked, on the judge's own
/// upstream model, beside the turn it routed.
#[tokio::test]
async fn a_judge_routed_turn_bills_the_judge_call_and_the_served_turn() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    // `p_solve` under the threshold: the weak tier is not trusted, so strong.
    JUDGE
        .mock(
            verdict(json!({"crux": "bounded task", "primary_rule": "SUP-1",
                             "capability_boundary": "supported", "p_solve": 0.1})),
            1,
        )
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.json_reply("STRONG-ANSWER"), 1)
        .mount(&upstream)
        .await;
    WEAK.mock(WEAK.json_reply("WEAK-ANSWER"), 0)
        .mount(&upstream)
        .await;
    let gateway = start(config(&upstream, CAPABILITY_ROUTER)).await;

    let response = turn(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-routed-model"], "strong-alias");
    assert!(response.text().await.unwrap().contains("STRONG-ANSWER"));

    let expected = JUDGE.cost() + STRONG.cost();
    assert!(JUDGE.cost() > 0 && STRONG.cost() > 0);
    assert_eq!(spent(&gateway), [expected; 3]);
    upstream.verify().await;
}

/// A declined escalation replays the retained weak turn: billed once, at its
/// capture, plus the judge — never again when the replay is served.
#[tokio::test]
async fn a_replayed_gated_turn_is_billed_once() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    WEAK.mock(WEAK.sse_reply("WEAK-ANSWER"), 1)
        .mount(&upstream)
        .await;
    JUDGE
        .mock(
            verdict(json!({"escalate": false, "reason": "progressing"})),
            1,
        )
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.sse_reply("STRONG-ANSWER"), 0)
        .mount(&upstream)
        .await;
    let gateway = start(config(&upstream, ESCALATION_ROUTER)).await;

    let response = turn(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(source(&response), "escalation_weak");
    assert!(response.text().await.unwrap().contains("WEAK-ANSWER"));

    assert_eq!(spent(&gateway), [WEAK.cost() + JUDGE.cost(); 3]);
    upstream.verify().await;
}

/// An escalated turn discards the retained weak turn — billed once at its
/// capture though the caller never sees it — and serves the strong tier live,
/// billed once by the served-response hook.
#[tokio::test]
async fn a_turn_discarded_for_escalation_and_the_escalated_turn_are_each_billed_once() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    WEAK.mock(WEAK.json_reply("WEAK-ANSWER"), 1)
        .mount(&upstream)
        .await;
    JUDGE
        .mock(verdict(json!({"escalate": true, "reason": "stuck"})), 1)
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.json_reply("STRONG-ANSWER"), 1)
        .mount(&upstream)
        .await;
    let gateway = start(config(&upstream, ESCALATION_ROUTER)).await;

    let response = turn(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-routed-model"], "strong-alias");
    let body = response.text().await.unwrap();
    assert!(body.contains("STRONG-ANSWER"), "{body}");

    assert_eq!(
        spent(&gateway),
        [WEAK.cost() + JUDGE.cost() + STRONG.cost(); 3]
    );
    upstream.verify().await;
}

fn capability_verdict(p_solve: f64) -> ResponseTemplate {
    verdict(json!({"crux": "bounded task", "primary_rule": "SUP-1",
                   "capability_boundary": "supported", "p_solve": p_solve}))
}

/// A mixed envelope: the injecting judge makes the request metered, but the
/// turn it routes to the passthrough weak tier is paid with the caller's own
/// credential, so only the judge is billed. The positive twin below is the
/// same deployment routed to the injecting strong tier.
#[tokio::test]
async fn a_turn_served_by_a_passthrough_tier_bills_only_the_injecting_judge() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    // `p_solve` over the threshold: the weak tier is trusted with the task.
    JUDGE
        .mock(capability_verdict(0.9), 1)
        .mount(&upstream)
        .await;
    WEAK.mock(WEAK.json_reply("WEAK-ANSWER"), 1)
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.json_reply("STRONG-ANSWER"), 0)
        .mount(&upstream)
        .await;
    let gateway = start(config_with(&upstream, CAPABILITY_ROUTER, true)).await;

    let response = turn(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-routed-model"], "weak-alias");
    assert!(response.text().await.unwrap().contains("WEAK-ANSWER"));

    assert_eq!(spent(&gateway), [JUDGE.cost(); 3]);
    upstream.verify().await;
}

/// Positive twin of the test above: the same mixed deployment, routed to the
/// injecting strong tier, bills the judge and the turn.
#[tokio::test]
async fn the_same_mixed_envelope_bills_a_turn_served_by_an_injecting_tier() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    JUDGE
        .mock(capability_verdict(0.1), 1)
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.json_reply("STRONG-ANSWER"), 1)
        .mount(&upstream)
        .await;
    WEAK.mock(WEAK.json_reply("WEAK-ANSWER"), 0)
        .mount(&upstream)
        .await;
    let gateway = start(config_with(&upstream, CAPABILITY_ROUTER, true)).await;

    let response = turn(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-routed-model"], "strong-alias");

    assert_eq!(spent(&gateway), [JUDGE.cost() + STRONG.cost(); 3]);
    upstream.verify().await;
}

/// The capture side of the same rule: a gated weak turn on a passthrough
/// tier, replayed, is not billed; the injecting judge that declined to
/// escalate it is.
#[tokio::test]
async fn a_gated_turn_captured_from_a_passthrough_tier_is_not_billed() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    WEAK.mock(WEAK.sse_reply("WEAK-ANSWER"), 1)
        .mount(&upstream)
        .await;
    JUDGE
        .mock(
            verdict(json!({"escalate": false, "reason": "progressing"})),
            1,
        )
        .mount(&upstream)
        .await;
    let gateway = start(config_with(&upstream, ESCALATION_ROUTER, true)).await;

    let response = turn(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(source(&response), "escalation_weak");
    response.bytes().await.unwrap();

    assert_eq!(spent(&gateway), [JUDGE.cost(); 3]);
    upstream.verify().await;
}

/// An Anthropic upstream that commits SSE headers, sends a prompt-usage frame
/// and 20 chars of text, then keeps the connection alive with pings forever:
/// the turn never reaches its final usage, so `gated_max_duration_ms` is what
/// ends it.
async fn endless_weak_stream() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 16384];
        let _ = socket.read(&mut buffer).await;
        let chunk = |payload: &str| format!("{:x}\r\n{payload}\r\n", payload.len());
        let start = json!({"type": "message_start", "message": {
            "id": "msg_weak", "type": "message", "role": "assistant",
            "model": WEAK.model, "content": [],
            "usage": {"input_tokens": WEAK.input, "output_tokens": 1}}});
        let delta = json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "0123456789abcdefghij"}});
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                    transfer-encoding: chunked\r\n\r\n";
        let mut out = head.to_string();
        out += &chunk(&format!("event: message_start\ndata: {start}\n\n"));
        out += &chunk(&format!("event: content_block_delta\ndata: {delta}\n\n"));
        if socket.write_all(out.as_bytes()).await.is_err() {
            return;
        }
        let ping = chunk("event: ping\ndata: {\"type\":\"ping\"}\n\n");
        while socket.write_all(ping.as_bytes()).await.is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    format!("http://{addr}")
}

/// A streamed weak capture that the outer `gated_max_duration_ms` cancels
/// mid-read is still billed, once, for what it received: the prompt usage and
/// the delivered-text floor (20 chars = 5 tokens).
#[tokio::test]
async fn a_streamed_capture_cut_by_the_duration_bound_is_billed_once() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    JUDGE
        .mock(
            verdict(json!({"escalate": false, "reason": "progressing"})),
            0,
        )
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.sse_reply("STRONG-ANSWER"), 1)
        .mount(&upstream)
        .await;
    let router = ESCALATION_ROUTER.replace(
        "[escalation]",
        "gated_idle_ms = 5000\ngated_max_duration_ms = 600\n\n[escalation]",
    );
    let mut config = config(&upstream, &router);
    let weak_url = endless_weak_stream().await;
    config
        .providers
        .get_mut(WEAK.upstream)
        .expect("the weak provider exists")
        .base_url = weak_url;
    let gateway = start(config).await;

    let response = turn(&gateway, true).await;
    let status = response.status();
    let heads = format!("{:?}", response.headers());
    let text = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{heads} {text}");
    assert!(text.contains("STRONG-ANSWER"), "{text}");

    let (input, output, cache_read, cache_write) = WEAK.rates;
    let weak_partial = Rates::from_usd_per_million(input, output, cache_read, cache_write)
        .cost_femto_usd(&Usage {
            input_tokens: WEAK.input,
            output_tokens: 5,
            ..Usage::default()
        });
    assert_eq!(spent(&gateway), [weak_partial + STRONG.cost(); 3]);
    upstream.verify().await;
}

/// The chunk that crosses `gated_max_bytes` was received and charged upstream,
/// so the capture it cuts is still billed for it, once. The cap is smaller than
/// the first frame, so `message_start` is all the capture saw.
#[tokio::test]
async fn a_streamed_capture_cut_by_the_byte_cap_is_billed_once() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let upstream = MockServer::start().await;
    JUDGE
        .mock(
            verdict(json!({"escalate": false, "reason": "progressing"})),
            0,
        )
        .mount(&upstream)
        .await;
    STRONG
        .mock(STRONG.sse_reply("STRONG-ANSWER"), 1)
        .mount(&upstream)
        .await;
    let router = ESCALATION_ROUTER.replace("[escalation]", "gated_max_bytes = 100\n\n[escalation]");
    let mut config = config(&upstream, &router);
    let weak_url = endless_weak_stream().await;
    config
        .providers
        .get_mut(WEAK.upstream)
        .expect("the weak provider exists")
        .base_url = weak_url;
    let gateway = start(config).await;

    let response = turn(&gateway, true).await;
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("STRONG-ANSWER"), "{text}");

    // `message_start` is the crossing chunk: input and output as it declared.
    let (input, output, cache_read, cache_write) = WEAK.rates;
    let weak_partial = Rates::from_usd_per_million(input, output, cache_read, cache_write)
        .cost_femto_usd(&Usage {
            input_tokens: WEAK.input,
            output_tokens: 1,
            ..Usage::default()
        });
    assert_eq!(spent(&gateway), [weak_partial + STRONG.cost(); 3]);
    upstream.verify().await;
}
