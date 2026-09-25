//! A gated escalation weak turn over the Codex WebSocket transport, stalled
//! with its socket open (#667).
//!
//! The Responses WebSocket collector builds a non-streaming reply from an
//! event channel before `run_chain` returns, so `routing::serve`'s own idle
//! timer has not started yet: without `gated_idle_ms` applied inside the
//! adapter, a backend that accepts the turn and then goes silent is held to the
//! transport's 300 s idle timeout, not the operator's gap. These pin that the
//! stall is cut at the gap — before the first event and between events — and
//! that escalation then falls back to the strong tier.

mod common;
mod judge_harness;

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::config::Config;
use tokio::{io::AsyncWriteExt, net::TcpListener};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use judge_harness::can_bind_loopback;

/// The escalation entry under test.
const GATED_MODEL: &str = "ws-gated";

/// Where the websocket turn stalls.
#[derive(Clone, Copy)]
enum Stall {
    /// Accept the turn's frame and send nothing at all.
    BeforeFirstEvent,
    /// Start the turn, then send nothing more.
    AfterFirstEvent,
}

/// A Codex upstream whose websocket accepts a turn and stalls as `stall`
/// says, holding the socket open. Any HTTP request — an HTTP fallback, which a
/// cut must not become — is refused and counted.
async fn spawn_stalling_upstream(stall: Stall) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let http_hits = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&http_hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            if !request_is_websocket(&socket).await {
                hits.fetch_add(1, Ordering::SeqCst);
                let _ = socket
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n")
                    .await;
                continue;
            }
            tokio::spawn(async move {
                // An explicit config, as `tests/codex_websocket_fallback.rs`
                // passes: it is what negotiates the permessage-deflate
                // extension the client offers.
                let Ok(mut ws) = tokio_tungstenite::accept_async_with_config(
                    socket,
                    Some(WebSocketConfig::default()),
                )
                .await
                else {
                    return;
                };
                let _ = ws.next().await; // the client's response.create frame
                if let Stall::AfterFirstEvent = stall {
                    for event in [
                        r#"{"type":"response.created","response":{"id":"resp_ws"}}"#,
                        r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
                        r#"{"type":"response.output_text.delta","delta":"WEAK-PARTIAL"}"#,
                    ] {
                        ws.send(Message::Text(event.to_string().into()))
                            .await
                            .expect("mock upstream should stream the turn's start");
                    }
                }
                // Silent, not closed: the socket stays open until the test ends.
                std::future::pending::<()>().await;
                drop(ws);
            });
        }
    });
    (format!("http://{addr}"), http_hits)
}

/// Peek the leading bytes to tell a websocket upgrade (`GET`) from an HTTP
/// `POST` without consuming them, waiting out a partial request line.
async fn request_is_websocket(socket: &tokio::net::TcpStream) -> bool {
    let mut head = [0u8; 4];
    loop {
        match socket.peek(&mut head).await {
            Ok(0) | Err(_) => return false,
            Ok(n) if n >= 4 => return &head == b"GET ",
            Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// A minimal unsigned JWT with a far-future `exp`, so the codex auth store
/// treats it as valid without a refresh.
fn fake_jwt() -> String {
    let payload = json!({
        "exp": 4_102_444_800u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct_gated"}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// Temp files and dirs this test writes, removed when it ends however it ends.
struct Scratch(Vec<std::path::PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Point the codex credential at a fake ChatGPT login and its account store at
/// an empty dir, through the caller's guard: a host's real codex accounts must
/// not leak into the turn.
fn fake_codex_login(vars: &mut common::EnvVars) -> Scratch {
    let unique = format!(
        "shunt-ws-gated-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let accounts = std::env::temp_dir().join(format!("{unique}-accounts"));
    std::fs::create_dir_all(&accounts).unwrap();
    let auth = std::env::temp_dir().join(format!("{unique}-auth.json"));
    std::fs::write(
        &auth,
        serde_json::to_vec(&json!({
            "tokens": {
                "access_token": fake_jwt(),
                "refresh_token": "refresh-xyz",
                "account_id": "acct_gated"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts);
    vars.set("CODEX_AUTH_FILE", &auth);
    Scratch(vec![accounts, auth])
}

/// An escalation entry whose weak tier is the websocket-enabled codex provider
/// at `codex` and whose strong tier and never-consulted judge answer at
/// `strong`, cut at a 300 ms idle gap well inside its 8 s duration bound.
fn gated_config(codex: String, strong: &str, scratch: &mut Scratch) -> Config {
    let path = std::env::temp_dir().join(format!(
        "shunt-ws-gated-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        format!(
            r#"
[providers.strong]
kind = "anthropic"
base_url = "{strong}"
auth = "passthrough"

[providers.judge]
kind = "anthropic"
base_url = "{strong}"
auth = "none"

[[models]]
id = "{GATED_MODEL}"
[models.router]
type = "llm_classifier"
mode = "escalation"
classifier_target = "judge-alias"
strong_target = "strong-alias"
weak_target = "ws-alias"
judge_timeout_ms = 300
gated_idle_ms = 300
gated_max_duration_ms = 8000

[[models]]
id = "ws-alias"
upstream_model = {{ codex = "gpt-5.2-codex" }}

[[models]]
id = "strong-alias"
upstream_model = {{ strong = "upstream-strong" }}

[[models]]
id = "judge-alias"
upstream_model = {{ judge = "upstream-judge" }}
"#
        ),
    )
    .unwrap();
    scratch.0.push(path.clone());
    let mut config = Config::load(Some(&path)).expect("the gated fixture config loads");
    let provider = config
        .providers
        .get_mut("codex")
        .expect("the built-in codex provider");
    provider.base_url = codex;
    provider.websocket = true;
    config.server.bind = "127.0.0.1:0".to_string();
    config
}

async fn stalled_weak_turn_falls_back_at_the_idle_gap(stall: Stall) {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let mut scratch = fake_codex_login(&mut vars);
    let strong = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_strong",
            "type": "message",
            "role": "assistant",
            "model": "upstream-strong",
            "content": [{"type": "text", "text": "STRONG"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 1},
        })))
        // Once: the strong fallback. A judge call would be a second.
        .expect(1)
        .mount(&strong)
        .await;
    let (codex, http_hits) = spawn_stalling_upstream(stall).await;
    let config = gated_config(codex, &strong.uri(), &mut scratch);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _, _) = shunt::server::build_router(config).unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(20),
        reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .json(&json!({
                "model": GATED_MODEL,
                "max_tokens": 64,
                "stream": false,
                "messages": [{ "role": "user", "content": "hi" }],
            }))
            .send(),
    )
    .await
    .expect("the gated turn finishes within the guard")
    .expect("the request reaches the gateway");
    let elapsed = started.elapsed();

    let status = response.status();
    // Kept past the body read, so a wrong status is reported with its body
    // before a missing header can panic.
    let headers = response.headers().clone();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(headers["x-gateway-route-source"], "escalation_fallback");
    assert_eq!(body["content"][0]["text"], "STRONG", "got: {body}");
    assert!(
        elapsed < Duration::from_millis(4000),
        "cut after {elapsed:?}: the stall ran on toward the 8000 ms duration bound"
    );
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        0,
        "an idle cut is the call's bound, not a reason to re-drive the turn over HTTP"
    );
    strong.verify().await;
    server.abort();
}

/// A backend that accepts the turn's frame and never sends a first event is
/// cut at the gap in `open_ws_turn`'s peek, rather than held to the
/// transport's idle timeout, and the cut is not an HTTP fallback.
///
/// Non-vacuity: pass `None` as the peek's idle gap in `forward_websocket` and
/// the peek waits out the 8 s duration bound, so the elapsed-time assertion
/// goes red.
#[tokio::test]
async fn a_gated_websocket_turn_silent_before_its_first_event_falls_back_at_the_idle_gap() {
    stalled_weak_turn_falls_back_at_the_idle_gap(Stall::BeforeFirstEvent).await;
}

/// A backend that starts the turn and then goes silent with its socket open
/// is cut at the gap in `json_events_response`.
///
/// Non-vacuity: pass `ResponseBounds { idle: None, .. }` to
/// `json_events_response` in `forward_websocket` and the collector waits out
/// the 8 s duration bound, so the elapsed-time assertion goes red.
#[tokio::test]
async fn a_gated_websocket_turn_silent_after_its_first_event_falls_back_at_the_idle_gap() {
    stalled_weak_turn_falls_back_at_the_idle_gap(Stall::AfterFirstEvent).await;
}
