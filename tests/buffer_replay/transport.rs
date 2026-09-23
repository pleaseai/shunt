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
    anthropic_json, anthropic_sse, escalation_router, gated_config, gated_config_with_gemini,
    header, judge_text, messages_mock, post, sse_events, sse_reply, streamed_text, Tiers,
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
    let mut reply =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\n\r\n"
            .to_vec();
    reply.extend_from_slice(anthropic_sse(EFFICIENT_UPSTREAM_MODEL, "WEAK-ANSWER").as_bytes());
    reply.extend_from_slice(tail);
    raw_upstream(reply, hold_open).await
}

/// A one-shot upstream that reads the request, writes `reply` verbatim, then
/// closes — or, with `hold_open`, waits for the gateway to drop the connection.
async fn raw_upstream(reply: Vec<u8>, hold_open: bool) -> (String, tokio::task::JoinHandle<()>) {
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
        socket.write_all(&reply).await.unwrap();
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

/// A non-streaming weak turn whose upstream sent its `200` headers and then
/// broke its body mid-read: the Anthropic adapter reads an alias route's JSON
/// reply whole, so the break surfaces as an adapter error rather than as a
/// short body. That turn ended before it was complete — it is cut, and
/// escalation falls back to the strong tier instead of refusing the request.
///
/// Non-vacuity: drop the `body_broke` check in `chain_failure` and the chain
/// error is reported as the upstream's own failure, so the request is refused
/// with a `502` and the strong mock's `expect(1)` goes red.
#[tokio::test]
async fn a_non_streaming_weak_body_broken_after_its_headers_falls_back_to_the_strong_tier() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, responses, judge) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(anthropic_json(CAPABLE_UPSTREAM_MODEL, "STRONG"), 1)
        .mount(&strong)
        .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let mut reply =
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\n\r\n"
            .to_vec();
    reply.extend_from_slice(b"{\"id\":\"msg_cut\",\"type\":\"message\",\"content\":[{\"type\":\"text\",\"text\":\"WEAK-PARTIAL");
    let (weak, responder) = raw_upstream(reply, false).await;
    let tiers = Tiers {
        strong: strong.uri(),
        weak,
        responses: responses.uri(),
        judge: judge.uri(),
    };
    let gateway = start_gateway(gated_config(&tiers, &escalation_router("efficient-alias"))).await;

    let response = post(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_fallback"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "STRONG", "got: {body}");
    responder.await.unwrap();
}

/// The idle gap and the wall-clock bound the stalled-body tests below run
/// under: far enough apart that a turn cut at the idle gap is plainly told from
/// one cut at the duration bound, which also falls back to the strong tier.
const STALL_ROUTER_EXTRA: &str = "gated_idle_ms = 300\ngated_max_duration_ms = 8000\n";

/// A non-streaming weak turn whose upstream sent its `200` headers and part of
/// its JSON body, then held the connection open: the Anthropic adapter reads an
/// alias route's reply whole before `run_chain` returns, so the stall is inside
/// that read. It is cut at `gated_idle_ms`, and escalation falls back to the
/// strong tier — well before `gated_max_duration_ms`.
///
/// Non-vacuity: pass no idle gap in `capture`'s `ResponseBounds` and the stall
/// is cut only at the duration bound — the turn still falls back, but the
/// elapsed-time assertion goes red.
#[tokio::test]
async fn a_non_streaming_weak_body_stalled_after_its_headers_is_cut_at_the_idle_gap() {
    stalled_non_streaming_weak_turn_falls_back(
        StalledTier::Anthropic,
        b"HTTP/1.1 200 OK\r\n",
        b"{\"id\":\"msg_stall\",\"type\":\"message\",\"content\":[{\"type\":\"text\",\"text\":\"WEAK-PARTIAL",
    )
    .await;
}

/// The refusal twin: a non-streaming weak `400` whose error body stalls after
/// its headers. The alias rewrite reads a refusal whole too, and `400` is a
/// status the chain relays rather than advances on, so the stall is again
/// inside the adapter's read — and is cut at the idle gap like a success.
///
/// Non-vacuity: pass no idle gap in `capture`'s `ResponseBounds` and the
/// refusal is cut only at the duration bound, so the elapsed-time assertion
/// goes red.
#[tokio::test]
async fn a_non_streaming_weak_refusal_stalled_after_its_headers_is_cut_at_the_idle_gap() {
    stalled_non_streaming_weak_turn_falls_back(
        StalledTier::Anthropic,
        b"HTTP/1.1 400 Bad Request\r\n",
        b"{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"WEAK-",
    )
    .await;
}

/// A non-streaming Responses weak turn whose upstream sent its `200` SSE
/// headers and the start of the turn, then held the connection open. The
/// Responses adapter asks for SSE even for a `stream: false` caller and reads
/// that body whole in `json_response` before `run_chain` returns, so the stall
/// is inside the adapter's read — and is cut at the idle gap.
///
/// Non-vacuity: pass `None` for the idle gap to `collect_upstream_body` in
/// `json_response` and the stall is cut only at the duration bound, so the
/// elapsed-time assertion goes red.
#[tokio::test]
async fn a_non_streaming_responses_weak_body_stalled_after_its_headers_is_cut_at_the_idle_gap() {
    stalled_non_streaming_weak_turn_falls_back(
        StalledTier::Responses,
        b"HTTP/1.1 200 OK\r\n",
        concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_stall\",\"usage\":{\"output_tokens\":0}}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"WEAK-PAR",
        )
        .as_bytes(),
    )
    .await;
}

/// A non-streaming Gemini weak turn whose upstream sent its `200` headers and
/// part of its `generateContent` JSON, then held the connection open: the
/// Gemini adapter reads a non-streaming reply whole to translate it, so the
/// stall is inside that read — and is cut at the idle gap.
///
/// Non-vacuity: pass `None` for the idle gap to the success-body
/// `collect_upstream_body` in `gemini::forward_single` and the stall is cut
/// only at the duration bound, so the elapsed-time assertion goes red.
#[tokio::test]
async fn a_non_streaming_gemini_weak_body_stalled_after_its_headers_is_cut_at_the_idle_gap() {
    stalled_non_streaming_weak_turn_falls_back(
        StalledTier::Gemini,
        b"HTTP/1.1 200 OK\r\n",
        b"{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"WEAK-PAR",
    )
    .await;
}

/// The Gemini refusal twin: a `400` whose error body stalls after its headers.
/// The adapter reads an error body whole before `map_gemini_error` renders it,
/// and a `400` is not retried, so the stall is inside that read — and is cut
/// at the idle gap like a success.
///
/// Non-vacuity: pass `None` for the idle gap to the error-body
/// `collect_upstream_body` in `gemini::forward_single` and the refusal is cut
/// only at the duration bound, so the elapsed-time assertion goes red.
#[tokio::test]
async fn a_non_streaming_gemini_weak_refusal_stalled_after_its_headers_is_cut_at_the_idle_gap() {
    stalled_non_streaming_weak_turn_falls_back(
        StalledTier::Gemini,
        b"HTTP/1.1 400 Bad Request\r\n",
        b"{\"error\":{\"code\":400,\"status\":\"INVALID_ARGUMENT\",\"message\":\"WEAK-",
    )
    .await;
}

/// Which weak tier the stalled raw socket stands behind.
#[derive(Clone, Copy)]
enum StalledTier {
    Anthropic,
    Responses,
    Gemini,
}

impl StalledTier {
    fn weak_target(self) -> &'static str {
        match self {
            Self::Anthropic => "efficient-alias",
            Self::Responses => "responses-alias",
            Self::Gemini => "gemini-alias",
        }
    }

    fn content_type(self) -> &'static [u8] {
        match self {
            Self::Responses => b"text/event-stream",
            Self::Anthropic | Self::Gemini => b"application/json",
        }
    }
}

async fn stalled_non_streaming_weak_turn_falls_back(
    tier: StalledTier,
    status_line: &[u8],
    partial: &[u8],
) {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, unused_tier, judge) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(anthropic_json(CAPABLE_UPSTREAM_MODEL, "STRONG"), 1)
        .mount(&strong)
        .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let mut reply = status_line.to_vec();
    reply.extend_from_slice(b"content-type: ");
    reply.extend_from_slice(tier.content_type());
    reply.extend_from_slice(b"\r\ncontent-length: 4096\r\n\r\n");
    reply.extend_from_slice(partial);
    let (stalled, responder) = raw_upstream(reply, true).await;
    // The tiers the stalled socket does not stand behind get a mock that is
    // never called.
    let tiers = Tiers {
        strong: strong.uri(),
        weak: match tier {
            StalledTier::Anthropic => stalled.clone(),
            _ => unused_tier.uri(),
        },
        responses: match tier {
            StalledTier::Responses => stalled.clone(),
            _ => unused_tier.uri(),
        },
        judge: judge.uri(),
    };
    let router = format!(
        "{}{STALL_ROUTER_EXTRA}",
        escalation_router(tier.weak_target())
    );
    let config = match tier {
        StalledTier::Gemini => gated_config_with_gemini(&tiers, &router, stalled),
        _ => gated_config(&tiers, &router),
    };
    let gateway = start_gateway(config).await;

    let started = std::time::Instant::now();
    let response = post(&gateway, false).await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_fallback"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "STRONG", "got: {body}");
    assert!(
        elapsed < Duration::from_millis(4000),
        "cut after {elapsed:?}: the stall ran on toward the 8000 ms duration bound"
    );
    responder.await.unwrap();
}
