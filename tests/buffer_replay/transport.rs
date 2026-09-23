//! A transport that breaks after the gated turn's terminal marker: the turn is
//! finished, so it is retained — the positive twin of the cut in
//! `a_weak_turn_cut_before_message_stop_falls_back_to_the_strong_tier`.

use reqwest::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::MockServer;

use super::harness::{
    anthropic_sse, escalation_router, gated_config, header, judge_text, messages_mock, post,
    sse_events, sse_reply, streamed_text, Tiers,
};
use super::judge_harness::{
    can_bind_loopback, env, start_gateway, CAPABLE_UPSTREAM_MODEL, EFFICIENT_UPSTREAM_MODEL,
};
use super::DECLINE;

/// A one-shot weak upstream that reads the request, writes the complete turn
/// and half of a following frame, then closes short of its declared
/// content-length — so the body read fails after `message_stop`. wiremock
/// cannot express this: its server refuses a length its body disagrees with.
async fn breaking_upstream() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let responder = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Read the whole request first: closing on unread bytes would reset
        // the connection before the response is read.
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the gateway closed before its request ended");
            request.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&request);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length = text[..head_end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length {
                    break;
                }
            }
        }
        let turn = anthropic_sse(EFFICIENT_UPSTREAM_MODEL, "WEAK-ANSWER");
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(turn.as_bytes()).await.unwrap();
        socket.write_all(b"event: ping\ndata: {\"ty").await.unwrap();
    });
    (url, responder)
}

/// The weak turn reached `message_stop` before its connection broke: the judge
/// is asked, declines, and the retained turn is replayed whole — no strong
/// call, no `error` event, and none of the partial frame that followed.
///
/// Non-vacuity: check the broken transport before the terminal marker in
/// `retain_stream` and the turn is cut — the header comes back
/// `escalation_fallback` and the strong mock's `expect(0)` goes red; keep the
/// partial frame and libsy's decode fails the turn, so the status goes red.
#[tokio::test]
async fn a_transport_break_after_message_stop_still_replays_the_weak_turn() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, responses, judge) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(
        sse_reply(anthropic_sse(CAPABLE_UPSTREAM_MODEL, "STRONG")),
        0,
    )
    .mount(&strong)
    .await;
    messages_mock(judge_text(DECLINE), 1).mount(&judge).await;
    let (weak, responder) = breaking_upstream().await;
    let tiers = Tiers {
        strong: strong.uri(),
        weak,
        responses: responses.uri(),
        judge: judge.uri(),
    };
    let gateway = start_gateway(gated_config(&tiers, &escalation_router("efficient-alias"))).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_weak"
    );
    let body = response.text().await.unwrap();
    let events = sse_events(&body);
    assert_eq!(
        events.last().map(|(event, _)| event.as_str()),
        Some("message_stop"),
        "got:\n{body}"
    );
    assert!(
        events.iter().all(|(event, _)| event != "error"),
        "got:\n{body}"
    );
    assert_eq!(streamed_text(&events), "WEAK-ANSWER");
    responder.await.unwrap();
}
