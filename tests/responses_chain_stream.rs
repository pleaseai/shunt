#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end coverage for the multi-upstream streaming chain: a streaming
//! turn with ordered upstreams runs the failover chain inside the committed
//! SSE response, deferring the synthetic `message_start` until an upstream
//! wins, so pre-header failures advance to the next upstream instead of
//! becoming a terminal error event.

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use shunt::{
    config::{
        AccountConfig, AccountSelection, AuthMap, Config, ModelConfig, ProviderKind, RetryConfig,
        UpstreamAuth, UpstreamConfig,
    },
    server,
};
use tokio::task::JoinHandle;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

mod common;

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

/// A chain config whose two upstreams map one model: `primary` first, then
/// `fallback`, in upstream order. Both kinds and base URLs are per-test.
fn chain_config(primary: (ProviderKind, String), fallback: (ProviderKind, String)) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![
        UpstreamConfig {
            name: "primary".to_string(),
            provider: None,
            kind: Some(primary.0),
            base_url: Some(primary.1),
            auth: None,
            effort: None,
            service_tier: None,
            count_tokens: Default::default(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            // A failing primary must fail once, not retry with backoff.
            retry: RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
            workspace_roots: Vec::new(),
            sandbox: true,
        },
        UpstreamConfig {
            name: "fallback".to_string(),
            provider: None,
            kind: Some(fallback.0),
            base_url: Some(fallback.1),
            auth: None,
            effort: None,
            service_tier: None,
            count_tokens: Default::default(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
            workspace_roots: Vec::new(),
            sandbox: true,
        },
    ];
    config.server.default_provider = "primary".to_string();
    config.models = vec![ModelConfig {
        id: "chain-test-model".to_string(),
        display_name: None,
        upstream_model: Some(
            [
                ("primary".to_string(), "gpt-5.6-sol".to_string()),
                ("fallback".to_string(), "claude-fable-5-1".to_string()),
            ]
            .into_iter()
            .collect(),
        ),
        stage_router: None,
    }];
    config
}

async fn start_gateway(config: Config) -> TestGateway {
    let mut config = config;
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// A base URL whose connections are deterministically refused: a socket
/// bound but never listening, held open for the test's lifetime. A dropped
/// listener frees the port, and the OS can hand it to another ephemeral
/// socket before the gateway connects — the race that made the old
/// bind-then-drop helper flake.
struct RefusedPort {
    url: String,
    _socket: socket2::Socket,
}

fn refused_base_url() -> RefusedPort {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    socket
        .bind(
            &"127.0.0.1:0"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        )
        .unwrap();
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    RefusedPort {
        url: format!("http://127.0.0.1:{port}"),
        _socket: socket,
    }
}

#[test]
fn refused_port_is_deterministically_refused() {
    if !can_bind_loopback() {
        return;
    }
    let refused = refused_base_url();
    let port = refused.url.rsplit(':').next().unwrap();
    let error =
        std::net::TcpStream::connect(format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap())
            .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::ConnectionRefused,
        "a bound, non-listening socket must refuse deterministically"
    );
}

async fn stream_request(gateway: &TestGateway) -> reqwest::Response {
    // `no_proxy`: the refused-loopback assertions must fail at the transport
    // layer, never route through a system HTTP proxy that would turn the
    // refusal into a proxy response.
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "chain-test-model",
                "max_tokens": 16,
                "stream": true,
                "messages": [{ "role": "user", "content": "Reply with OK." }],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

fn count_event(body: &str, event: &str) -> usize {
    body.lines()
        .filter(|line| *line == format!("event: {event}"))
        .count()
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_anthropic\",\"model\":\"claude-fable-5-1\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"from anthropic\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"response\":{\"id\":\"resp_chain\",\"usage\":{\"output_tokens\":0}}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"delta\":\"from responses\"}\n\n",
    "event: response.output_text.done\n",
    "data: {}\n\n",
    "event: response.completed\n",
    "data: {\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":4}}}\n\n",
    "data: [DONE]\n\n"
);

#[tokio::test]
async fn transport_failure_on_the_primary_advances_to_the_anthropic_fallback() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            // set_body_raw so the mock actually returns text/event-stream;
            // insert_header would leave wiremock's text/plain default in place.
            ResponseTemplate::new(200).set_body_raw(ANTHROPIC_SSE, "text/event-stream"),
        )
        .expect(1)
        .mount(&fallback)
        .await;

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Responses, refused.url.clone()),
        (ProviderKind::Anthropic, fallback.uri()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    // Exactly one message_start — the fallback's own relayed start, never a
    // synthetic one from the failed primary.
    assert_eq!(count_event(&body, "message_start"), 1, "got:\n{body}");
    assert!(body.contains("from anthropic"), "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 0, "got:\n{body}");
    drop(gateway);
    fallback.verify().await;
}

#[tokio::test]
async fn a_broken_chain_ends_in_one_terminal_error_event() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Responses, refused.url.clone()),
        (ProviderKind::Anthropic, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_start"), 0, "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 1, "got:\n{body}");
}

#[tokio::test]
async fn advance_status_on_the_primary_defers_the_synthetic_start_to_the_winner() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&primary)
        .await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSES_SSE, "text/event-stream"))
        .expect(1)
        .mount(&fallback)
        .await;

    let config = chain_config(
        (ProviderKind::Responses, primary.uri()),
        (ProviderKind::Responses, fallback.uri()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    // The single synthetic start is deferred until the fallback wins; the
    // primary's model string never reaches the client because it never relayed.
    assert_eq!(count_event(&body, "message_start"), 1, "got:\n{body}");
    assert!(!body.contains("gpt-5.6-sol"), "got:\n{body}");
    assert!(body.contains("from responses"), "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 0, "got:\n{body}");
    drop(gateway);
    primary.verify().await;
    fallback.verify().await;
}

#[tokio::test]
async fn chain_exhaustion_relays_the_best_remembered_failure() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&primary)
        .await;

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Responses, primary.uri()),
        (ProviderKind::Anthropic, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_start"), 0, "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 1, "got:\n{body}");
    // The remembered 429 is the best failure by priority and must be the
    // relayed envelope, not the transport error of the last attempt.
    assert!(body.contains("rate_limit_error"), "got:\n{body}");
    assert!(!body.contains("error sending request"), "got:\n{body}");
    drop(gateway);
    primary.verify().await;
}

#[tokio::test]
async fn a_non_sse_anthropic_winner_becomes_one_terminal_error_event() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    // A successful Anthropic-compatible upstream that answers the streaming
    // request with JSON: the committed stream is already `text/event-stream`
    // and cannot relay the body inside it, so the chain emits one terminal
    // error event instead of mislabeled bytes.
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"message","content":"not sse"}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&fallback)
        .await;

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Responses, refused.url.clone()),
        (ProviderKind::Anthropic, fallback.uri()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_start"), 0, "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 1, "got:\n{body}");
    assert!(!body.contains("not sse"), "got:\n{body}");
    drop(gateway);
    fallback.verify().await;
}

#[tokio::test]
async fn an_exhausted_pool_primary_advances_to_the_anthropic_fallback() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    // The pool's only account resolves its credential from this env; unset
    // is the test's precondition (resolution fails, the pool exhausts before
    // any account frame).
    vars.unset("SHUNT_POOL_UNSET_TOKEN");
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ANTHROPIC_SSE, "text/event-stream"))
        .expect(1)
        .mount(&fallback)
        .await;

    let refused = refused_base_url();
    let mut config = chain_config(
        (ProviderKind::Responses, refused.url.clone()),
        (ProviderKind::Anthropic, fallback.uri()),
    );
    config.upstreams[0].auth = Some(UpstreamAuth::Map(AuthMap::ChatgptOauth {
        account: None,
        accounts: Some(vec![AccountSelection::Inline(AccountConfig {
            name: "pool-a".to_string(),
            token_env: Some("SHUNT_POOL_UNSET_TOKEN".to_string()),
            ..Default::default()
        })]),
    }));
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    // The exhausted pool is a pre-frame failure the chain advances on (§4),
    // never a terminal mid-relay error: the fallback's own start and output
    // reach the client.
    assert_eq!(count_event(&body, "message_start"), 1, "got:\n{body}");
    assert!(body.contains("from anthropic"), "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 0, "got:\n{body}");
    drop(gateway);
    fallback.verify().await;
}

#[tokio::test]
async fn a_healthy_primary_still_commits_exactly_one_message_start() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSES_SSE, "text/event-stream"))
        .expect(1)
        .mount(&primary)
        .await;

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Responses, primary.uri()),
        (ProviderKind::Anthropic, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_start"), 1, "got:\n{body}");
    assert!(body.contains("from responses"), "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 0, "got:\n{body}");
    drop(gateway);
    primary.verify().await;
}
