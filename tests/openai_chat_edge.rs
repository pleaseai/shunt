#[allow(dead_code)]
mod openai_chat_support;

use std::{
    io::ErrorKind,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::Duration,
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{config::Config, server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use openai_chat_support::{
    anthropic_request, anthropic_streaming_request, chat_delta, collect_sse_events,
};

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var(self.key, previous),
            None => std::env::remove_var(self.key),
        }
    }
}

static EDGE_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static EDGE_ENV_COUNTER: AtomicU64 = AtomicU64::new(0);

async fn lock_env() -> tokio::sync::MutexGuard<'static, ()> {
    EDGE_ENV_LOCK
        .get_or_init(|| Mutex::const_new(()))
        .lock()
        .await
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => false,
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

fn unique_env_name(prefix: &str) -> &'static str {
    let id = EDGE_ENV_COUNTER.fetch_add(1, Ordering::Relaxed);
    Box::leak(format!("{prefix}_{id}").into_boxed_str())
}

fn config_for(base_url: &str, api_key_env: &str) -> Config {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    value["providers"]["openai-chat-test"] = json!({
        "kind": "openai_chat",
        "base_url": base_url,
        "auth": "api_key",
        "api_key_env": api_key_env
    });
    value["server"]["default_provider"] = json!("openai-chat-test");
    value["routes"] = json!([{
        "model": "claude-via-chat",
        "provider": "openai-chat-test",
        "upstream_model": "gpt-5",
        "effort": null,
        "service_tier": null
    }]);
    serde_json::from_value(value).unwrap()
}

struct Gateway {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway(mut config: Config) -> Gateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _, _) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Gateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// Serves one Chat SSE body and then leaves the socket open so the adapter
/// must finish on its post-`[DONE]` drain instead of waiting for EOF.
async fn open_sse_backend(body: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_string();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0_u8; 1024];
        let mut seen = Vec::new();
        loop {
            let Ok(n) = socket.read(&mut buf).await else {
                return;
            };
            if n == 0 {
                return;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        let chunk = format!("{:x}\r\n{}\r\n", body.len(), body);
        let _ = socket.write_all(header.as_bytes()).await;
        let _ = socket.write_all(chunk.as_bytes()).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn comment_after_done_still_completes() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_env().await;
    let api_key_env = unique_env_name("SHUNT_OPENAI_CHAT_EDGE_KEY");
    let _key = EnvVarGuard::set(api_key_env, "fixture-openai-key");

    let backend = MockServer::start().await;
    let body = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(
            format!(
                "data: {}\n\ndata: [DONE]\n\n: keep-alive\n\n",
                chat_delta(json!({"content": "done"}), Some("stop"))
            ),
            "text/event-stream",
        );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(body)
        .expect(1)
        .mount(&backend)
        .await;

    let gateway = start_gateway(config_for(&backend.uri(), api_key_env)).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(anthropic_streaming_request("claude-via-chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = collect_sse_events(response).await;
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| event == "message_stop")
            .count(),
        1,
        "{events:?}"
    );
    assert!(
        events.iter().all(|(event, _)| event != "error"),
        "{events:?}"
    );
}

#[tokio::test]
async fn open_body_after_done_completes_on_the_drain() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_env().await;
    let api_key_env = unique_env_name("SHUNT_OPENAI_CHAT_EDGE_KEY");
    let _key = EnvVarGuard::set(api_key_env, "fixture-openai-key");
    let payload = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        chat_delta(json!({"content": "held"}), Some("stop"))
    );
    let backend = open_sse_backend(&payload).await;
    let gateway = start_gateway(config_for(&backend, api_key_env)).await;
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        reqwest::Client::new()
            .post(format!("{}/v1/messages", gateway.base_url))
            .body(anthropic_streaming_request("claude-via-chat"))
            .send(),
    )
    .await
    .expect("an open body must not hang past the post-terminal drain")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = tokio::time::timeout(Duration::from_secs(3), collect_sse_events(response))
        .await
        .expect("relay must finish after the drain");
    assert!(
        events.iter().any(|(event, data)| {
            event == "content_block_delta" && data["delta"]["text"] == "held"
        }),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| event == "message_stop")
            .count(),
        1,
        "{events:?}"
    );
}

#[tokio::test]
async fn huge_json_error_message_is_truncated() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_env().await;
    let api_key_env = unique_env_name("SHUNT_OPENAI_CHAT_EDGE_KEY");
    let _key = EnvVarGuard::set(api_key_env, "fixture-openai-key");
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({
            "error": {"message": "y".repeat(12_000)}
        })))
        .expect(1)
        .mount(&backend)
        .await;
    let gateway = start_gateway(config_for(&backend.uri(), api_key_env)).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(anthropic_request("claude-via-chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.ends_with('…'), "{message}");
    assert!(message.len() < 5_000, "{}", message.len());
}
