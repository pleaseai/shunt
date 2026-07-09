use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use reqwest::StatusCode;
use shunt::{
    config::{Config, CountTokens, RoutePrefixConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string_contains, header, method, path, query_param},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

/// Exact, whole-value header matcher.
///
/// wiremock's built-in `header()` matcher splits comma-separated header values,
/// so it cannot assert that a value like `anthropic-beta: a,b=c` is forwarded
/// verbatim. This matcher compares the raw header value byte-for-byte.
struct ExactHeader(&'static str, &'static str);

impl Match for ExactHeader {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get(self.0)
            .and_then(|value| value.to_str().ok())
            == Some(self.1)
    }
}

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway(upstream_base_url: String) -> TestGateway {
    // Default providers, but point the anthropic passthrough at the mock upstream.
    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream_base_url;
    start_gateway_with(config).await
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let app = server::build_router(config);
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

#[tokio::test]
async fn head_root_returns_ok() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .head(format!("{}/", gateway.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().len(), 0);
}

#[tokio::test]
async fn messages_forwards_anthropic_headers_verbatim_and_preserves_query() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(query_param("beta", "true"))
        .and(ExactHeader(
            "anthropic-beta",
            "tools-2025-01-01,custom=value",
        ))
        .and(ExactHeader("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", gateway.base_url))
        .header("anthropic-beta", "tools-2025-01-01,custom=value")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-opus-4-1"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn messages_forwards_incoming_credentials_unchanged() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("authorization", "Bearer gateway-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("x-api-key", "sk-ant-test")
        .header("authorization", "Bearer gateway-token")
        .body(r#"{"model":"claude-sonnet-4-5"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn relays_sse_response_with_content_type_preserved() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let sse = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
               event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            // Use set_body_raw so the mock actually returns text/event-stream;
            // set_body_string would force content-type back to text/plain.
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(10))
                .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(r#"{"model":"claude-sonnet-4-5","stream":true}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.text().await.unwrap(), sse);
    upstream.verify().await;
}

#[tokio::test]
async fn upstream_error_status_and_body_are_returned_unmodified() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let error_body =
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad beta"}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("content-type", "application/json")
                .set_body_string(error_body),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(r#"{"model":"claude-sonnet-4-5"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.text().await.unwrap(), error_body);
    upstream.verify().await;
}

#[tokio::test]
async fn count_tokens_is_passed_through() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(body_string_contains("claude-sonnet-4-5"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages/count_tokens", gateway.base_url))
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), r#"{"input_tokens":7}"#);
    upstream.verify().await;
}

#[tokio::test]
async fn count_tokens_returns_404_for_responses_model() {
    if !can_bind_loopback() {
        return;
    }
    // The upstream must never be hit: a responses-model count_tokens is
    // short-circuited to 404 (so Claude Code estimates locally) rather than
    // translated into a billed inference call.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream.uri();
    config.route_prefixes = vec![RoutePrefixConfig {
        prefix: "gpt-".to_string(),
        provider: "codex".to_string(),
    }];
    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages/count_tokens", gateway.base_url))
        .body(r#"{"model":"gpt-5.6-sol","messages":[]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.text().await.unwrap().contains("count_tokens"));
    upstream.verify().await;
}

#[tokio::test]
async fn count_tokens_uses_tiktoken_when_enabled() {
    if !can_bind_loopback() {
        return;
    }
    // With count_tokens = tiktoken, shunt answers locally (200 + input_tokens)
    // without ever calling an upstream.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream.uri();
    config.providers.get_mut("codex").unwrap().count_tokens = CountTokens::Tiktoken;
    config.route_prefixes = vec![RoutePrefixConfig {
        prefix: "gpt-".to_string(),
        provider: "codex".to_string(),
    }];
    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages/count_tokens", gateway.base_url))
        .body(r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"Write a haiku about the sea."}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert!(body["input_tokens"].as_u64().unwrap() > 0);
    upstream.verify().await;
}
