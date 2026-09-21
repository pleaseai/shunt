#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end coverage for Claude Code's auto-mode server-side classifier on a
//! route served by a translation adapter.
//!
//! The client sends a top-level `safeguards` array and expects a matching
//! `safeguard_results` back; a completed turn that carries none at all makes it
//! retire the server classifier for the rest of the session. The Responses
//! adapter builds its own Messages-shaped response and has no such field to
//! relay, so the gateway synthesizes one (see `docs/notes/auto-mode-server-classifier.md`).

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use shunt::{
    config::{Config, ModelConfig, ProviderKind, RetryConfig, UpstreamConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

mod common;

const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"response\":{\"id\":\"resp_auto_mode\",\"usage\":{\"output_tokens\":0}}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"delta\":\"ok\"}\n\n",
    "event: response.output_text.done\n",
    "data: {}\n\n",
    "event: response.completed\n",
    "data: {\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    "data: [DONE]\n\n"
);

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

/// One Responses-kind upstream pointed at the mock.
fn responses_config(base_url: String) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![UpstreamConfig {
        name: "responses".to_string(),
        provider: None,
        kind: Some(ProviderKind::Responses),
        base_url: Some(base_url),
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
    }];
    config.server.default_provider = "responses".to_string();
    config.models = vec![ModelConfig {
        id: "auto-mode-test-model".to_string(),
        display_name: None,
        upstream_model: Some(
            [("responses".to_string(), "gpt-5.6-sol".to_string())]
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

#[tokio::test]
async fn a_translated_stream_answers_the_auto_mode_safeguards_request() {
    if !can_bind_loopback() {
        return;
    }
    let _env = common::env_lock().await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSES_SSE, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway(responses_config(upstream.uri())).await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "dangerous-tool-use-2026-09-03")
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "auto-mode-test-model",
                "max_tokens": 16,
                "stream": true,
                "messages": [{ "role": "user", "content": "Reply with OK." }],
                "safeguards": [{
                    "type": "dangerous_tool_use",
                    "classifier_context": {"cwd": "/tmp"},
                }],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    let delta = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .find(|value| value["type"] == "message_delta")
        .unwrap_or_else(|| panic!("the relay carries a message_delta frame; got:\n{body}"));

    assert_eq!(
        delta["delta"]["safeguard_results"],
        serde_json::json!([{
            "type": "dangerous_tool_use",
            "status": {"type": "available", "tool_uses": {}},
        }]),
        "got:\n{body}"
    );
    drop(gateway);
    upstream.verify().await;
}
