//! Codex WebSocket v2 transport (issue #32) — HTTP fallback safety net.
//!
//! Enabling `websocket = true` must never do worse than plain HTTP: when the
//! websocket cannot be established, the turn is transparently re-driven over the
//! HTTP Responses path. Here the upstream is a plain HTTP mock that has no
//! websocket endpoint, so the handshake fails and the request must still succeed
//! over HTTP.

use std::fs;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, Config, PoolConfig, RouteConfig},
    server,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

struct TestGateway {
    base_url: String,
    /// The router's state, so a test can inspect the account pool a turn fed.
    state: server::AppState,
    task: JoinHandle<()>,
}

struct LogWriter {
    output: Arc<StdMutex<Vec<u8>>>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reprobe_log_subscriber(
    output: &Arc<StdMutex<Vec<u8>>>,
) -> impl tracing::Subscriber + Send + Sync {
    let writer_output = Arc::clone(output);
    tracing_subscriber::fmt()
        .with_writer(move || LogWriter {
            output: Arc::clone(&writer_output),
        })
        .with_ansi(false)
        .without_time()
        .finish()
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    shunt::state_persist::restore(&state).await;
    let state = state.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        state,
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

/// A minimal unsigned JWT (`x.<payload>.y`) with a far-future `exp`, so the codex
/// auth store treats it as valid without any network refresh.
fn fake_jwt(exp: u64) -> String {
    fake_jwt_for_account(exp, "acct_fallback")
}

fn fake_jwt_for_account(exp: u64, account_id: &str) -> String {
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn write_stale_pool_state(path: &std::path::Path, account_id: &str) {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 61;
    let state = serde_json::json!({
        "version": 2,
        "accounts": [{
            "key": {
                "store_family": "chatgpt",
                "identity": {"kind": "verified", "id": account_id}
            },
            "quota": {
                "utilization_5h": 0.9,
                "observed_at_5h": observed_at
            }
        }]
    });
    fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
}

/// Point `SHUNT_CODEX_ACCOUNTS_DIR` at a fresh empty dir for the test's
/// lifetime, through the caller's guard: a host with real shunt-managed codex
/// accounts (`~/.shunt/accounts/codex`) must not leak them into the unpooled
/// tests' credential resolution. The guard deletes the dir on drop.
struct EmptyAccountsDir(std::path::PathBuf);

impl Drop for EmptyAccountsDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pin_empty_accounts_dir(vars: &mut common::EnvVars) -> EmptyAccountsDir {
    let dir = std::env::temp_dir().join(format!(
        "shunt-ws-accounts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    EmptyAccountsDir(dir)
}

/// The guard removes its directory when it drops: the suite must not
/// accumulate empty account dirs in the system temp dir.
#[tokio::test]
async fn empty_accounts_dir_guard_removes_its_dir_on_drop() {
    let mut vars = common::env_lock().await;
    let guard = pin_empty_accounts_dir(&mut vars);
    let dir = guard.0.clone();
    assert!(dir.is_dir(), "dir exists while the guard lives");
    drop(guard);
    assert!(!dir.exists(), "dir is removed when the guard drops");
}

/// Write a codex-style `auth.json` a valid ChatGPT credential can be read from,
/// and point `CODEX_AUTH_FILE` at it. Returns the path for cleanup.
fn write_fake_codex_auth(vars: &mut common::EnvVars) -> PathBuf {
    let unique_name = format!(
        "shunt-ws-fallback-auth-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique_name);
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_add(3_600);
    let auth = serde_json::json!({
        "tokens": {
            "access_token": fake_jwt(expires_at),
            "refresh_token": "refresh-xyz",
            "account_id": "acct_fallback"
        }
    });
    std::fs::write(&path, serde_json::to_vec(&auth).unwrap()).unwrap();
    // Through the caller's guard, not `set_var` directly: the variable has to be
    // removed when the test ends however it ends, and the write has to happen
    // under the same lock the caller is holding (issue #539).
    vars.set("CODEX_AUTH_FILE", &path);
    path
}

/// A minimal Responses SSE stream the HTTP path translates into an Anthropic
/// message carrying the assistant text.
const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"output_tokens\":0}}}\n\n",
    "event: response.output_item.added\n",
    "data: {\"item\":{\"type\":\"message\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"delta\":\"served over HTTP fallback\"}\n\n",
    "event: response.output_text.done\n",
    "data: {}\n\n",
    "event: response.completed\n",
    "data: {\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":4}}}\n\n",
    "data: [DONE]\n\n"
);

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

#[tokio::test]
async fn websocket_handshake_failure_falls_back_to_http() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    // Upstream speaks only HTTP: it serves the Responses POST but has no websocket
    // endpoint, so the codex ws handshake (a GET upgrade) 404s and must fall back.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSES_SSE))
        .mount(&upstream)
        .await;

    let auth_path = write_fake_codex_auth(&mut vars);

    let mut config = Config::default();
    {
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = upstream.uri();
        codex.websocket = true; // opt in to the ws transport (should fail → HTTP)
    }
    config.routes.push(RouteConfig {
        model: "codex-fallback-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });

    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the turn succeeds over the HTTP fallback despite the ws handshake failing"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("served over HTTP fallback"),
        "fallback response carries the upstream's translated text; got: {body}"
    );

    // The upstream saw the HTTP Responses POST (proving the fallback ran).
    let requests = upstream
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests
            .iter()
            .any(|r| r.method.as_str() == "POST" && r.url.path() == "/codex/responses"),
        "the HTTP Responses endpoint was called by the fallback"
    );

    let _ = std::fs::remove_file(auth_path);
}

#[tokio::test]
async fn streaming_ws_fallback_still_seeds_message_start_estimate() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    // Streaming variant of the fallback: codex defaults to count_tokens = tiktoken,
    // so forward() builds an input-token estimate. The ws attempt fails (HTTP-only
    // upstream) and forward_http re-runs the encode and seeds message_start — so
    // this exercises forward_websocket's estimate-handle spawn, the ws→http
    // double-encode fallback path, and the estimate surviving into message_start.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(RESPONSES_SSE.as_bytes().to_vec(), "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let auth_path = write_fake_codex_auth(&mut vars);

    let mut config = Config::default();
    {
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = upstream.uri();
        codex.websocket = true; // opt in to the ws transport (should fail → HTTP)
    }
    config.routes.push(RouteConfig {
        model: "codex-fallback-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });

    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"Write a haiku about the sea."}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.unwrap();
    // The tiktoken estimate (nonzero) is seeded even though usage only arrives in
    // the terminal message_delta — proving the estimate survives the ws→http
    // fallback on the streaming path.
    assert!(
        message_start_input_tokens(&sse) > 0,
        "message_start must carry the tiktoken estimate after ws→http fallback; got:\n{sse}"
    );

    let _ = std::fs::remove_file(auth_path);
}

/// When the mock websocket drops the socket: before it has emitted any event
/// (nothing has reached the client, so the turn is safely re-driven over HTTP),
/// or after a first event (streaming has begun, so a restart would duplicate
/// output — the drop must surface as a clean error instead). `CompleteTurn`
/// drops nothing: it streams a whole turn, including the backend's in-stream
/// `codex.rate_limits` event. `WrappedRefusal` answers with the backend's
/// wrapped HTTP-class `error` frame (`status: 400`) as the first event, and the
/// HTTP half refuses the fallback with an HTTP 400 [`HTTP_REFUSAL_DETAIL`].
#[derive(Clone, Copy)]
enum WsDrop {
    BeforeFirstEvent,
    AfterFirstEvent,
    CompleteTurn,
    WrappedRefusal,
}

/// The `detail` the HTTP half's refusal carries under [`WsDrop::WrappedRefusal`],
/// distinct from the websocket frame's message so a test can tell which
/// transport answered.
const HTTP_REFUSAL_DETAIL: &str = "model not supported (HTTP fallback)";

/// Build a codex-provider config with the websocket transport enabled, pointing
/// both the websocket and HTTP paths at `base_url`.
fn codex_ws_config(base_url: String) -> Config {
    let mut config = Config::default();
    {
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = base_url;
        codex.websocket = true;
    }
    config.routes.push(RouteConfig {
        model: "codex-fallback-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    config
}

fn pooled_codex_ws_config(base_url: String, token_envs: [&str; 2]) -> Config {
    let mut config = codex_ws_config(base_url);
    config.providers.get_mut("codex").unwrap().accounts = token_envs
        .into_iter()
        .enumerate()
        .map(|(index, token_env)| AccountConfig {
            name: format!("account-{index}"),
            token_env: Some(token_env.to_string()),
            ..Default::default()
        })
        .collect();
    config
}

fn pooled_codex_ws_config_with_state(
    base_url: String,
    token_envs: [&str; 2],
    state_path: PathBuf,
) -> Config {
    let mut config = pooled_codex_ws_config(base_url, token_envs);
    config.server.pool = Some(PoolConfig {
        default_threshold: Some(0.5),
        reprobe_seconds: Some(60),
        state_path: Some(state_path),
        ..Default::default()
    });
    config
}

/// A mock Codex upstream that serves BOTH the websocket upgrade and the HTTP
/// Responses `POST` on one port, so a turn can open a socket, have it drop, and
/// fall back to HTTP against the same `base_url`. The websocket half performs the
/// handshake then applies `drop`; the HTTP half always answers [`RESPONSES_SSE`]
/// and increments the returned counter, so a test can assert whether the HTTP
/// fallback ran. Returns the upstream base URL and that counter.
async fn spawn_dual_upstream(drop: WsDrop) -> (String, Arc<AtomicUsize>) {
    spawn_counted_dual_upstream(drop, Arc::new(AtomicUsize::new(0))).await
}

async fn spawn_counted_dual_upstream(
    drop: WsDrop,
    total_hits: Arc<AtomicUsize>,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let http_hits = Arc::new(AtomicUsize::new(0));
    let hits = http_hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            total_hits.fetch_add(1, Ordering::SeqCst);
            if request_is_websocket(&socket).await {
                tokio::spawn(serve_ws(socket, drop));
            } else {
                hits.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve_http(socket, drop));
            }
        }
    });
    (format!("http://{addr}"), http_hits)
}

/// Peek the leading bytes to tell a websocket upgrade (`GET`) from the HTTP
/// Responses `POST`, without consuming them so the handshake still sees the whole
/// request.
async fn request_is_websocket(socket: &TcpStream) -> bool {
    let mut head = [0u8; 4];
    loop {
        match socket.peek(&mut head).await {
            Ok(0) | Err(_) => return false,
            Ok(n) if n >= 4 => return &head == b"GET ",
            // A partial read (<4 bytes) leaves the peeked bytes buffered, so the
            // next peek returns immediately with the same count — back off briefly
            // instead of busy-looping until the rest of the request line arrives.
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
}

/// Complete the websocket handshake, wait for the client's `response.create`
/// frame (so the drop is deterministic), optionally stream a first event, then
/// drop the socket — a truncation before any terminal event.
async fn serve_ws(socket: TcpStream, drop: WsDrop) {
    let Ok(mut ws) =
        tokio_tungstenite::accept_async_with_config(socket, Some(WebSocketConfig::default())).await
    else {
        return;
    };
    let _ = ws.next().await; // the client's response.create frame
    if let WsDrop::WrappedRefusal = drop {
        let frame = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"model not supported (websocket)"}}"#;
        ws.send(Message::Text(frame.to_string().into()))
            .await
            .expect("mock upstream should send the wrapped error frame");
        return;
    }
    if let WsDrop::CompleteTurn = drop {
        for event in [
            r#"{"type":"response.created","response":{"id":"resp_ws"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","delta":"served over websocket"}"#,
            r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":26.0,"window_minutes":10080}}}"#,
            r#"{"type":"response.output_text.done"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":2}}}"#,
        ] {
            ws.send(Message::Text(event.to_string().into()))
                .await
                .expect("mock upstream should stream the whole turn");
        }
        let _ = ws.send(Message::Close(None)).await;
        return;
    }
    if let WsDrop::AfterFirstEvent = drop {
        for event in [
            r#"{"type":"response.created","response":{"id":"resp_ws"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","delta":"partial over websocket"}"#,
        ] {
            // Surface a send failure loudly rather than swallowing it: a dropped
            // event would silently break the "partial over websocket" assertions
            // and make the AfterFirstEvent tests non-deterministic.
            ws.send(Message::Text(event.to_string().into()))
                .await
                .expect("mock upstream should stream the event before dropping");
        }
    }
    // Dropping `ws` closes the socket before a terminal event.
}

/// Answer an HTTP Responses `POST` with [`RESPONSES_SSE`]. Reads the full request
/// first so the client's write completes before the reply (a `content-length`
/// body lets the client finish reading before the socket closes).
async fn serve_http(mut socket: TcpStream, drop: WsDrop) {
    drain_http_request(&mut socket).await;
    if let WsDrop::WrappedRefusal = drop {
        let body = serde_json::json!({"detail": HTTP_REFUSAL_DETAIL}).to_string();
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
        return;
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
        RESPONSES_SSE.len(),
        RESPONSES_SSE
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Serve complete turns per accepted socket until Close and capture each
/// request frame. A pooled metadata session uses one socket across two turns;
/// the hash-fallback control uses two one-turn sockets.
async fn spawn_recording_ws_upstream(
    expected_connections: usize,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<StdMutex<Vec<serde_json::Value>>>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(StdMutex::new(Vec::new()));
    let server_accepted = Arc::clone(&accepted);
    let server_frames = Arc::clone(&frames);
    let server = tokio::spawn(async move {
        let mut handlers = JoinSet::new();
        for _ in 0..expected_connections {
            let (socket, _) = listener.accept().await.unwrap();
            server_accepted.fetch_add(1, Ordering::SeqCst);
            let frames = Arc::clone(&server_frames);
            handlers.spawn(async move {
                let mut ws = tokio_tungstenite::accept_async_with_config(
                    socket,
                    Some(WebSocketConfig::default()),
                )
                .await
                .unwrap();
                while let Some(message) = ws.next().await {
                    match message.unwrap() {
                        Message::Text(frame) => {
                            let frame: serde_json::Value =
                                serde_json::from_str(frame.as_str()).unwrap();
                            let turn = {
                                let mut frames = frames.lock().unwrap();
                                frames.push(frame);
                                frames.len()
                            };
                            let response_id = format!("resp_{turn}");
                            for event in [
                                serde_json::json!({
                                    "type": "response.created",
                                    "response": {"id": response_id}
                                }),
                                serde_json::json!({
                                    "type": "response.output_item.added",
                                    "item": {"type": "message"}
                                }),
                                serde_json::json!({
                                    "type": "response.output_text.delta",
                                    "delta": "hello"
                                }),
                                serde_json::json!({"type": "response.output_text.done"}),
                                serde_json::json!({
                                    "type": "response.output_item.done",
                                    "item": {
                                        "type": "message",
                                        "role": "assistant",
                                        "id": format!("msg_{turn}"),
                                        "phase": "final_answer",
                                        "status": "completed",
                                        "content": [{
                                            "type": "output_text",
                                            "text": "hello",
                                            "annotations": [],
                                            "logprobs": []
                                        }]
                                    }
                                }),
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": response_id,
                                        "usage": {"input_tokens": 5, "output_tokens": 1}
                                    }
                                }),
                            ] {
                                ws.send(Message::Text(event.to_string().into()))
                                    .await
                                    .unwrap();
                            }
                        }
                        Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                        Message::Pong(_) => {}
                        Message::Close(_) => break,
                        other => panic!("unexpected frame: {other:?}"),
                    }
                }
            });
        }
        while handlers.join_next().await.is_some() {}
    });
    (format!("http://{addr}"), accepted, frames, server)
}

/// Serve complete turns per accepted socket until Close and record each
/// connection's codex identity handshake headers. Like
/// [`spawn_recording_ws_upstream`] but capturing the upgrade request's
/// headers, so a test can pin which thread identity each pooled socket was
/// opened under.
async fn spawn_handshake_recording_ws_upstream(
    expected_connections: usize,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<StdMutex<Vec<serde_json::Value>>>,
    JoinHandle<()>,
) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    const IDENTITY_HEADERS: &[&str] = &[
        "session-id",
        "thread-id",
        "x-client-request-id",
        "x-codex-window-id",
        "x-codex-parent-thread-id",
        "x-openai-subagent",
    ];

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let handshakes = Arc::new(StdMutex::new(Vec::new()));
    let server_accepted = Arc::clone(&accepted);
    let server_handshakes = Arc::clone(&handshakes);
    let server = tokio::spawn(async move {
        let mut handlers = JoinSet::new();
        for _ in 0..expected_connections {
            let (socket, _) = listener.accept().await.unwrap();
            server_accepted.fetch_add(1, Ordering::SeqCst);
            let handshakes = Arc::clone(&server_handshakes);
            handlers.spawn(async move {
                let recorder = {
                    let handshakes = Arc::clone(&handshakes);
                    #[allow(clippy::result_large_err)]
                    move |request: &Request, response: Response| -> Result<Response, tungstenite::handshake::server::ErrorResponse> {
                        let mut recorded = serde_json::Map::new();
                        for name in IDENTITY_HEADERS {
                            if let Some(value) = request
                                .headers()
                                .get(*name)
                                .and_then(|value| value.to_str().ok())
                            {
                                recorded.insert((*name).to_string(), serde_json::json!(value));
                            }
                        }
                        handshakes.lock().unwrap().push(serde_json::Value::Object(recorded));
                        Ok(response)
                    }
                };
                let mut ws = tokio_tungstenite::accept_hdr_async_with_config(
                    socket,
                    recorder,
                    Some(WebSocketConfig::default()),
                )
                .await
                .unwrap();
                while let Some(message) = ws.next().await {
                    match message.unwrap() {
                        Message::Text(_) => {
                            for event in [
                                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
                                r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":1}}}"#,
                            ] {
                                ws.send(Message::Text(event.to_string().into()))
                                    .await
                                    .unwrap();
                            }
                        }
                        Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                        Message::Pong(_) => {}
                        Message::Close(_) => break,
                        other => panic!("unexpected frame: {other:?}"),
                    }
                }
            });
        }
        while handlers.join_next().await.is_some() {}
    });
    (format!("http://{addr}"), accepted, handshakes, server)
}

/// Read an HTTP request's headers and `content-length` body off the socket, so
/// the client finishes sending before the mock replies.
async fn drain_http_request(socket: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let content_length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut remaining = content_length.saturating_sub(buf.len() - (pos + 4));
            while remaining > 0 {
                match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => remaining = remaining.saturating_sub(n),
                }
            }
            return;
        }
        match socket.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
}

/// A websocket that drops *before* streaming any event — an idle-eviction race,
/// a backend hiccup — must re-drive the turn over HTTP, exactly like a failed
/// handshake, since nothing has reached the client yet (issue #46).
#[tokio::test]
async fn websocket_drop_before_first_event_falls_back_to_http() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::BeforeFirstEvent).await;
    let auth_path = write_fake_codex_auth(&mut vars);
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the turn recovers over HTTP after the socket drops before any event"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("served over HTTP fallback"),
        "the recovered response carries the HTTP upstream's translated text; got: {body}"
    );
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        1,
        "the fallback POSTs the turn to the HTTP endpoint exactly once (no double-send)"
    );

    let _ = std::fs::remove_file(auth_path);
}

/// A pre-response refusal the backend wraps as an `error` frame with
/// `status: 400` is re-shaped like a refused handshake rather than committed as
/// a `502` stream: the turn falls back to HTTP, and the client receives that
/// transport's `400 invalid_request_error` with its message.
#[tokio::test]
async fn websocket_wrapped_status_error_surfaces_the_refusal_status() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::WrappedRefusal).await;
    let auth_path = write_fake_codex_auth(&mut vars);
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert_eq!(body["error"]["message"], HTTP_REFUSAL_DETAIL, "{body}");
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        1,
        "the wrapped refusal falls back to HTTP exactly once"
    );

    let _ = std::fs::remove_file(auth_path);
}

/// End-to-end: a completed websocket turn whose stream carries the backend's
/// in-stream `codex.rate_limits` event records the reported window against the
/// observed Codex account, so `GET /usage` no longer reports `null` for a
/// websocket-only pool (the handshake headers alone miss every reused turn).
#[tokio::test]
async fn websocket_rate_limits_event_records_account_quota() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::CompleteTurn).await;
    let auth_path = write_fake_codex_auth(&mut vars);
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("served over websocket"),
        "the turn streamed over the websocket; got: {body}"
    );
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        0,
        "a completed websocket turn never falls back to HTTP"
    );

    // The unpooled Codex CLI credential is still an observed account, keyed by
    // the auth file's account id (see `write_fake_codex_auth`).
    let observed = vec![AccountConfig {
        name: "local-codex".to_string(),
        uuid: Some("acct_fallback".to_string()),
        ..Default::default()
    }];
    let snaps = gateway
        .state
        .accounts
        .snapshot("codex", &observed, None, None);
    assert_eq!(
        snaps[0].utilization_7d,
        Some(0.26),
        "the in-stream rate-limit event feeds the account pool"
    );

    let _ = std::fs::remove_file(auth_path);
}

/// A websocket that drops *after* a first event has streamed must NOT restart the
/// turn (that would duplicate the tokens already sent). The tokens streamed so far
/// reach the client, the drop surfaces as a clean Anthropic `error` event, and no
/// HTTP fallback is attempted (issue #46).
#[tokio::test]
async fn websocket_drop_after_first_event_surfaces_clean_error() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::AfterFirstEvent).await;
    let auth_path = write_fake_codex_auth(&mut vars);
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the response was already committed when the socket dropped mid-stream"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("partial over websocket"),
        "tokens streamed before the drop reach the client; got: {body}"
    );
    assert!(
        body.contains("event: error"),
        "the mid-stream drop surfaces as a clean Anthropic error event; got: {body}"
    );
    assert!(
        !body.contains("served over HTTP fallback"),
        "a mid-stream failure must not restart over HTTP once tokens have streamed; got: {body}"
    );
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        0,
        "no HTTP fallback POST is made after streaming has begun"
    );

    let _ = std::fs::remove_file(auth_path);
}

#[tokio::test]
async fn pooled_websocket_drop_after_first_event_stops_without_http_or_rotation() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let total_hits = Arc::new(AtomicUsize::new(0));
    let (base_url, http_hits) =
        spawn_counted_dual_upstream(WsDrop::AfterFirstEvent, total_hits.clone()).await;
    let token = fake_jwt(4_000_000_000);
    vars.set("SHUNT_WS_POOL_TOKEN_A", &token);
    vars.set("SHUNT_WS_POOL_TOKEN_B", &token);
    let gateway = start_gateway_with(pooled_codex_ws_config(
        base_url,
        ["SHUNT_WS_POOL_TOKEN_A", "SHUNT_WS_POOL_TOKEN_B"],
    ))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(http_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        total_hits.load(Ordering::SeqCst),
        1,
        "only the first account's websocket may be attempted"
    );
}

#[tokio::test]
async fn websocket_pool_does_not_reprobe_restored_stale_account_on_http_fallback() {
    // WebSocket-enabled outbound pools must bypass opportunistic re-probing.
    // The stale account is restored from a v2 snapshot, so a mistaken probe
    // would put it first. The socket drops before an event and the same selected
    // healthy account must then complete the HTTP fallback without a second pool
    // selection or a stale-account dispatch.
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let token_a = fake_jwt_for_account(4_000_000_000, "acct-ws-restored-a");
    let token_b = fake_jwt_for_account(4_000_000_000, "acct-ws-restored-b");
    vars.set("SHUNT_WS_REPROBE_TOKEN_A", &token_a);
    vars.set("SHUNT_WS_REPROBE_TOKEN_B", &token_b);

    let state_dir = std::env::temp_dir().join(format!(
        "shunt-ws-reprobe-state-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&state_dir).unwrap();
    let state_path = state_dir.join("pool-state.json");
    write_stale_pool_state(&state_path, "acct-ws-restored-a");

    let total_hits = Arc::new(AtomicUsize::new(0));
    let (base_url, http_hits) =
        spawn_counted_dual_upstream(WsDrop::BeforeFirstEvent, total_hits).await;
    let gateway = start_gateway_with(pooled_codex_ws_config_with_state(
        base_url,
        ["SHUNT_WS_REPROBE_TOKEN_A", "SHUNT_WS_REPROBE_TOKEN_B"],
        state_path,
    ))
    .await;

    let logs = Arc::new(StdMutex::new(Vec::new()));
    let subscriber = reprobe_log_subscriber(&logs);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-1",
        "WebSocket-enabled selection must not promote the restored stale account"
    );
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("served over HTTP fallback"));
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        1,
        "the selected account's websocket failure falls back over HTTP once"
    );
    drop(_subscriber_guard);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("opportunistically re-probing a stale near-quota account")
            .count(),
        0,
        "WebSocket selection and its HTTP fallback must not record a probe: {logs}"
    );

    fs::remove_dir_all(state_dir).ok();
}

fn conversation_body(user_id: &str, messages: serde_json::Value) -> String {
    serde_json::json!({
        "model": "codex-fallback-model",
        "max_tokens": 16,
        "stream": false,
        "metadata": {"user_id": user_id},
        "messages": messages
    })
    .to_string()
}

async fn send_turn(base_url: &str, body: String) -> (StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("{base_url}/v1/messages"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn metadata_session_reuses_websocket_but_hashed_user_id_does_not() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);
    let auth_path = write_fake_codex_auth(&mut vars);

    let first_messages = serde_json::json!([{"role": "user", "content": "hi"}]);
    let second_messages = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
        {"role": "user", "content": "bye"}
    ]);

    let metadata_id = r#"{"session_id":"meta-conversation"}"#;
    let (base_url, metadata_accepts, metadata_frames, metadata_server) =
        spawn_recording_ws_upstream(2).await;
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;
    let (status, body) = send_turn(
        &gateway.base_url,
        conversation_body(metadata_id, first_messages.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first metadata turn failed: {body}");
    let (status, body) = send_turn(
        &gateway.base_url,
        conversation_body(metadata_id, second_messages.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "second metadata turn failed: {body}"
    );
    let frames = metadata_frames.lock().unwrap().clone();
    assert_eq!(
        metadata_accepts.load(Ordering::SeqCst),
        1,
        "metadata session must reuse one websocket"
    );
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1]["previous_response_id"], "resp_1");
    assert_eq!(
        frames[1]["input"],
        serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "bye"}]
        }])
    );
    drop(gateway);
    metadata_server.abort();

    let plain_user = "plain-user-for-pool-control";
    let (base_url, hash_accepts, hash_frames, hash_server) = spawn_recording_ws_upstream(2).await;
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;
    let (status, body) = send_turn(
        &gateway.base_url,
        conversation_body(plain_user, first_messages),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first hash turn failed: {body}");
    let (status, body) = send_turn(
        &gateway.base_url,
        conversation_body(plain_user, second_messages),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second hash turn failed: {body}");
    let frames = hash_frames.lock().unwrap().clone();
    assert_eq!(
        hash_accepts.load(Ordering::SeqCst),
        2,
        "hashed user id must keep each websocket one-shot"
    );
    assert_eq!(frames.len(), 2);
    assert!(frames[1].get("previous_response_id").is_none());
    assert_eq!(frames[1]["input"].as_array().unwrap().len(), 3);
    assert_eq!(frames[0]["prompt_cache_key"], "3834f31dbf734510");
    assert_eq!(frames[1]["prompt_cache_key"], "3834f31dbf734510");
    drop(gateway);
    hash_server.abort();

    let _ = std::fs::remove_file(auth_path);
}

/// The non-streaming analogue of the mid-stream drop: a `stream:false` client
/// whose socket drops after the first event. The turn is already committed (the
/// first event was peeked), so `json_events_response` must surface the truncation
/// as a gateway error rather than presenting the partial output as a successful
/// 200 — and it must NOT fall back to HTTP once the turn is under way (issue #46).
#[tokio::test]
async fn websocket_drop_after_first_event_json_surfaces_gateway_error() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::AfterFirstEvent).await;
    let auth_path = write_fake_codex_auth(&mut vars);
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "a non-streaming mid-stream drop surfaces as a gateway error, not a 200 with partial output"
    );
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        0,
        "no HTTP fallback POST is made once the turn has committed to the websocket"
    );

    let _ = std::fs::remove_file(auth_path);
}

/// A delegated (Task child) turn must pool its websocket under the CHILD's
/// identity and the parent's next turn must open its own: per-turn identity
/// rides the handshake, so one shared pool key would send the parent's turn
/// under the child's `thread-id` and subagent markers (or, once the child's
/// socket expires, every later parent turn under them). Two turns — the child
/// then the parent — must produce two handshakes, the child's carrying the
/// `{session}::{agent}` identities.
#[tokio::test]
async fn delegated_turn_pools_its_own_websocket_and_the_parent_never_rides_it() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);
    let auth_path = write_fake_codex_auth(&mut vars);

    let (base_url, accepts, handshakes, server) = spawn_handshake_recording_ws_upstream(2).await;
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let body = serde_json::json!({
        "model": "codex-fallback-model",
        "max_tokens": 16,
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let (status, body) = post_messages(
        &gateway.base_url,
        &[
            ("x-claude-code-session-id", "sess-delegated-1"),
            ("x-claude-code-agent-id", "agent-7"),
            ("x-claude-code-agent-type", "Explore"),
        ],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "child turn failed: {body}");
    let (status, body) = post_messages(
        &gateway.base_url,
        &[("x-claude-code-session-id", "sess-delegated-1")],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "parent turn failed: {body}");

    assert_eq!(
        accepts.load(Ordering::SeqCst),
        2,
        "the parent turn must not ride the child's pooled websocket"
    );
    let handshakes = handshakes.lock().unwrap().clone();
    let child = &handshakes[0];
    assert_eq!(child["session-id"], "sess-delegated-1");
    assert_eq!(child["thread-id"], "sess-delegated-1::agent-7");
    assert_eq!(child["x-codex-parent-thread-id"], "sess-delegated-1");
    assert_eq!(child["x-openai-subagent"], "Explore");
    assert_eq!(child["x-client-request-id"], "sess-delegated-1::agent-7");
    assert_eq!(child["x-codex-window-id"], "sess-delegated-1::agent-7:0");
    let parent = &handshakes[1];
    assert_eq!(parent["session-id"], "sess-delegated-1");
    assert_eq!(parent["thread-id"], "sess-delegated-1");
    assert_eq!(parent["x-client-request-id"], "sess-delegated-1");
    assert_eq!(parent["x-codex-window-id"], "sess-delegated-1:0");
    assert!(
        parent.get("x-codex-parent-thread-id").is_none()
            && parent.get("x-openai-subagent").is_none(),
        "the parent's socket must not carry a child identity"
    );

    drop(gateway);
    server.abort();
    let _ = std::fs::remove_file(auth_path);
}

async fn post_messages(
    base_url: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, String) {
    let mut request = reqwest::Client::new()
        .post(format!("{base_url}/v1/messages"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.body(body.to_string()).send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    (status, text)
}

/// Two DISTINCT conversations whose identity text concatenates to the same
/// `:`-joined string — session `s::t` with no agent, and session `s` with
/// agent `t` — must not share a pooled websocket: one internal key per
/// (client, session, agent) triple, so the second turn opens its own socket
/// and handshakes under its own identity instead of riding the first
/// conversation's.
#[tokio::test]
async fn distinct_conversations_never_share_a_pool_key_even_when_their_text_concatenates() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);
    let auth_path = write_fake_codex_auth(&mut vars);

    let (base_url, accepts, handshakes, server) = spawn_handshake_recording_ws_upstream(2).await;
    let gateway = start_gateway_with(codex_ws_config(base_url)).await;

    let body = serde_json::json!({
        "model": "codex-fallback-model",
        "max_tokens": 16,
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let (status, body) = post_messages(
        &gateway.base_url,
        &[("x-claude-code-session-id", "s::t")],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first conversation failed: {body}");
    let (status, body) = post_messages(
        &gateway.base_url,
        &[
            ("x-claude-code-session-id", "s"),
            ("x-claude-code-agent-id", "t"),
        ],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second conversation failed: {body}");

    assert_eq!(
        accepts.load(Ordering::SeqCst),
        2,
        "the two conversations must pool under distinct keys"
    );
    let handshakes = handshakes.lock().unwrap().clone();
    let first = &handshakes[0];
    assert_eq!(first["session-id"], "s::t");
    assert_eq!(first["thread-id"], "s::t");
    assert_eq!(first["x-client-request-id"], "s::t");
    assert_eq!(first["x-codex-window-id"], "s::t:0");
    assert!(
        first.get("x-codex-parent-thread-id").is_none() && first.get("x-openai-subagent").is_none(),
        "the first conversation is not delegated"
    );
    let second = &handshakes[1];
    assert_eq!(second["session-id"], "s");
    assert_eq!(second["thread-id"], "s::t");
    assert_eq!(second["x-codex-parent-thread-id"], "s");
    assert_eq!(second["x-openai-subagent"], "subagent");
    assert_eq!(second["x-client-request-id"], "s::t");
    assert_eq!(second["x-codex-window-id"], "s::t:0");

    drop(gateway);
    server.abort();
    let _ = std::fs::remove_file(auth_path);
}

/// The bump's sweep must match pool keys by identity, not by text suffix: a
/// conversation whose crafted session/agent text embeds another conversation's
/// window key (`acct-0::<x>::::<s>` ends with `::<s>`) must survive that
/// conversation's compaction bump. Three turns: B pools its socket, A's
/// marked turn bumps, B's next turn must still ride B's own socket (2
/// handshakes total, not 3).
#[tokio::test]
async fn a_bump_does_not_evict_a_socket_whose_key_text_ends_with_the_window_key() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);

    let token = fake_jwt(4_000_000_000);
    vars.set("SHUNT_POOL_N1_TOKEN", &token);
    let (base_url, accepts, handshakes, server) = spawn_handshake_recording_ws_upstream(3).await;
    let gateway = start_gateway_with(pooled_codex_ws_config(
        base_url,
        ["SHUNT_POOL_N1_TOKEN", "SHUNT_POOL_N1_NEVER_SET"],
    ))
    .await;

    let body = serde_json::json!({
        "model": "codex-fallback-model",
        "max_tokens": 16,
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let b_headers = [
        ("x-claude-code-session-id", "x"),
        ("x-claude-code-agent-id", "::s"),
    ];
    // Turn 1: conversation B (session `x`, agent `::s`) pools its socket.
    let (status, body) = post_messages(&gateway.base_url, &b_headers, &body).await;
    assert_eq!(status, StatusCode::OK, "B's first turn failed: {body}");
    // Turn 2: conversation A (session `s`) carries the compaction mark.
    let (status, body) = post_messages(
        &gateway.base_url,
        &[
            ("x-claude-code-session-id", "s"),
            ("x-claude-code-context-compacted", "auto"),
        ],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "A's marked turn failed: {body}");
    // Turn 3: B again — its socket must have survived A's bump.
    let (status, body) = post_messages(&gateway.base_url, &b_headers, &body).await;
    assert_eq!(status, StatusCode::OK, "B's second turn failed: {body}");

    assert_eq!(
        accepts.load(Ordering::SeqCst),
        2,
        "A's bump must not evict B's pooled socket"
    );
    let handshakes = handshakes.lock().unwrap().clone();
    assert_eq!(handshakes[0]["thread-id"], "x::::s", "B's own thread id");
    assert_eq!(handshakes[1]["thread-id"], "s", "A's own thread id");

    drop(gateway);
    server.abort();
}

/// An HTTP turn carries the same `{session}:{window}` a websocket handshake
/// would: the compaction mark bumps the counter on the first dispatch that
/// reaches an upstream on EITHER transport, and a later turn of the same
/// conversation reads the advanced window without bumping. The mock records
/// each turn's `x-codex-window-id` header; a marked turn must send `:1` and
/// the turn after it `:1` again.
#[tokio::test]
async fn an_http_turn_carries_the_advanced_window_id() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let _accounts_dir = pin_empty_accounts_dir(&mut vars);
    let auth_path = write_fake_codex_auth(&mut vars);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSES_SSE))
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    config.providers.get_mut("codex").unwrap().base_url = upstream.uri();
    config.routes.push(RouteConfig {
        model: "codex-fallback-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    let gateway = start_gateway_with(config).await;

    let body = serde_json::json!({
        "model": "codex-fallback-model",
        "max_tokens": 16,
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (status, body) = post_messages(
        &gateway.base_url,
        &[
            ("x-claude-code-session-id", "sess-http-window-1"),
            ("x-claude-code-context-compacted", "auto"),
        ],
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "marked turn failed: {body}");
    let (status, body) = post_messages(
        &gateway.base_url,
        &[("x-claude-code-session-id", "sess-http-window-1")],
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second turn failed: {body}");

    let requests = upstream
        .received_requests()
        .await
        .expect("mock records requests");
    assert_eq!(requests.len(), 2, "one HTTP turn each");
    let window = |request: &wiremock::Request| {
        request
            .headers
            .get("x-codex-window-id")
            .and_then(|value| value.to_str().ok())
            .expect("the turn sends the window id")
            .to_string()
    };
    assert_eq!(
        window(&requests[0]),
        "sess-http-window-1:1",
        "the marked HTTP turn carries the advanced window"
    );
    assert_eq!(
        window(&requests[1]),
        "sess-http-window-1:1",
        "the next turn reads the advanced window without bumping"
    );

    let _ = std::fs::remove_file(auth_path);
}
