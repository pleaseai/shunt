//! Integration coverage for bounded upstream retry/backoff (issues #48, #126).
//!
//! Drives the real proxy → Anthropic passthrough (and single-credential
//! Responses) path against a mock upstream, asserting the observable retry
//! contract. Messages and Responses are non-idempotent creation POSTs
//! (`RetrySafety::NonIdempotentPost`, issue #126), so a transient *status* is
//! surfaced immediately rather than retried — a response means the upstream may
//! already have accepted a billable generation. A non-transient status likewise
//! surfaces immediately, `count_tokens` never retries, and a success is never
//! re-issued (so a retry can never happen mid-stream). The still-idempotent
//! status-retry path and the transport-error-retry path (which stays live even
//! for a non-idempotent POST) are unit-tested directly in `src/retry.rs`.

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
    let (app, _shared, _state) = server::build_router(config).unwrap();
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
async fn transient_status_is_not_retried_on_messages_path() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // A transient 503 on the first (and only) attempt. A 200 is *also* mounted at
    // lower priority as a trap: were the non-idempotent Messages POST retried,
    // the second attempt would fall through to it and the client would see 200.
    // It must not — a response status means the upstream may already have
    // accepted a billable generation, so the 503 is surfaced immediately and the
    // 200 trap is never reached.
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
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .with_priority(2)
        .expect(0)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(retry_test_config(upstream.uri())).await;

    let response = post_messages(&gateway, "/v1/messages").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    // The mock expectations (503 hit once, 200 trap never hit) are verified when
    // `upstream` drops: proof no retry fired on the non-idempotent POST.
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
async fn messages_path_does_not_retry_transient_status_despite_enabled_policy() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    // A persistent 503 under the *default* (enabled) retry policy. An idempotent
    // path would hit this 3 times (1 try + 2 retries) before surfacing; the
    // non-idempotent Messages POST hits it exactly once. The acceptance-safety
    // gate — not an exhausted budget — is what stops the retry here (contrast
    // `disabled_policy_surfaces_transient_immediately`, which removes the budget
    // outright).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"still down"}"#),
        )
        .expect(1)
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
async fn responses_path_does_not_retry_transient_status() {
    if !can_bind_loopback() {
        return;
    }
    // A unique api-key env var (avoids colliding with a real OPENAI_API_KEY or a
    // parallel test) so the single-credential Responses path resolves its
    // credential and reaches forward_http, where the shared retry driver runs.
    std::env::set_var("SHUNT_TEST_OPENAI_RETRY_KEY", "sk-test-retry");

    let upstream = MockServer::start().await;
    // A persistent 503 on `/responses` under the default (enabled) retry policy.
    // Like the Anthropic path, the single-credential Responses POST is
    // non-idempotent (`RetrySafety::NonIdempotentPost`), so a transient status is
    // surfaced after exactly one upstream hit rather than retried through the
    // driver.
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"still down"}"#),
        )
        .expect(1)
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
    // The transient status surfaces as a non-success; the mock's `.expect(1)`
    // (verified on drop) is the proof no retry fired on this non-idempotent path.
    assert!(!response.status().is_success());
}
