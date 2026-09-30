#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end pin for the prompt-cache affinity contract: a metadata-only
//! client (no `x-claude-code-session-id` header) must reach the upstream with
//! the `session-id`/`thread-id` headers AND the body `prompt_cache_key` all
//! carrying the `metadata.user_id` session. The backend derives cache affinity
//! from the header alone — a body key without the matching header caches
//! nothing (measured 2026-09-20 against the live ChatGPT backend,
//! openai/codex#44716) — so this pins the adapter wiring (`forward`'s
//! effective-id resolution) that the unit tests cannot reach.

use reqwest::StatusCode;
use shunt::{
    config::{
        AccountConfig, AccountSelection, AuthMap, Config, ModelConfig, UpstreamAuth, UpstreamConfig,
    },
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string_contains, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

const SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
    "event: response.content_part.added\n",
    "data: {\"type\":\"response.content_part.added\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n",
    "event: response.output_text.done\n",
    "data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"text\":\"hi\"}\n\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n",
);

async fn start_gateway(config: Config) -> (String, JoinHandle<()>) {
    let mut config = config;
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: std::net::SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), task)
}

#[tokio::test]
async fn a_metadata_only_client_reaches_the_upstream_with_matching_affinity_fields() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/codex/responses"))
        .and(header("session-id", "meta-sess-1"))
        .and(header("thread-id", "meta-sess-1"))
        .and(body_string_contains("\"prompt_cache_key\":\"meta-sess-1\""))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE))
        .expect(1)
        .mount(&mock)
        .await;

    // An inline raw-token account keeps the test off the box's real account
    // store, and the empty accounts-dir override keeps it off the real codex
    // auth file. The token is a bare JWT whose payload carries the
    // chatgpt_account_id claim `jwt_account_id` reads; nothing verifies the
    // signature. Both writes go through the shared env lock (issue #539).
    let accounts_dir = std::env::temp_dir().join(format!(
        "shunt-cache-affinity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&accounts_dir).unwrap();
    let _env = common::set_env(&[
        (
            "CACHE_AFFINITY_TEST_TOKEN",
            "eyJhbGciOiAibm9uZSIsICJ0eXAiOiAiSldUIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOiB7ImNoYXRncHRfYWNjb3VudF9pZCI6ICJhY2N0LXRlc3QtMSJ9fQ.sig",
        ),
        (
            "SHUNT_CODEX_ACCOUNTS_DIR",
            accounts_dir.to_str().unwrap(),
        ),
    ])
    .await;

    let mut config = Config::default();
    config.server.default_provider = "chatgpt".to_string();
    config.upstreams = vec![UpstreamConfig {
        name: "chatgpt".to_string(),
        provider: Some("codex".to_string()),
        kind: None,
        base_url: Some(format!("{}/v1", mock.uri())),
        auth: Some(UpstreamAuth::Map(AuthMap::ChatgptOauth {
            account: None,
            accounts: Some(vec![AccountSelection::Inline(AccountConfig {
                name: "test-account".to_string(),
                credentials: None,
                token_env: Some("CACHE_AFFINITY_TEST_TOKEN".to_string()),
                uuid: None,
                threshold: None,
                threshold_5h: None,
                threshold_7d: None,
                threshold_fable: None,
                priority: 0,
                disabled: false,
                store_entry: false,
                store_family: None,
            })]),
        })),
        effort: None,
        service_tier: None,
        classifier_model: None,
        count_tokens: Default::default(),
        websocket: false,
        tool_search: None,
        // The upstream must see the plain JSON body (the matcher reads it).
        request_compression: false,
        retry: Default::default(),
        workspace_roots: Vec::new(),
        profile_dir: None,
        sandbox: true,
    }];
    config.models = vec![ModelConfig {
        id: "gpt-5.6-luna".to_string(),
        display_name: None,
        upstream_model: Some(
            [("chatgpt".to_string(), "gpt-5.6-luna".to_string())]
                .into_iter()
                .collect(),
        ),
        router: None,
        stage_router: None,
        subagents: None,
    }];
    let (base_url, task) = start_gateway(config).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "gpt-5.6-luna",
            "max_tokens": 32,
            "stream": true,
            "metadata": {"user_id": "{\"device_id\":\"d1\",\"session_id\":\"meta-sess-1\"}"},
            "system": [{"type": "text", "text": "be terse"}],
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("message_stop"),
        "expected a completed SSE stream, got: {body}"
    );

    task.abort();
    std::fs::remove_dir_all(&accounts_dir).ok();
    // The mock's `expect(1)` is verified when the server drops at test end.
}
