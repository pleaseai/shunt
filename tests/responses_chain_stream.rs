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
        AccountConfig, AccountSelection, AuthMap, Config, CountTokens, ModelConfig, ProviderKind,
        RandomAffinity, RandomRouterConfig, RetryConfig, RouterConfig, UpstreamAuth,
        UpstreamConfig,
    },
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string_contains, method},
    Mock, MockServer, ResponseTemplate,
};

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
            classifier_model: None,
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
            profile_dir: None,
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
            classifier_model: None,
            count_tokens: Default::default(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
            workspace_roots: Vec::new(),
            profile_dir: None,
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
        router: None,
        stage_router: None,
        subagents: None,
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
    stream_request_for(gateway, "chain-test-model").await
}

/// The same streaming request against an arbitrary public model id, so a test
/// can enter the chain through a `[models.router]` entry that resolves to it.
async fn stream_request_for(gateway: &TestGateway, model: &str) -> reqwest::Response {
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
                "model": model,
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

/// An opted-out route never starts the chain's shared estimate: a chain whose
/// primary opted out of local counting and whose fallback opted in must hand
/// the winner the fallback's real count, never the primary's zero. A cache
/// initialized unconditionally captures the opted-out primary's route, its
/// zero poisons the cell, and the winner's synthetic start carries 0, failing
/// the assertion.
#[tokio::test]
async fn an_opted_out_primary_never_starts_the_chains_shared_estimate() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSES_SSE, "text/event-stream"))
        .expect(1)
        .mount(&fallback)
        .await;

    let refused = refused_base_url();
    let mut config = chain_config(
        (ProviderKind::Responses, refused.url.clone()),
        (ProviderKind::Responses, fallback.uri()),
    );
    // The primary opts out; the fallback keeps the Tiktoken default and opts
    // in. The gate must keep the opted-out primary from initializing the
    // chain's shared estimate with its own zero.
    config.upstreams[0].count_tokens = CountTokens::Estimate;
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_start"), 1, "got:\n{body}");
    assert!(body.contains("from responses"), "got:\n{body}");
    let start_data = body
        .split_once("event: message_start\ndata: ")
        .expect("carries a synthetic start")
        .1
        .split("\n\n")
        .next()
        .expect("event frame is terminated");
    let start: serde_json::Value = serde_json::from_str(start_data).expect("start data is json");
    assert!(
        start["message"]["usage"]["input_tokens"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "the winner's synthetic start carries the fallback's real count, got: {start_data}"
    );
    drop(gateway);
    fallback.verify().await;
}

/// The final chain attempt still receives the request body: the last route
/// moves the buffered body (no later attempt needs it) and must forward the
/// request bytes faithfully. A take applied to a non-final attempt would
/// empty the body for the next route and abort the chain before any request
/// reached the fallback.
#[tokio::test]
async fn the_final_attempt_receives_the_request_body() {
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
        .and(body_string_contains("Reply with OK."))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ANTHROPIC_SSE, "text/event-stream"))
        .expect(1)
        .mount(&fallback)
        .await;

    let config = chain_config(
        (ProviderKind::Responses, primary.uri()),
        (ProviderKind::Anthropic, fallback.uri()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("from anthropic"), "got:\n{body}");
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

/// A router-routed streaming turn carries the two router headers.
///
/// This path commits the SSE response before any upstream has won, so the
/// upstream-naming headers are deliberately omitted — but the router pair is
/// not winner-dependent: it reports what the `[models.router]` entry chose
/// before the first attempt was made, and the reference documents it for every
/// router type. Omitting it here would have made that claim false for exactly
/// the streaming failover chains.
#[tokio::test]
async fn a_routed_streaming_chain_stamps_the_router_headers() {
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
    let mut config = chain_config(
        (ProviderKind::Responses, primary.uri()),
        (ProviderKind::Anthropic, refused.url.clone()),
    );
    // One target and one arm, so the assertion is about the stamp rather than
    // about which way a weighted draw fell.
    config.models.push(ModelConfig {
        id: "chain-router".to_string(),
        display_name: None,
        upstream_model: None,
        router: Some(RouterConfig::Random(RandomRouterConfig {
            targets: vec!["chain-test-model".to_string()],
            weights: None,
            seed: None,
            affinity: RandomAffinity::Request,
        })),
        stage_router: None,
        subagents: None,
    });
    let gateway = start_gateway(config).await;
    let response = stream_request_for(&gateway, "chain-router").await;

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers["x-gateway-model"], "chain-router",
        "the client-requested id is the router entry, whatever it resolved to"
    );
    assert_eq!(
        headers["x-gateway-routed-model"], "chain-test-model",
        "the committed streaming chain must report the target the router chose"
    );
    assert_eq!(
        headers["x-gateway-route-source"], "random",
        "and why it chose it"
    );
    // The turn itself still relays normally: the stamp is additive.
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_start"), 1, "got:\n{body}");
    assert!(body.contains("from responses"), "got:\n{body}");
    drop(gateway);
    primary.verify().await;
}

/// A winner whose body errors after the complete turn already relayed —
/// `message_stop` included, then the connection closing short of the declared
/// content-length — must end the relay silently. Appending an `error` event
/// to a completed response corrupts the client's finished turn, and the
/// stream observer (which gives error events precedence over terminal events)
/// would record the request as failed.
#[tokio::test]
async fn a_body_error_after_message_stop_ends_the_relay_silently() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    // wiremock cannot express this fault: its hyper server refuses a
    // content-length header that disagrees with the body it sends. A raw
    // one-shot responder writes the full turn, then closes with bytes still
    // unread against the declared length — the reader's body errors on the
    // final chunk, after every frame has relayed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let responder = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(ANTHROPIC_SSE.as_bytes()).await.unwrap();
    });

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Anthropic, format!("http://{addr}")),
        (ProviderKind::Responses, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(count_event(&body, "message_stop"), 1, "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 0, "got:\n{body}");
    drop(gateway);
    responder.await.unwrap();
}

/// The silent end is terminal-frame-gated: a body error BEFORE the turn's
/// `message_stop` still becomes one terminal `error` event with the failure
/// recorded — only a completed turn may end silently.
#[tokio::test]
async fn a_body_error_before_message_stop_still_emits_the_terminal_error_event() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    // The same raw one-shot responder, this time cutting the turn short:
    // the start and one delta relay, then the connection closes with bytes
    // still unread against the declared content-length.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let responder = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\n\r\n",
            )
            .await
            .unwrap();
        socket
            .write_all(
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_anthropic\",\"model\":\"claude-fable-5-1\"}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Anthropic, format!("http://{addr}")),
        (ProviderKind::Responses, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("partial"), "got:\n{body}");
    assert_eq!(count_event(&body, "message_stop"), 0, "got:\n{body}");
    assert_eq!(count_event(&body, "error"), 1, "got:\n{body}");
    drop(gateway);
    responder.await.unwrap();
}

/// One raw chunked SSE responder: serves the whole `turn` as a single chunk
/// — a trailing frame then shares the terminal frame's chunk — and parks the
/// connection afterwards, no terminating chunk, no EOF, until the caller
/// releases the park. wiremock cannot express the park: its server closes or
/// errors the body.
async fn parked_responder(
    turn: String,
) -> (
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_park, parked) = tokio::sync::oneshot::channel::<()>();
    let responder = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        socket
            .write_all(format!("{:x}\r\n", turn.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(turn.as_bytes()).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        let _ = parked.await;
    });
    (addr, release_park, responder)
}

/// An Anthropic-kind winner whose upstream parks the connection after the
/// full turn — no EOF, no error — must not strand the client: the relay ends
/// at the terminal frame and the post-terminal drain reads the still-open
/// upstream detached, so no keepalive ping can ever follow `message_stop` and
/// the client's stream completes instead of waiting on the upstream to close.
#[tokio::test]
async fn a_parked_upstream_after_message_stop_ends_the_relay_at_the_terminal_frame() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let turn = format!("{ANTHROPIC_SSE}event: ping\ndata: {{}}\n\n");
    let (addr, release_park, responder) = parked_responder(turn).await;

    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Anthropic, format!("http://{addr}")),
        (ProviderKind::Responses, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(std::time::Duration::from_secs(3), response.text())
        .await
        .expect("the relay ends at the terminal frame instead of waiting on the parked upstream")
        .unwrap();
    let frames: Vec<&str> = body
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .collect();
    assert_eq!(
        frames.last().unwrap().lines().next().unwrap(),
        "event: message_stop",
        "no frame may follow the terminal frame, got:\n{body}"
    );
    assert_eq!(
        count_event(&body, "ping"),
        0,
        "no frame may follow the terminal frame — neither an upstream frame nor a keepalive ping, got:\n{body}"
    );
    drop(gateway);
    drop(release_park);
    responder.await.unwrap();
}

/// An Anthropic-kind winner whose upstream relays a terminal `error` frame
/// and then parks the connection must not strand the client: the error frame
/// is terminal exactly like `message_stop`, so the relay ends at that frame
/// and the post-terminal drain reads the still-open upstream detached — no
/// keepalive ping can ever follow the terminal error, and the client's
/// stream completes instead of waiting on the upstream to close.
#[tokio::test]
async fn a_parked_upstream_after_an_error_frame_ends_the_relay_at_it() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let turn = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_anthropic\",\"model\":\"claude-fable-5-1\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n",
        "event: ping\n",
        "data: {}\n\n",
    );
    let (addr, release_park, responder) = parked_responder(turn.to_string()).await;
    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Anthropic, format!("http://{addr}")),
        (ProviderKind::Responses, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(std::time::Duration::from_secs(3), response.text())
        .await
        .expect(
            "the relay ends at the terminal error frame instead of waiting on the parked upstream",
        )
        .unwrap();
    let frames: Vec<&str> = body
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .collect();
    assert_eq!(
        frames.last().unwrap().lines().next().unwrap(),
        "event: error",
        "no frame may follow the terminal error frame, got:\n{body}"
    );
    assert_eq!(
        count_event(&body, "ping"),
        0,
        "no frame may follow the terminal error frame — neither an upstream frame nor a keepalive ping, got:\n{body}"
    );
    assert_eq!(count_event(&body, "error"), 1, "got:\n{body}");
    drop(gateway);
    drop(release_park);
    responder.await.unwrap();
}

/// The terminal event's field may omit the post-colon space — the spelling
/// the metrics parser accepts — and the relay still ends at that frame: the
/// scan shares the observer's field parser, so a no-space `message_stop`
/// cannot strand the client on a parked upstream.
#[tokio::test]
async fn a_parked_upstream_with_a_no_space_terminal_field_ends_the_relay_at_it() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let no_space = ANTHROPIC_SSE.replace("event: message_stop\n", "event:message_stop\n");
    let turn = format!("{no_space}event: ping\ndata: {{}}\n\n");
    let (addr, release_park, responder) = parked_responder(turn).await;
    let refused = refused_base_url();
    let config = chain_config(
        (ProviderKind::Anthropic, format!("http://{addr}")),
        (ProviderKind::Responses, refused.url.clone()),
    );
    let gateway = start_gateway(config).await;
    let response = stream_request(&gateway).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(std::time::Duration::from_secs(3), response.text())
        .await
        .expect("the relay ends at the terminal frame instead of waiting on the parked upstream")
        .unwrap();
    let frames: Vec<&str> = body
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .collect();
    assert_eq!(
        frames.last().unwrap().lines().next().unwrap(),
        "event:message_stop",
        "no frame may follow the terminal frame, got:\n{body}"
    );
    assert_eq!(
        count_event(&body, "ping"),
        0,
        "no frame may follow the terminal frame — neither an upstream frame nor a keepalive ping, got:\n{body}"
    );
    drop(gateway);
    drop(release_park);
    responder.await.unwrap();
}
