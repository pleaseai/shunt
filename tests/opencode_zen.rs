#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end coverage for the `opencode` preset: the zen endpoint speaks
//! Anthropic Messages natively but reads the credential from `x-api-key`
//! only, so a preset reference alone must yield a working credential without
//! an explicit `auth` map.

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use shunt::{
    config::{Config, ModelConfig, UpstreamConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string_contains, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

/// Fails when the request carries an `authorization` header: the client's own
/// (wrong) bearer must be stripped before the preset key is injected, and zen
/// would reject a forwarded one as a missing key.
struct NoAuthorization;

impl wiremock::Match for NoAuthorization {
    fn matches(&self, request: &wiremock::Request) -> bool {
        !request.headers.contains_key("authorization")
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

/// A config whose single ordered upstream is a bare `opencode` preset
/// reference — the minimal declaration a user writes.
fn zen_config(mock_base_url: String) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![UpstreamConfig {
        name: "opencode".to_string(),
        provider: Some("opencode".to_string()),
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
        classifier_model: None,
        count_tokens: Default::default(),
        websocket: false,
        tool_search: None,
        request_compression: true,
        retry: Default::default(),
        workspace_roots: Vec::new(),
        profile_dir: None,
        sandbox: true,
    }];
    config.server.default_provider = "opencode".to_string();
    config.models = vec![ModelConfig {
        id: "claude-fable-5-1-via-zen".to_string(),
        display_name: None,
        upstream_model: Some(
            [("opencode".to_string(), "claude-fable-5-1".to_string())]
                .into_iter()
                .collect(),
        ),
        stage_router: None,
    }];
    // Point the preset's fixed zen base_url at the mock; everything else about
    // the preset (kind, auth, env, header) stays as shipped.
    config.upstreams[0].base_url = Some(mock_base_url);
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

#[tokio::test]
async fn preset_reference_sends_the_zen_key_in_x_api_key_only() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("OPENCODE_API_KEY", "sk-zen-test-key");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-zen-test-key"))
        // The client's own (wrong) bearer must be stripped, not forwarded: zen
        // rejects it as a missing key, and its absence pins that the preset
        // (not client passthrough) chose the credential shape.
        .and(NoAuthorization)
        // Trailing quote pins the REWRITTEN upstream id: the unrewritten
        // public id `claude-fable-5-1-via-zen` also contains the bare needle,
        // so without the quote a broken model-map rewrite would still match.
        .and(body_string_contains("claude-fable-5-1\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_mock",
            "type": "message",
            "role": "assistant",
            "model": "claude-fable-5-1",
            "content": [{ "type": "text", "text": "OK" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    // A catch-all that fails the request when the exact credential shape above
    // (x-api-key, no bearer, rewritten model) did not match.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway(zen_config(upstream.uri())).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        // The client's own (wrong) credential must be stripped, not forwarded.
        .header("authorization", "Bearer sk-client-wrong")
        .header("x-api-key", "sk-client-wrong")
        .body(
            serde_json::json!({
                "model": "claude-fable-5-1-via-zen",
                "max_tokens": 16,
                "messages": [{ "role": "user", "content": "Reply with OK." }],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "OK");
    drop(gateway);
    upstream.verify().await;
}

#[tokio::test]
async fn preset_reference_streams_sse_and_maps_usage_events() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("OPENCODE_API_KEY", "sk-zen-test-key");

    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"model\":\"claude-fable-5-1\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"stream\":true"))
        .and(header("x-api-key", "sk-zen-test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway(zen_config(upstream.uri())).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "claude-fable-5-1-via-zen",
                "max_tokens": 16,
                "stream": true,
                "messages": [{ "role": "user", "content": "Reply with OK." }],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: message_start"));
    assert!(body.contains("text_delta"));
    assert!(body.contains("event: message_stop"));
    drop(gateway);
    upstream.verify().await;
}
