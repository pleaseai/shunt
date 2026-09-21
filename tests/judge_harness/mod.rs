//! Harness for `tests/router_judge.rs`: the driven-lane deployment every test
//! in that binary runs against, plus the two mocks it needs — wiremock for a
//! well-behaved judge and a raw socket for one that stalls.

#![allow(dead_code)]

use std::{collections::BTreeMap, io::ErrorKind, net::SocketAddr, time::Duration};

use serde_json::{json, Value};
use shunt::{
    config::{
        ApiKeyHeader, AuthMap, AuthMode, Config, CountTokens, InboundAuthConfig, ModelConfig,
        ProviderKind, RetryConfig, RouterConfig, StageClassifierConfig, StageRouterConfig,
        StageRouterPicker, UpstreamAuth, UpstreamConfig,
    },
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use wiremock::{
    matchers::{method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

pub(crate) const ROUTER_ID: &str = "claude-auto";
pub(crate) const CAPABLE_UPSTREAM_MODEL: &str = "upstream-capable";
pub(crate) const EFFICIENT_UPSTREAM_MODEL: &str = "upstream-efficient";
pub(crate) const JUDGE_UPSTREAM_MODEL: &str = "upstream-judge";
pub(crate) const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

pub(crate) const CLIENT_TOKEN: &str = "client-token";
pub(crate) const JUDGE_KEY: &str = "judge-key";
pub(crate) const CAPABLE_KEY: &str = "capable-key";

pub(crate) const TOKENS_ENV: &str = "SHUNT_TEST_JUDGE_TOKENS";
pub(crate) const JUDGE_KEY_ENV: &str = "SHUNT_TEST_JUDGE_KEY";
pub(crate) const CAPABLE_KEY_ENV: &str = "SHUNT_TEST_JUDGE_CAPABLE_KEY";

pub(crate) struct TestGateway {
    pub(crate) base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Matches the outbound body's `model`: how the tier the router picked, and the
/// judge the driver called, become observable at the upstream.
pub(crate) struct ModelIs(pub(crate) &'static str);

impl Match for ModelIs {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body.get("model")?.as_str().map(ToOwned::to_owned))
            .is_some_and(|model| model == self.0)
    }
}

pub(crate) fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

pub(crate) fn upstream_with(name: &str, base_url: String, auth: UpstreamAuth) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_string(),
        provider: None,
        kind: Some(ProviderKind::Anthropic),
        base_url: Some(base_url),
        auth: Some(auth),
        effort: None,
        service_tier: None,
        classifier_model: None,
        // Tiktoken, so a `count_tokens` probe is answered locally and makes no
        // upstream call of its own.
        count_tokens: CountTokens::Tiktoken,
        websocket: false,
        tool_search: None,
        request_compression: true,
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        workspace_roots: Vec::new(),
        profile_dir: None,
        sandbox: true,
    }
}

pub(crate) fn api_key(name: &str, base_url: String, env: &str) -> UpstreamConfig {
    upstream_with(
        name,
        base_url,
        UpstreamAuth::Map(AuthMap::ApiKey {
            env: Some(env.to_string()),
            header: Some(ApiKeyHeader::XApiKey),
        }),
    )
}

pub(crate) fn alias(id: &str, upstream: &str, upstream_model: &str) -> ModelConfig {
    ModelConfig {
        subagents: None,
        id: id.to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            upstream.to_string(),
            upstream_model.to_string(),
        )])),
        router: None,
        stage_router: None,
    }
}

/// The driven entry: a passthrough efficient tier, an injecting capable tier,
/// and an injecting judge. The mix is deliberate — it is what makes the
/// envelope's admission rule observable.
pub(crate) fn driven_config(
    capable: &MockServer,
    efficient: &MockServer,
    judge_url: String,
) -> Config {
    driven_config_with(capable, efficient, judge_url, |_| {})
}

/// [`driven_config`] with one edit applied before validation, for the tests
/// that need a different shape of the same deployment.
pub(crate) fn driven_config_with(
    capable: &MockServer,
    efficient: &MockServer,
    judge_url: String,
    tweak: impl FnOnce(&mut Config),
) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![
        api_key("capable", capable.uri(), CAPABLE_KEY_ENV),
        upstream_with(
            "efficient",
            efficient.uri(),
            UpstreamAuth::Shorthand(AuthMode::Passthrough),
        ),
        api_key("judge", judge_url, JUDGE_KEY_ENV),
    ];
    config.server.default_provider = "efficient".to_string();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
    });
    config.models = vec![
        ModelConfig {
            subagents: None,
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(RouterConfig::StageRouter(StageRouterConfig {
                capable_target: "capable-alias".to_string(),
                efficient_target: "efficient-alias".to_string(),
                picker: StageRouterPicker::EfficientFirst,
                confidence_threshold: 0.5,
                recent_turn_window: 3,
                min_dwell_turns: 2,
                deescalate_threshold: None,
                session_ttl_seconds: 3600,
                capable_hold_turns: 0,
                tool_semantics: Default::default(),
                handoff_notes: None,
                classifier: Some(StageClassifierConfig {
                    target: "judge-alias".to_string(),
                    base_threshold: 0.5,
                }),
                // Short enough that the stall tests finish inside their own
                // assertion window.
                judge_timeout_ms: 500,
                judge_max_response_bytes: shunt::config::DEFAULT_JUDGE_MAX_RESPONSE_BYTES,
                gated_max_bytes: shunt::config::DEFAULT_GATED_MAX_BYTES,
                gated_idle_ms: shunt::config::DEFAULT_GATED_IDLE_MS,
                gated_max_duration_ms: shunt::config::DEFAULT_GATED_MAX_DURATION_MS,
                max_judge_calls: 1,
            })),
            stage_router: None,
        },
        alias("capable-alias", "capable", CAPABLE_UPSTREAM_MODEL),
        alias("efficient-alias", "efficient", EFFICIENT_UPSTREAM_MODEL),
        alias("judge-alias", "judge", JUDGE_UPSTREAM_MODEL),
    ];
    tweak(&mut config);
    config.validate().expect("the driven config is well formed")
}

pub(crate) async fn start_gateway(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _, _) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// One failed investigative turn: a single saturated signal scores
/// `tanh(0.5) ≈ 0.46`, under the 0.5 gate, so the scorer falls open — the one
/// turn a configured judge is consulted on.
pub(crate) fn undecided_messages() -> Value {
    json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
    ])
}

/// Two of them: corroborating signals reach `tanh(1.0) ≈ 0.76` and the scorer
/// decides on its own.
pub(crate) fn decisive_messages() -> Value {
    json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "Grep"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "is_error": true}]},
    ])
}

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::new()
}

pub(crate) async fn post(gateway: &TestGateway, messages: Value) -> reqwest::Response {
    post_to(gateway, "/v1/messages", messages, Some(CLIENT_TOKEN)).await
}

pub(crate) async fn post_to(
    gateway: &TestGateway,
    path: &str,
    messages: Value,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = client()
        .post(format!("{}{path}", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION);
    if let Some(token) = token {
        request = request.header("x-shunt-token", token);
    }
    request
        .body(json!({"model": ROUTER_ID, "max_tokens": 16, "messages": messages}).to_string())
        .send()
        .await
        .unwrap()
}

pub(crate) fn tier_reply(upstream_model: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(
        json!({"id": "msg_1", "type": "message", "model": upstream_model}).to_string(),
    )
}

pub(crate) fn tier_mock(upstream_model: &'static str, expect: u64) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(ModelIs(upstream_model))
        .respond_with(tier_reply(upstream_model))
        .expect(expect)
}

/// The judge's verdict, in the shape libsy's capability classifier parses: a
/// `p_solve` under `base_threshold` means the efficient tier is not trusted
/// with the task, which is the capable tier.
pub(crate) fn verdict(p_solve: f64) -> ResponseTemplate {
    let text = json!({
        "crux": "bounded task",
        "primary_rule": "SUP-1",
        "capability_boundary": "supported",
        "p_solve": p_solve,
    })
    .to_string();
    ResponseTemplate::new(200).set_body_string(
        json!({
            "id": "msg_judge",
            "type": "message",
            "role": "assistant",
            "model": JUDGE_UPSTREAM_MODEL,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })
        .to_string(),
    )
}

pub(crate) fn judge_mock(p_solve: f64, expect: u64) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(verdict(p_solve))
        .expect(expect)
}

/// Sets the three variables every test in this binary needs, under the shared
/// env lock.
pub(crate) async fn env() -> crate::common::EnvVars {
    crate::common::set_env(&[
        (TOKENS_ENV, &format!("client:{CLIENT_TOKEN}")),
        (JUDGE_KEY_ENV, JUDGE_KEY),
        (CAPABLE_KEY_ENV, CAPABLE_KEY),
    ])
    .await
}
/// A conversation with no tool activity at all: nothing for the scorer to read,
/// so not even a fall-open.
pub(crate) fn quiet_messages() -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
}

/// The two stalls `judge_timeout_ms` exists for, and the reason the deadline
/// cannot be the transport's. Both commit headers first, so every timeout that
/// stops at `.send()` has already been satisfied when the hang begins.
#[derive(Clone, Copy)]
pub(crate) enum Stall {
    /// `200`, then a partial body and silence.
    AfterHeaders,
    /// `200`, then SSE keep-alive frames forever: the socket is alive and
    /// chunks keep arriving, but the turn never progresses.
    EndlessPing,
}

/// A judge upstream that answers and then hangs, on a raw socket because no
/// mock server can express "committed headers, then nothing".
///
/// The receiver fires when the accepted connection is observed closed, which is
/// what proves the elapsed deadline actually cancelled the upstream request
/// rather than merely stopping waiting for it.
pub(crate) async fn stall_mock(stall: Stall) -> (String, tokio::sync::oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed, closed_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("the judge call connects");
        let mut buffer = [0u8; 8192];
        // One read is enough to get past the request head; the body is small
        // and this mock answers the same way regardless of it.
        let _ = socket.read(&mut buffer).await;
        let head: &[u8] = match stall {
            Stall::AfterHeaders => {
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n"
            }
            Stall::EndlessPing => {
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            }
        };
        if socket.write_all(head).await.is_err() {
            let _ = closed.send(());
            return;
        }
        match stall {
            Stall::AfterHeaders => {
                // A partial chunk, never terminated.
                let _ = socket.write_all(b"12\r\n{\"id\":\"msg_judge\",\r\n").await;
                // Then wait for the peer to go away, which is the assertion.
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
            Stall::EndlessPing => {
                let frame = b"22\r\nevent: ping\ndata: {\"t\":\"ping\"}\n\n\r\n";
                // A write to a closed socket is how this arm notices; the
                // cadence is fast enough that it notices promptly.
                while socket.write_all(frame).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        let _ = closed.send(());
    });
    (format!("http://{addr}"), closed_rx)
}
