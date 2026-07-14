//! Codex WebSocket v2 transport (issue #32) — HTTP fallback safety net.
//!
//! Enabling `websocket = true` must never do worse than plain HTTP: when the
//! websocket cannot be established, the turn is transparently re-driven over the
//! HTTP Responses path. Here the upstream is a plain HTTP mock that has no
//! websocket endpoint, so the handshake fails and the request must still succeed
//! over HTTP.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use shunt::{
    config::{Config, RouteConfig},
    server,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

/// Serializes tests that mutate the process-global `CODEX_AUTH_FILE` env var.
/// Held across each test body so one test's teardown (`remove_var`) can never
/// unset the auth file while another test's request is still resolving the
/// credential.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
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

/// A minimal unsigned JWT (`x.<payload>.y`) with a far-future `exp`, so the codex
/// auth store treats it as valid without any network refresh.
fn fake_jwt(exp: u64) -> String {
    let payload = serde_json::json!({ "exp": exp });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// Write a codex-style `auth.json` a valid ChatGPT credential can be read from,
/// and point `CODEX_AUTH_FILE` at it. Returns the path for cleanup.
fn write_fake_codex_auth() -> PathBuf {
    let unique_name = format!(
        "shunt-ws-fallback-auth-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique_name);
    let auth = serde_json::json!({
        "tokens": {
            "access_token": fake_jwt(4_000_000_000),
            "refresh_token": "refresh-xyz",
            "account_id": "acct_fallback"
        }
    });
    std::fs::write(&path, serde_json::to_vec(&auth).unwrap()).unwrap();
    std::env::set_var("CODEX_AUTH_FILE", &path);
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
    let _env = ENV_LOCK.lock().await;

    // Upstream speaks only HTTP: it serves the Responses POST but has no websocket
    // endpoint, so the codex ws handshake (a GET upgrade) 404s and must fall back.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSES_SSE))
        .mount(&upstream)
        .await;

    let auth_path = write_fake_codex_auth();

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

    std::env::remove_var("CODEX_AUTH_FILE");
    let _ = std::fs::remove_file(auth_path);
}

#[tokio::test]
async fn streaming_ws_fallback_still_seeds_message_start_estimate() {
    if !can_bind_loopback() {
        return;
    }
    let _env = ENV_LOCK.lock().await;

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

    let auth_path = write_fake_codex_auth();

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

    std::env::remove_var("CODEX_AUTH_FILE");
    let _ = std::fs::remove_file(auth_path);
}

/// When the mock websocket drops the socket: before it has emitted any event
/// (nothing has reached the client, so the turn is safely re-driven over HTTP),
/// or after a first event (streaming has begun, so a restart would duplicate
/// output — the drop must surface as a clean error instead).
#[derive(Clone, Copy)]
enum WsDrop {
    BeforeFirstEvent,
    AfterFirstEvent,
}

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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let http_hits = Arc::new(AtomicUsize::new(0));
    let hits = http_hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            if request_is_websocket(&socket).await {
                tokio::spawn(serve_ws(socket, drop));
            } else {
                hits.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve_http(socket));
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
    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
        return;
    };
    let _ = ws.next().await; // the client's response.create frame
    if let WsDrop::AfterFirstEvent = drop {
        for event in [
            r#"{"type":"response.created","response":{"id":"resp_ws"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","delta":"partial over websocket"}"#,
        ] {
            // Surface a send failure loudly rather than swallowing it: a dropped
            // event would silently break the "partial over websocket" assertions
            // and make the AfterFirstEvent tests non-deterministic.
            ws.send(Message::Text(event.to_string()))
                .await
                .expect("mock upstream should stream the event before dropping");
        }
    }
    // Dropping `ws` closes the socket before a terminal event.
}

/// Answer an HTTP Responses `POST` with [`RESPONSES_SSE`]. Reads the full request
/// first so the client's write completes before the reply (a `content-length`
/// body lets the client finish reading before the socket closes).
async fn serve_http(mut socket: TcpStream) {
    drain_http_request(&mut socket).await;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
        RESPONSES_SSE.len(),
        RESPONSES_SSE
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
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
    let _env = ENV_LOCK.lock().await;

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::BeforeFirstEvent).await;
    let auth_path = write_fake_codex_auth();
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

    std::env::remove_var("CODEX_AUTH_FILE");
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
    let _env = ENV_LOCK.lock().await;

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::AfterFirstEvent).await;
    let auth_path = write_fake_codex_auth();
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

    std::env::remove_var("CODEX_AUTH_FILE");
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
    let _env = ENV_LOCK.lock().await;

    let (base_url, http_hits) = spawn_dual_upstream(WsDrop::AfterFirstEvent).await;
    let auth_path = write_fake_codex_auth();
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

    std::env::remove_var("CODEX_AUTH_FILE");
    let _ = std::fs::remove_file(auth_path);
}
