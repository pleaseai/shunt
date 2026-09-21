//! End-to-end coverage for `[models.router] type = "noop"`.
//!
//! The unit tests in `src/adapters/noop.rs` pin the two response shapes. These
//! pin that a real request reaches them at all — through the axum router, the
//! inbound-auth gate, and the adapter dispatch — and that nothing upstream is
//! contacted on the way: the config below declares **no upstream whose base URL
//! resolves**, so any dispatch off the noop route would fail the request rather
//! than quietly succeed.
//!
//! Non-vacuity: give `AdapterKind::Noop` no dispatch arm and every test here
//! fails to compile; make `is_passthrough_route` answer `true` for it and
//! `a_noop_route_still_requires_the_inbound_credential` goes red.

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{Config, InboundAuthConfig, ModelConfig, RouterConfig},
    server,
};
use tokio::task::JoinHandle;

mod common;

const NOOP_ID: &str = "claude-quiet";

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn can_bind_loopback() -> bool {
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

fn noop_config() -> Config {
    let config = Config {
        models: vec![ModelConfig {
            id: NOOP_ID.to_string(),
            display_name: Some("Quiet".to_string()),
            upstream_model: None,
            router: Some(RouterConfig::Noop {}),
            stage_router: None,
            subagents: None,
        }],
        ..Config::default()
    };
    config.validate().expect("a noop entry is well formed")
}

async fn start_gateway(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
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

async fn post(
    gateway: &TestGateway,
    path: &str,
    body: Value,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{path}", gateway.base_url))
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("x-shunt-token", token);
    }
    request.body(body.to_string()).send().await.unwrap()
}

#[tokio::test]
async fn a_streaming_caller_gets_a_synthesized_sse_turn() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway(noop_config()).await;

    let response = post(
        &gateway,
        "/v1/messages",
        json!({"model": NOOP_ID, "max_tokens": 16, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    // The route is stamped like any other, so a client can see which entry
    // answered even though no upstream was involved.
    assert_eq!(
        response.headers().get("x-gateway-upstream").unwrap(),
        "noop"
    );
    assert_eq!(
        response.headers().get("x-gateway-route-source").unwrap(),
        "noop"
    );
    assert_eq!(
        response.headers().get("x-gateway-routed-model").unwrap(),
        NOOP_ID
    );

    let body = response.text().await.unwrap();
    let events: Vec<&str> = body
        .lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .collect();
    assert_eq!(events, ["message_start", "message_delta", "message_stop"]);
    assert!(body.contains(&format!(r#""model":"{NOOP_ID}""#)));
}

#[tokio::test]
async fn a_non_streaming_caller_gets_one_terminal_message() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway(noop_config()).await;

    let response = post(
        &gateway,
        "/v1/messages",
        json!({"model": NOOP_ID, "max_tokens": 16,
               "messages": [{"role": "user", "content": "hi"}]}),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let message: Value = response.json().await.unwrap();
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["model"], NOOP_ID);
    assert_eq!(message["content"], json!([]));
    assert_eq!(message["stop_reason"], "end_turn");
}

#[tokio::test]
async fn count_tokens_on_a_noop_route_answers_zero() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway(noop_config()).await;

    let response = post(
        &gateway,
        "/v1/messages/count_tokens",
        json!({"model": NOOP_ID, "messages": [{"role": "user", "content": "a longer prompt"}]}),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"input_tokens": 0}),
        "a noop turn carries no content, and no provider's count_tokens mode applies"
    );
}

/// A noop route injects nothing, but it is not passthrough either: the caller
/// presents no upstream credential of their own, so `[server.auth]` must still
/// gate it. `is_passthrough_route` decides this on the adapter rather than the
/// provider name, so a provider an operator happened to call `noop` cannot open
/// the route up.
#[tokio::test]
async fn a_noop_route_still_requires_the_inbound_credential() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_NOOP_TOKENS", "alice:secret-token");

    let mut config = noop_config();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_NOOP_TOKENS".to_string(),
    });
    let config = config.validate().expect("inbound auth is well formed");
    let gateway = start_gateway(config).await;

    let body = json!({"model": NOOP_ID, "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}]});

    let refused = post(&gateway, "/v1/messages", body.clone(), None).await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    let allowed = post(&gateway, "/v1/messages", body, Some("secret-token")).await;
    assert_eq!(allowed.status(), StatusCode::OK);
}
