//! Integration coverage for bounded upstream retry/backoff (issue #48).
//!
//! Drives the real proxy → Anthropic passthrough path against a mock upstream,
//! asserting the observable retry contract: a transient status retries and can
//! succeed, a non-transient status surfaces immediately, exhausted retries
//! surface the last response, `count_tokens` never retries, and a success is
//! never re-issued (so a retry can never happen mid-stream).

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use shunt::{config::Config, server};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Default providers with the Anthropic passthrough pointed at the mock
/// upstream, and its retry backoff shrunk to near-zero so tests never sleep for
/// real. Retry attempt counts and status behavior are unchanged by the shrink.
fn retry_test_config(upstream_base_url: String) -> Config {
    let mut config = Config::default();
    let anthropic = config.providers.get_mut("anthropic").unwrap();
    anthropic.base_url = upstream_base_url;
    anthropic.retry.initial_backoff_ms = 1;
    anthropic.retry.max_backoff_ms = 2;
    config
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
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

async fn post_messages(gateway: &TestGateway, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", gateway.base_url))
        .body(r#"{"model":"claude-opus-4-1","max_tokens":1}"#)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn transient_503_then_success_is_retried() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // First attempt: a transient 503 (retry-after 0 keeps the backoff instant),
    // capped to one response so the retry falls through to the 200 below.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"transient"}"#),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&upstream)
        .await;
    // The retry succeeds.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .with_priority(2)
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(retry_test_config(upstream.uri())).await;

    let response = post_messages(&gateway, "/v1/messages").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);
    // The mock expectations (503 once, 200 once = 2 upstream hits) are verified
    // when `upstream` drops: proof the retry actually fired.
}

#[tokio::test]
async fn non_transient_400_surfaces_without_retry() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // A 400 is a request error an identical retry cannot fix; expect exactly one
    // upstream hit even though the default policy allows retries.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"bad request"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(retry_test_config(upstream.uri())).await;

    let response = post_messages(&gateway, "/v1/messages").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn exhausted_retries_surface_last_transient_response() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // Always 503: the default policy is 1 initial try + 2 retries = 3 hits, then
    // the last 503 is surfaced to the client.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"still down"}"#),
        )
        .expect(3)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(retry_test_config(upstream.uri())).await;

    let response = post_messages(&gateway, "/v1/messages").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn disabled_policy_surfaces_transient_immediately() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"down"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut config = retry_test_config(upstream.uri());
    config
        .providers
        .get_mut("anthropic")
        .unwrap()
        .retry
        .max_retries = 0;
    let gateway = start_gateway_with(config).await;

    let response = post_messages(&gateway, "/v1/messages").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn count_tokens_is_not_retried() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // count_tokens passes through for an Anthropic-kind provider, but retry is
    // held off it — a transient 503 must surface after a single upstream hit.
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"down"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(retry_test_config(upstream.uri())).await;

    let response = post_messages(&gateway, "/v1/messages/count_tokens").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn success_is_never_retried() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // A 2xx is committed the moment its body starts relaying, so it must be hit
    // exactly once — the structural guarantee that a retry never happens
    // mid-stream.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(retry_test_config(upstream.uri())).await;

    let response = post_messages(&gateway, "/v1/messages").await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn responses_path_retries_transient_upstream() {
    if !can_bind_loopback() {
        return;
    }
    // A unique api-key env var (avoids colliding with a real OPENAI_API_KEY or a
    // parallel test) so the single-credential Responses path resolves its
    // credential and reaches forward_http, where the shared retry driver runs.
    std::env::set_var("SHUNT_TEST_OPENAI_RETRY_KEY", "sk-test-retry");

    let upstream = MockServer::start().await;
    // Always 503: the default policy is 1 initial try + 2 retries = 3 hits on the
    // Responses `/responses` endpoint — proof the responses adapter retries
    // transient upstream failures through the same driver as the Anthropic path.
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"still down"}"#),
        )
        .expect(3)
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = upstream.uri();
    openai.api_key_env = Some("SHUNT_TEST_OPENAI_RETRY_KEY".to_string());
    openai.retry.initial_backoff_ms = 1;
    openai.retry.max_backoff_ms = 2;
    // Route a probe model at the single-credential Responses provider.
    config.routes.push(shunt::config::RouteConfig {
        model: "responses-retry-probe".to_string(),
        provider: "openai".to_string(),
        upstream_model: None,
        effort: None,
    });
    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(
            r#"{"model":"responses-retry-probe","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();
    // Exhausted retries surface a non-success status; the mock's `.expect(3)`
    // (verified on drop) is the proof the retry actually fired on this path.
    assert!(!response.status().is_success());
}
