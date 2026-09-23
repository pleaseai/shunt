//! What follows the gated turn's terminal marker — a break, a keep-alive, a
//! connection held open — is not part of the turn: it is retained and replayed
//! exactly through `message_stop`, the positive twin of the cut in
//! `a_weak_turn_cut_before_message_stop_falls_back_to_the_strong_tier`.

use std::time::Duration;

use reqwest::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer};

use super::harness::{
    anthropic_json, anthropic_sse, escalation_router, gated_config, header, judge_text,
    messages_mock, post, sse_events, sse_reply, streamed_text, Tiers,
};
use super::judge_harness::{
    can_bind_loopback, env, start_gateway, CAPABLE_UPSTREAM_MODEL, EFFICIENT_UPSTREAM_MODEL,
};
use super::DECLINE;

/// A one-shot weak upstream that reads the request, writes the complete turn
/// followed by `tail`, then either closes short of its declared content-length
/// — so the body read fails after `message_stop` — or, with `hold_open`, keeps
/// the connection open until the gateway drops it. wiremock cannot express
/// either: its server refuses a length its body disagrees with.
async fn after_the_turn_upstream(
    tail: &'static [u8],
    hold_open: bool,
) -> (String, tokio::task::JoinHandle<()>) {
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
        socket.write_all(tail).await.unwrap();
        if hold_open {
            // Bounded, so a gateway that never lets go fails the test on its
            // assertions rather than hanging it here.
            let _ = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut chunk)).await;
        }
    });
    (url, responder)
}

/// The weak turn reached `message_stop` before its connection broke: the judge
/// is asked, declines, and the retained turn is replayed whole — no strong
/// call, no `error` event, and none of the partial frame that followed.
///
/// Non-vacuity: read on past the marker in `retain_stream` and cut on the
/// break it reaches, and the header comes back `escalation_fallback` and the
/// strong mock's `expect(0)` goes red.
#[tokio::test]
async fn a_transport_break_after_message_stop_still_replays_the_weak_turn() {
    replays_the_weak_turn_through_message_stop(b"event: ping\ndata: {\"ty", false, "").await;
}

/// The weak turn reached `message_stop` and the upstream then sent a
/// keep-alive and held the connection open: the turn is finished at the
/// marker, so it is replayed at once — not held until the idle bound cuts it
/// and escalation bills the strong tier — and without the ping that followed.
///
/// Non-vacuity: drain to end of stream in `retain_stream` and the idle bound
/// cuts the turn — the header comes back `escalation_fallback` and the strong
/// mock's `expect(0)` goes red; cut the retained bytes anywhere but at the
/// marker's frame and the replay ends on the ping, so the last-event
/// assertion goes red.
#[tokio::test]
async fn a_connection_held_open_after_message_stop_still_replays_the_weak_turn() {
    replays_the_weak_turn_through_message_stop(
        b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
        true,
        "gated_idle_ms = 500\n",
    )
    .await;
}

async fn replays_the_weak_turn_through_message_stop(
    tail: &'static [u8],
    hold_open: bool,
    router_extra: &str,
) {
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
    let (weak, responder) = after_the_turn_upstream(tail, hold_open).await;
    let tiers = Tiers {
        strong: strong.uri(),
        weak,
        responses: responses.uri(),
        judge: judge.uri(),
    };
    let router = format!("{}{router_extra}", escalation_router("efficient-alias"));
    let gateway = start_gateway(gated_config(&tiers, &router)).await;

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

/// A Responses turn whose upstream ended before `response.completed`.
const TRUNCATED_RESPONSES: &str = concat!(
    "event: response.created\n",
    "data: {\"response\":{\"id\":\"resp_cut\",\"usage\":{\"output_tokens\":0}}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"delta\":\"WEAK-PARTIAL\"}\n\n",
);

/// A Responses weak turn whose upstream ended before `response.completed`:
/// the adapter still closes the client's stream with a synthesized
/// `message_stop`, behind the upstream-truncation marker. The turn is cut, so
/// escalation falls back to the strong tier and the partial text never
/// reaches the caller.
///
/// Non-vacuity: accept the synthesized `message_stop` (drop the `truncated`
/// check in `TerminalScan::is_terminal`) and the judge is asked and declines,
/// so the header comes back `escalation_weak` and the mocks' `expect` counts
/// go red.
#[tokio::test]
async fn a_truncated_responses_weak_turn_falls_back_to_the_strong_tier() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, efficient, responses, judge) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(
        sse_reply(anthropic_sse(CAPABLE_UPSTREAM_MODEL, "STRONG")),
        1,
    )
    .mount(&strong)
    .await;
    Mock::given(method("POST"))
        .respond_with(sse_reply(TRUNCATED_RESPONSES.to_string()))
        .expect(1)
        .mount(&responses)
        .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let tiers = Tiers {
        strong: strong.uri(),
        weak: efficient.uri(),
        responses: responses.uri(),
        judge: judge.uri(),
    };
    let gateway = start_gateway(gated_config(&tiers, &escalation_router("responses-alias"))).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_fallback"
    );
    let body = response.text().await.unwrap();
    assert!(
        !body.contains("WEAK-PARTIAL"),
        "the cut turn leaked: {body}"
    );
    assert_eq!(streamed_text(&sse_events(&body)), "STRONG");
}

/// The non-streaming twin: a Responses target asks its upstream for SSE even
/// for a `stream: false` caller, and synthesizes one message from whatever
/// arrived. That message parses like a finished one, so the adapter marks it,
/// and the gated capture cuts it rather than judging and replaying it.
///
/// Non-vacuity: drop the `UpstreamTruncated` check in `retain_message` and the
/// judge is asked and declines, so the header comes back `escalation_weak`
/// and the mocks' `expect` counts go red.
#[tokio::test]
async fn a_truncated_non_streaming_responses_weak_turn_falls_back_to_the_strong_tier() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, efficient, responses, judge) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(anthropic_json(CAPABLE_UPSTREAM_MODEL, "STRONG"), 1)
        .mount(&strong)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse_reply(TRUNCATED_RESPONSES.to_string()))
        .expect(1)
        .mount(&responses)
        .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let tiers = Tiers {
        strong: strong.uri(),
        weak: efficient.uri(),
        responses: responses.uri(),
        judge: judge.uri(),
    };
    let gateway = start_gateway(gated_config(&tiers, &escalation_router("responses-alias"))).await;

    let response = post(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_fallback"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "STRONG", "got: {body}");
}
