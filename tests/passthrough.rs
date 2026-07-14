use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use reqwest::StatusCode;
use serde_json::{json, Value};
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

/// Asserts a header is *absent* from the forwarded request. wiremock has no
/// built-in absence matcher.
struct HeaderAbsent(&'static str);

impl Match for HeaderAbsent {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
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
async fn get_root_returns_landing_text_with_version_and_endpoints() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .get(format!("{}/", gateway.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains(&format!("shunt v{}", env!("CARGO_PKG_VERSION"))));
    assert!(body.contains("/v1/messages"));
    assert!(body.contains("/health"));
}

#[tokio::test]
async fn get_health_returns_ok_status_and_version() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .get(format!("{}/health", gateway.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
    );
}

#[tokio::test]
async fn get_protocol_returns_descriptor_format_and_version() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .get(format!("{}/protocol", gateway.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["format"], "anthropic-messages");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
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
async fn messages_drops_duplicate_x_api_key_for_oauth_bearer() {
    // Claude Code's `apiKeyHelper` sends its value in BOTH `x-api-key` and
    // `Authorization: Bearer`. For a subscription OAuth token (`sk-ant-oat…`)
    // the copy in `x-api-key` would make api.anthropic.com reject the request,
    // so shunt must forward only the bearer.
    if !can_bind_loopback() {
        return;
    }
    // Build the bearer value from parts so no contiguous `Bearer <token>` literal
    // appears (secret scanners flag such literals as hardcoded credentials).
    let token = "sk-ant-oat01-abc";
    let auth_header = format!("Bearer {token}");
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", auth_header.as_str()))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("x-api-key", token)
        .header("authorization", auth_header.as_str())
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
async fn count_tokens_returns_501_not_supported_for_responses_model() {
    if !can_bind_loopback() {
        return;
    }
    // The upstream must never be hit: with the opt-in estimate mode, a
    // responses-model count_tokens is short-circuited to 501 not_supported
    // (so the client falls back on its own) rather than translated into a
    // billed inference call.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream.uri();
    config.providers.get_mut("codex").unwrap().count_tokens = CountTokens::Estimate;
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

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "not_supported",
                "message": "count_tokens is not available for this model; Claude Code estimates tokens locally"
            }
        })
    );
    upstream.verify().await;
}

#[tokio::test]
async fn count_tokens_uses_tiktoken_by_default() {
    if !can_bind_loopback() {
        return;
    }
    // tiktoken is the default count_tokens mode: shunt answers locally
    // (200 + input_tokens) without ever calling an upstream.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream.uri();
    assert_eq!(
        config.provider("codex").unwrap().count_tokens,
        CountTokens::Tiktoken,
        "tiktoken must be the built-in default"
    );
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

#[tokio::test]
async fn responses_upstream_429_keeps_retry_after_header() {
    if !can_bind_loopback() {
        return;
    }
    // Claude Code honors Retry-After when backing off on 429; the responses
    // adapter re-shapes the upstream error body, and the header must survive.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "17")
                .set_body_string(r#"{"detail":"rate limited"}"#),
        )
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    // Passthrough auth on a responses provider sends no credential — fine for
    // a mock; it keeps the test free of key material.
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = upstream.uri();
    openai.auth = shunt::config::AuthMode::Passthrough;
    openai.api_key_env = None;
    config.route_prefixes = vec![RoutePrefixConfig {
        prefix: "gpt-".to_string(),
        provider: "openai".to_string(),
    }];
    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(r#"{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("17")
    );
}

/// Minimal Responses SSE stream: a `response.created` (which triggers
/// `message_start`) followed by a `response.completed` carrying the real
/// upstream usage. Enough to drive the responses→Anthropic translation E2E.
fn responses_sse_stream() -> Vec<u8> {
    concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":2}}}\n\n",
    )
    .as_bytes()
    .to_vec()
}

/// Pull `message.usage.input_tokens` out of the translated `message_start` SSE
/// event in a gateway streaming response.
fn message_start_input_tokens(sse: &str) -> u64 {
    for line in sse.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if value["type"] == "message_start" {
            return value["message"]["usage"]["input_tokens"]
                .as_u64()
                .expect("message_start usage.input_tokens must be an integer");
        }
    }
    panic!("no message_start event found in gateway SSE:\n{sse}");
}

/// Build a gateway that routes `gpt-` to a passthrough `openai` responses
/// provider pointed at `upstream`, with the given local token-counting mode.
fn responses_gateway_config(upstream_uri: String, count_tokens: CountTokens) -> Config {
    let mut config = Config::default();
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = upstream_uri;
    // Passthrough auth sends no credential — keeps the test free of key material.
    openai.auth = shunt::config::AuthMode::Passthrough;
    openai.api_key_env = None;
    openai.count_tokens = count_tokens;
    config.route_prefixes = vec![RoutePrefixConfig {
        prefix: "gpt-".to_string(),
        provider: "openai".to_string(),
    }];
    config
}

const RESPONSES_STREAM_REQUEST: &str = r#"{"model":"gpt-5.6-sol","stream":true,"max_tokens":16,"messages":[{"role":"user","content":"Write a haiku about the sea."}]}"#;

#[tokio::test]
async fn message_start_seeds_tiktoken_estimate_for_streaming_responses_model() {
    if !can_bind_loopback() {
        return;
    }
    // The forward-level wiring: with count_tokens = "tiktoken" (the default for
    // mapped providers), a *streaming* responses turn seeds message_start's
    // usage.input_tokens with the local tiktoken estimate — so Claude Code's
    // per-subagent progress indicator shows live context instead of a stuck 0.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(responses_sse_stream(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(responses_gateway_config(
        upstream.uri(),
        CountTokens::Tiktoken,
    ))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(RESPONSES_STREAM_REQUEST)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.unwrap();
    assert!(
        message_start_input_tokens(&sse) > 0,
        "expected a nonzero tiktoken estimate in message_start; got:\n{sse}"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn message_start_input_tokens_is_zero_when_count_tokens_estimate() {
    if !can_bind_loopback() {
        return;
    }
    // count_tokens = "estimate" opts out of the local encode entirely: no
    // tiktoken work runs and message_start stays at 0 (the client estimates on
    // its own), mirroring the count_tokens 404 opt-out.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(responses_sse_stream(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway_with(responses_gateway_config(
        upstream.uri(),
        CountTokens::Estimate,
    ))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(RESPONSES_STREAM_REQUEST)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.unwrap();
    assert_eq!(
        message_start_input_tokens(&sse),
        0,
        "estimate mode must leave message_start at 0; got:\n{sse}"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn messages_strip_empty_text_blocks_before_forwarding_upstream() {
    // Regression for #132: a Codex/LiteLLM turn can persist an empty
    // `{"type":"text","text":""}` block in the transcript. When the client
    // switches back to a native Claude model, that poisoned request is
    // forwarded to api.anthropic.com, which rejects empty text blocks with a
    // 400 ("text content blocks must be non-empty"). The gateway must strip
    // empty/whitespace-only text blocks on the way through (keeping tool_use /
    // thinking) so the switch stays valid. This exercises the real forward()
    // HTTP path end-to-end, not just the normalize_empty_text_blocks helper.
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let gateway = start_gateway(upstream.uri()).await;

    let request_body = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{
            "role": "assistant",
            "content": [
                {"type": "text", "text": ""},
                {"type": "text", "text": "  \n"},
                {"type": "tool_use", "id": "tool_1", "name": "work", "input": {}}
            ]
        }]
    });

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(serde_json::to_vec(&request_body).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let forwarded: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        forwarded["messages"][0]["content"],
        json!([{"type": "tool_use", "id": "tool_1", "name": "work", "input": {}}]),
        "empty text blocks must be stripped before reaching the upstream"
    );
    upstream.verify().await;
}
