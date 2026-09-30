//! Emulated Anthropic `stop_sequences` on the Responses path (issue #605).
//!
//! The OpenAI Responses API has no `stop` parameter, so shunt truncates the turn
//! itself in the Responses→Anthropic SSE translation. This drives the whole
//! gateway against an upstream that *keeps streaming* after the stop string, and
//! asserts both halves of the contract: the client sees the text up to the stop
//! and nothing after it, and the upstream connection is aborted rather than left
//! generating.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use shunt::{
    config::{Config, RouteConfig},
    server,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

mod common;

/// The stop sequence Claude Code's auto-mode permission classifier sends.
const STOP: &str = "</block>";

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway(config: Config) -> TestGateway {
    let mut config = config;
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = TcpListener::bind(config.server.bind_addr().unwrap())
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

/// A hand-rolled Responses upstream that answers with `text/event-stream`, emits
/// the stop sequence mid-text, and then *keeps writing* deltas until the socket
/// is torn down. Returns its base URL and the flag it sets once a write fails —
/// i.e. once shunt dropped the connection.
///
/// A mock-server fixture cannot express this: it serves a finite body and the
/// abort has nothing to fail against. The write loop is bounded by a generous
/// deadline, which is the cost the *green* path pays (it exits on the first
/// failed write), not a window a racing failure has to beat.
async fn spawn_streaming_upstream() -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let aborted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&aborted);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        // Read whatever of the request arrives; the body is irrelevant here.
        let mut scratch = [0_u8; 4096];
        let _ = socket.read(&mut scratch).await;

        let head = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n",
            "cache-control: no-cache\r\n",
            "connection: close\r\n",
            "\r\n",
        );
        let opening = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"answer</block>garbage\"}\n\n",
        );
        if socket.write_all(head.as_bytes()).await.is_err()
            || socket.write_all(opening.as_bytes()).await.is_err()
            || socket.flush().await.is_err()
        {
            return;
        }

        // Keep generating past the stop. A live upstream would; the point of the
        // abort is that shunt stops paying for it.
        let more = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"still talking\"}\n\n",
        );
        for _ in 0..200 {
            if socket.write_all(more.as_bytes()).await.is_err() || socket.flush().await.is_err() {
                flag.store(true, Ordering::SeqCst);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    (format!("http://{addr}"), aborted)
}

/// The counterpart upstream: no stop string in its text, a real
/// `response.completed`, and then a short tail of further events before it ends
/// the body itself. Returns its base URL, the flag it sets if a write fails —
/// i.e. if shunt tore the connection down — and the flag it sets once it reached
/// that natural end instead.
///
/// The tail is what makes the two outcomes distinguishable: without bytes after
/// the terminal there is nothing for an abort to fail against. Its length is the
/// cost the *green* path pays, so it is short.
async fn spawn_completing_upstream() -> (String, Arc<AtomicBool>, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let aborted = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let abort_flag = Arc::clone(&aborted);
    let finish_flag = Arc::clone(&finished);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut scratch = [0_u8; 4096];
        let _ = socket.read(&mut scratch).await;

        let head = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n",
            "cache-control: no-cache\r\n",
            "connection: close\r\n",
            "\r\n",
        );
        let turn = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"answer\"}\n\n",
            "event: response.output_text.done\n",
            "data: {}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        );
        if socket.write_all(head.as_bytes()).await.is_err()
            || socket.write_all(turn.as_bytes()).await.is_err()
            || socket.flush().await.is_err()
        {
            return;
        }

        // Trailing SSE comments: the translation ignores them, so they only ever
        // exercise the transport's read side.
        for _ in 0..20 {
            if socket.write_all(b": keep-alive\n\n").await.is_err() || socket.flush().await.is_err()
            {
                abort_flag.store(true, Ordering::SeqCst);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = socket.shutdown().await;
        finish_flag.store(true, Ordering::SeqCst);
    });

    (format!("http://{addr}"), aborted, finished)
}

/// Collect the `text` of every `text_delta` frame in an Anthropic SSE stream.
fn streamed_text(sse: &str) -> String {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter(|value| value["delta"]["type"] == "text_delta")
        .filter_map(|value| value["delta"]["text"].as_str().map(str::to_string))
        .collect()
}

/// The last non-empty `event:` name in an Anthropic SSE stream.
fn last_event(sse: &str) -> Option<String> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .next_back()
        .map(str::to_string)
}

#[tokio::test]
async fn responses_stop_sequence_truncates_the_client_stream_and_aborts_the_upstream() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_STOP_SEQUENCES_KEY", "sk-test");

    let (upstream_url, upstream_aborted) = spawn_streaming_upstream().await;

    let mut config = Config::default();
    {
        let openai = config.providers.get_mut("openai").unwrap();
        openai.base_url = upstream_url;
        openai.api_key_env = Some("SHUNT_TEST_STOP_SEQUENCES_KEY".to_string());
    }
    config.routes.push(RouteConfig {
        model: "stop-sequence-model".to_string(),
        provider: "openai".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    let gateway = start_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "stop-sequence-model",
                "max_tokens": 64,
                "stream": true,
                "stop_sequences": [STOP],
                "messages": [{"role": "user", "content": "classify"}]
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let sse = tokio::time::timeout(Duration::from_secs(15), response.text())
        .await
        .expect("the client stream must end at the stop, not at the upstream's")
        .unwrap();

    assert_eq!(
        streamed_text(&sse),
        "answer",
        "only the text before the stop reaches the client; got: {sse}"
    );
    assert!(
        !sse.contains("garbage") && !sse.contains("still talking"),
        "post-stop text leaked to the client: {sse}"
    );
    assert!(
        sse.contains("\"stop_reason\":\"stop_sequence\""),
        "got: {sse}"
    );
    assert!(sse.contains("\"stop_sequence\":\"</block>\""), "got: {sse}");
    assert_eq!(
        last_event(&sse).as_deref(),
        Some("message_stop"),
        "message_stop must be the last event; got: {sse}"
    );

    // The upstream's next write fails once shunt drops the connection. Poll for
    // it rather than sleeping a fixed amount: the deadline is generous because
    // it is only ever paid in full by a *failing* run.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !upstream_aborted.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        upstream_aborted.load(Ordering::SeqCst),
        "shunt must drop the upstream connection at the stop instead of letting it keep generating"
    );
}

/// The positive twin of the abort test: with no `stop_sequences` configured, a
/// turn that ends on a real `response.completed` must *not* have its upstream
/// torn down. Keying the abort on "the machine is stopped" rather than on the
/// emulated stop would drop the byte stream on every ordinary terminal, and a
/// reqwest body that never reaches EOF costs the connection its place in the
/// idle pool — on the overwhelming majority of turns, since `stop_sequences` is
/// usually unset.
#[tokio::test]
async fn responses_completed_turn_without_stop_sequences_keeps_the_upstream_connection() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_STOP_SEQUENCES_KEY", "sk-test");

    let (upstream_url, upstream_aborted, upstream_finished) = spawn_completing_upstream().await;

    let mut config = Config::default();
    {
        let openai = config.providers.get_mut("openai").unwrap();
        openai.base_url = upstream_url;
        openai.api_key_env = Some("SHUNT_TEST_STOP_SEQUENCES_KEY".to_string());
    }
    config.routes.push(RouteConfig {
        model: "stop-sequence-model".to_string(),
        provider: "openai".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    let gateway = start_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "stop-sequence-model",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "classify"}]
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // The client stream ends at the terminal frame rather than waiting on the
    // upstream drain (`translated_core` ends the outward stream immediately and
    // hands the remaining bytes to a detached `spawn_terminal_drain`, bounded by
    // `TERMINAL_DRAIN_BUDGET`, purely so the connection can still be pooled — see
    // its own `a_keepalive_ping_never_follows_the_terminal_frame` test). So
    // `response.text()` returning here proves nothing about the upstream socket
    // yet; poll for it to reach its own natural end within that same budget
    // instead of asserting on it synchronously.
    let sse = tokio::time::timeout(Duration::from_secs(15), response.text())
        .await
        .expect("the client stream must end at the terminal frame")
        .unwrap();

    assert_eq!(streamed_text(&sse), "answer", "got: {sse}");
    assert!(sse.contains("\"stop_reason\":\"end_turn\""), "got: {sse}");
    assert_eq!(
        last_event(&sse).as_deref(),
        Some("message_stop"),
        "message_stop must be the last event; got: {sse}"
    );

    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        while !upstream_finished.load(Ordering::SeqCst) && !upstream_aborted.load(Ordering::SeqCst)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "the detached drain must resolve the upstream to a terminal state within its budget"
    );
    assert!(
        !upstream_aborted.load(Ordering::SeqCst),
        "an ordinary terminal must not abort the upstream: the body has to drain to EOF for the \
         connection to stay poolable"
    );
    assert!(
        upstream_finished.load(Ordering::SeqCst),
        "the upstream must have reached its own end of body"
    );
}
