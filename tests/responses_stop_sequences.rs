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
