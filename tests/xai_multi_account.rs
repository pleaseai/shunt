//! Gateway-level tests for `auth = "xai_oauth"` dispatch and the legacy-file rule.
//!
//! The xai_oauth guard pins the provider to an https grok.com host, so these tests
//! drive only outcomes that never leave the process (missing or `{}` credential
//! files, or a valid one whose upstream request stops at a loopback
//! `HTTPS_PROXY`). Pool rotation is covered in
//! `src/adapters/responses/pool_xai_tests.rs`.

use std::{io::ErrorKind, net::SocketAddr, path::PathBuf};

use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, CodexEndpointConfig, CodexRouteConfig, Config, RouteConfig},
    server,
};
use tokio::task::JoinHandle;

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

async fn start_gateway(mut config: Config) -> TestGateway {
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

fn temp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "shunt-xai-multi-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// Streaming twin of [`post_messages`]: returns the status, the content type,
/// and the raw SSE body, so a test can check the turn really streamed.
async fn post_stream(gateway: &TestGateway) -> (StatusCode, String, String) {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .json(&serde_json::json!({
            "model": "grok-pooled",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.unwrap();
    (status, content_type, body)
}

/// The parsed `data:` payload of the first `event: <name>` frame in an SSE body.
fn sse_event_data(body: &str, name: &str) -> serde_json::Value {
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if line == format!("event: {name}") {
            let data = lines
                .next()
                .and_then(|next| next.strip_prefix("data: "))
                .unwrap_or_else(|| panic!("event {name} has no data line; body: {body}"));
            return serde_json::from_str(data).expect("event data is json");
        }
    }
    panic!("no event: {name} frame; body: {body}");
}

fn grok_config() -> Config {
    let mut config = Config::default();
    config.routes.push(RouteConfig {
        model: "grok-pooled".to_string(),
        provider: "grok".to_string(),
        upstream_model: Some("grok-4.6".to_string()),
        effort: None,
        service_tier: None,
    });
    config
}

async fn post_messages(gateway: &TestGateway, stream: bool) -> (StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .json(&serde_json::json!({
            "model": "grok-pooled",
            "max_tokens": 16,
            "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn legacy_auth_file_answers_non_streaming_turns_when_the_xai_store_is_empty() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("legacy-non-stream");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", root.join("accounts"));
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let gateway = start_gateway(grok_config()).await;
    let (status, body) = post_messages(&gateway, false).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert!(
        body.contains("xAI auth not found; run shunt login xai"),
        "body: {body}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A loopback stand-in for an `HTTPS_PROXY`. An HTTPS upstream reaches it as
/// `CONNECT host:443`; it refuses every tunnel and resolves to the first
/// request line naming a grok.com host, so a test sees where the gateway was
/// headed while no request leaves the machine.
async fn refusing_proxy() -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => head.extend_from_slice(&buf[..read]),
                }
            }
            let _ = socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                .await;
            let line = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            if line.contains("grok.com") {
                return line;
            }
        }
    });
    (url, task)
}

/// The positive twin of the missing-file tests: with the store empty, a valid
/// legacy login is resolved and the turn goes upstream (stopped at a loopback
/// proxy) instead of failing credential resolution.
#[tokio::test]
async fn a_valid_legacy_auth_file_serves_turns_when_the_xai_store_is_empty() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("legacy-valid");
    std::fs::create_dir_all(&root).unwrap();
    let legacy = root.join("xai-auth.json");
    let access_token = format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::json!({"exp": 4_000_000_000u64}).to_string())
    );
    std::fs::write(
        &legacy,
        serde_json::json!({
            "tokens": {"access_token": access_token, "refresh_token": "legacy-refresh"}
        })
        .to_string(),
    )
    .unwrap();
    let (proxy, upstream) = refusing_proxy().await;
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", root.join("accounts"));
    vars.set("SHUNT_XAI_AUTH_FILE", &legacy);
    vars.set("HTTPS_PROXY", &proxy);
    vars.set("https_proxy", &proxy);
    vars.unset("NO_PROXY");
    vars.unset("no_proxy");

    let gateway = start_gateway(grok_config()).await;
    let (_status, body) = post_messages(&gateway, false).await;

    let request_line = tokio::time::timeout(std::time::Duration::from_secs(5), upstream)
        .await
        .expect("the turn never went upstream")
        .unwrap();
    assert!(
        request_line.starts_with("CONNECT ") && request_line.contains("grok.com:443"),
        "{request_line}"
    );
    assert!(!body.contains("xAI auth not found"), "body: {body}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn legacy_auth_file_answers_streaming_turns_when_the_xai_store_is_empty() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("legacy-stream");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", root.join("accounts"));
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let gateway = start_gateway(grok_config()).await;
    let (status, content_type, body) = post_stream(&gateway).await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        content_type.starts_with("text/event-stream"),
        "content-type: {content_type}"
    );
    assert!(body.contains("event: message_start"), "body: {body}");
    let error = sse_event_data(&body, "error");
    assert_eq!(error["type"], "error", "body: {body}");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("xAI auth not found; run shunt login xai")),
        "body: {body}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn named_xai_accounts_replace_the_legacy_auth_file() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("named-non-stream");
    let accounts = root.join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("a.json"), "{}").unwrap();
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", &accounts);
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let gateway = start_gateway(grok_config()).await;
    let (status, body) = post_messages(&gateway, false).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    assert!(body.contains("all upstreams failed"), "body: {body}");
    assert!(!body.contains("xAI auth not found"), "body: {body}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn named_xai_accounts_streaming_exhaustion_names_the_xai_pool() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("named-stream");
    let accounts = root.join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("a.json"), "{}").unwrap();
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", &accounts);
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let gateway = start_gateway(grok_config()).await;
    let (status, content_type, body) = post_stream(&gateway).await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        content_type.starts_with("text/event-stream"),
        "content-type: {content_type}"
    );
    assert!(body.contains("event: message_start"), "body: {body}");
    let error = sse_event_data(&body, "error");
    assert_eq!(error["type"], "error", "body: {body}");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message
                .contains("all xAI OAuth accounts failed before receiving an upstream response")),
        "body: {body}"
    );
    assert!(!body.contains("xAI auth not found"), "body: {body}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_xai_account_scope_naming_a_missing_account_is_a_gateway_error() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("scope-missing");
    let accounts = root.join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("real.json"), "{}").unwrap();
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", &accounts);
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let mut config = grok_config();
    config.providers.get_mut("grok").unwrap().account_scope = vec!["ghost".to_string()];
    let gateway = start_gateway(config).await;
    let (status, body) = post_messages(&gateway, false).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    assert!(
        body.contains("xAI account scope references missing store account"),
        "body: {body}"
    );
    assert!(body.contains("ghost"), "body: {body}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A `[[server.codex_endpoint.routes]]` entry sending `grok-pooled` to the
/// `grok` `xai_oauth` provider, reached through the inbound Codex endpoint.
fn routed_grok_config() -> Config {
    let mut config = Config::default();
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: "grok-pooled".to_string(),
            provider: "grok".to_string(),
            upstream_model: Some("grok-4.6".to_string()),
        }],
    });
    config
}

async fn post_inbound_responses(gateway: &TestGateway) -> (StatusCode, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/backend-api/codex/responses", gateway.base_url))
        .json(&serde_json::json!({
            "model": "grok-pooled",
            "instructions": "be brief",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "stream": false,
            "store": false,
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn routed_xai_named_account_failure_is_an_openai_shaped_error() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("routed-named");
    let accounts = root.join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    let mut vars = common::env_lock().await;
    vars.unset("SHUNT_TEST_XAI_POOL_TOKEN_UNSET");
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", &accounts);
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let mut config = routed_grok_config();
    config.providers.get_mut("grok").unwrap().accounts = vec![AccountConfig {
        name: "pooled".to_string(),
        token_env: Some("SHUNT_TEST_XAI_POOL_TOKEN_UNSET".to_string()),
        ..Default::default()
    }];
    let gateway = start_gateway(config).await;
    let (status, body) = post_inbound_responses(&gateway).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "{body}"
    );
    // OpenAI Responses error shape: `{"error": {...}}` with no top-level `type`.
    assert!(body["error"]["message"].is_string(), "body: {body}");
    assert!(body.get("type").is_none(), "body: {body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("SHUNT_TEST_XAI_POOL_TOKEN_UNSET"),
        "body: {body}"
    );
    assert!(!message.contains("xAI auth not found"), "body: {body}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn routed_xai_empty_pool_falls_back_to_the_legacy_file_error() {
    if !can_bind_loopback() {
        return;
    }
    let root = temp_root("routed-empty");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_XAI_ACCOUNTS_DIR", root.join("accounts"));
    vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"));

    let gateway = start_gateway(routed_grok_config()).await;
    let (status, body) = post_inbound_responses(&gateway).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "{body}"
    );
    assert!(body.get("type").is_none(), "body: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("xAI auth not found; run shunt login xai"),
        "body: {body}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
