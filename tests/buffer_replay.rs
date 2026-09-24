//! End-to-end coverage for the buffer-and-replay lane: `mode = "escalation"`
//! and `type = "advisor"` (ADR-0005 §3, §8 PR 6, issue #596).
//!
//! The driven lane of PR 5 never serves a model it called: the judge only
//! picks a tier and the caller's turn is dispatched live. Here the first call
//! *is* the caller's turn, run under the three gated bounds and retained whole
//! so libsy can inspect it before the client sees a byte. What these pin is the
//! contract of that retention: the replayed bytes are the adapter's own
//! rendering (the router id, the caller's stream mode, either adapter), a turn
//! that never reached its terminal marker is never served, and every outcome —
//! decline, redo, stall, cut — reaches the client as one documented
//! `x-gateway-route-source`.
//!
//! Non-vacuity, per test, is on the test itself. Across the file: delete the
//! gated hook in `proxy::failover::forward` and the seven definition-of-done
//! tests go red (observed), each on its status or route-source assertion: the
//! PR 5 drive has no retained turn to serve.

mod common;
mod judge_harness;

#[path = "buffer_replay/harness.rs"]
mod harness;

// The advisor half of the definition of done, split only for the 500-line
// ceiling.
#[path = "buffer_replay/advisor.rs"]
mod advisor;

#[path = "buffer_replay/transport.rs"]
mod transport;

#[path = "buffer_replay/pool.rs"]
mod pool;

use reqwest::StatusCode;
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer};

use harness::{
    anthropic_json, anthropic_sse, anthropic_sse_truncated, bodies, escalation_router,
    gated_config, header, judge_text, messages_mock, post, responses_sse, sse_events, sse_reply,
    streamed_text, Tiers,
};
use judge_harness::{
    can_bind_loopback, env, stall_mock, start_gateway, Stall, CAPABLE_UPSTREAM_MODEL,
    EFFICIENT_UPSTREAM_MODEL, JUDGE_KEY_ENV, ROUTER_ID,
};

pub(crate) const DECLINE: &str = r#"{"escalate": false, "reason": "progressing"}"#;

/// The weak tier the test runs on: the passthrough Anthropic adapter or the
/// injecting Responses one.
#[derive(Clone, Copy, Debug)]
enum Weak {
    Anthropic,
    Responses,
}

impl Weak {
    fn alias(self) -> &'static str {
        match self {
            Weak::Anthropic => "efficient-alias",
            Weak::Responses => "responses-alias",
        }
    }
}

/// The weak tier's mock, answering in the upstream shape for `stream`: a
/// Responses upstream always streams, so it has one shape.
async fn mount_weak(weak: Weak, stream: bool, efficient: &MockServer, responses: &MockServer) {
    match weak {
        Weak::Anthropic => {
            let reply = if stream {
                sse_reply(anthropic_sse(EFFICIENT_UPSTREAM_MODEL, "WEAK-ANSWER"))
            } else {
                anthropic_json(EFFICIENT_UPSTREAM_MODEL, "WEAK-ANSWER")
            };
            messages_mock(reply, 1).mount(efficient).await;
        }
        Weak::Responses => {
            Mock::given(method("POST"))
                .respond_with(responses_sse("WEAK-ANSWER"))
                .expect(1)
                .mount(responses)
                .await;
        }
    }
}

/// DoD 1. A streaming escalation turn the judge declines is replayed from the
/// retained bytes: the client sees the weak tier's whole turn, rendered under
/// the router id, ending in `message_stop` — on both adapters.
///
/// Non-vacuity: serve anything but the retained bytes on `Replay` (the strong
/// tier live, say) and the capable mock's `expect(0)` and the text go red;
/// skip the adapter's model rewrite by retaining the raw upstream body and
/// `message_start.model` goes red on the Responses run; map `continue` to any
/// other source and the header goes red.
#[tokio::test]
async fn a_declined_streaming_escalation_turn_is_replayed_on_both_adapters() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    for weak in [Weak::Anthropic, Weak::Responses] {
        let (strong, efficient, responses, judge) = (
            MockServer::start().await,
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
        mount_weak(weak, true, &efficient, &responses).await;
        messages_mock(judge_text(DECLINE), 1).mount(&judge).await;
        let tiers = Tiers::of(&strong, &efficient, &responses, judge.uri());
        let gateway = start_gateway(gated_config(&tiers, &escalation_router(weak.alias()))).await;

        let response = post(&gateway, true).await;
        assert_eq!(response.status(), StatusCode::OK, "{weak:?}");
        assert_eq!(
            header(&response, "x-gateway-route-source"),
            "escalation_weak"
        );
        assert_eq!(header(&response, "x-gateway-routed-model"), weak.alias());
        assert!(
            header(&response, "content-type").starts_with("text/event-stream"),
            "{weak:?}"
        );
        let events = sse_events(&response.text().await.unwrap());
        let (first, first_data) = events.first().expect("a replayed turn");
        assert_eq!(first, "message_start", "{weak:?}");
        assert_eq!(first_data["message"]["model"], ROUTER_ID, "{weak:?}");
        assert_eq!(
            events.last().map(|(event, _)| event.as_str()),
            Some("message_stop"),
            "{weak:?}"
        );
        assert_eq!(streamed_text(&events), "WEAK-ANSWER", "{weak:?}");
    }
}

/// DoD 2. A non-streaming caller gets one JSON message, on both adapters: the
/// retained turn is whatever the adapter rendered for the caller's own mode.
/// On the passthrough Anthropic tier the upstream is asked for a non-streaming
/// turn too; the Responses adapter always streams upstream and aggregates, so
/// there only the client half is observable.
///
/// Non-vacuity: force `stream: true` on the gated call and the Anthropic
/// upstream's `stream` assertion goes red; retain only SSE and the
/// content-type assertion goes red on both runs.
#[tokio::test]
async fn a_non_streaming_caller_gets_a_single_json_message_on_both_adapters() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    for weak in [Weak::Anthropic, Weak::Responses] {
        let (strong, efficient, responses, judge) = (
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
        );
        mount_weak(weak, false, &efficient, &responses).await;
        messages_mock(judge_text(DECLINE), 1).mount(&judge).await;
        let tiers = Tiers::of(&strong, &efficient, &responses, judge.uri());
        let gateway = start_gateway(gated_config(&tiers, &escalation_router(weak.alias()))).await;

        let response = post(&gateway, false).await;
        assert_eq!(response.status(), StatusCode::OK, "{weak:?}");
        assert_eq!(
            header(&response, "x-gateway-route-source"),
            "escalation_weak"
        );
        assert!(
            header(&response, "content-type").starts_with("application/json"),
            "{weak:?}"
        );
        let message: Value = response.json().await.expect("a single JSON message");
        assert_eq!(message["type"], "message", "{weak:?}");
        assert_eq!(message["model"], ROUTER_ID, "{weak:?}");
        assert_eq!(message["content"][0]["text"], "WEAK-ANSWER", "{weak:?}");
        if let Weak::Anthropic = weak {
            let sent = bodies(&efficient).await;
            assert_ne!(
                sent[0].get("stream"),
                Some(&json!(true)),
                "the gated call keeps the caller's mode"
            );
        }
    }
}

/// DoD 4a. The escalation judge commits `200` and then hangs: the judge's
/// deadline elapses, libsy fails open to the weak tier, and the turn it
/// already retained is what the client gets.
///
/// Non-vacuity: drop the judge deadline and the request hangs past the
/// harness timeout; map `fail_open` to anything but `DrivenFailOpen` and the
/// header goes red; serve `fail_open` live rather than from the retained turn
/// and the weak mock's `expect(1)` goes red on a second call.
#[tokio::test]
async fn a_stalled_escalation_judge_replays_the_weak_turn_it_retained() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, efficient, responses) = (
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
    mount_weak(Weak::Anthropic, true, &efficient, &responses).await;
    let (judge, judge_closed) = stall_mock(Stall::AfterHeaders).await;
    let tiers = Tiers::of(&strong, &efficient, &responses, judge);
    let gateway = start_gateway(gated_config(&tiers, &escalation_router("efficient-alias"))).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "classifier_fail_open"
    );
    assert_eq!(
        header(&response, "x-gateway-routed-model"),
        "efficient-alias"
    );
    let events = sse_events(&response.text().await.unwrap());
    assert_eq!(streamed_text(&events), "WEAK-ANSWER");
    tokio::time::timeout(std::time::Duration::from_secs(5), judge_closed)
        .await
        .expect("the stalled judge call was cancelled")
        .ok();
}

/// DoD 5. A weak turn that ends before `message_stop` is never served: the
/// cut reaches libsy as a transport failure, which falls back to the strong
/// tier with no judge call, and the partial text never reaches the client.
///
/// Non-vacuity: drop the terminal-marker check from `retain_stream` and the
/// truncated turn is retained; libsy's decode then fails it as a protocol
/// error, not a transport cut, which escalation does not fall back on — the
/// status goes red on `502` (observed). Express a cut as `UpstreamError`
/// instead of a transport item and libsy propagates it the same way.
#[tokio::test]
async fn a_weak_turn_cut_before_message_stop_falls_back_to_the_strong_tier() {
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
    messages_mock(
        sse_reply(anthropic_sse_truncated(
            EFFICIENT_UPSTREAM_MODEL,
            "WEAK-PARTIAL",
        )),
        1,
    )
    .mount(&efficient)
    .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let tiers = Tiers::of(&strong, &efficient, &responses, judge.uri());
    let gateway = start_gateway(gated_config(&tiers, &escalation_router("efficient-alias"))).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_fallback"
    );
    assert_eq!(header(&response, "x-gateway-routed-model"), "capable-alias");
    let body = response.text().await.unwrap();
    assert!(
        !body.contains("WEAK-PARTIAL"),
        "the cut turn leaked: {body}"
    );
    assert_eq!(streamed_text(&sse_events(&body)), "STRONG");
}

/// A streaming gated turn keeps the live path's ordered failover. The weak
/// alias maps two upstreams — an HTTP Responses route that answers `503`,
/// then the Anthropic one — the shape the live path sends through the
/// committed chain stream. The Responses adapter commits a synthetic `200`
/// before it sends, so under the ordered loop the `503` would surface only as
/// an in-stream `error` frame, the chain would never reach the second route,
/// and escalation would read a cut turn and jump to the strong tier.
///
/// Non-vacuity: make `committed_stream` return `None` and the header comes
/// back `escalation_fallback`, the strong mock's `expect(0)` goes red, and the
/// Anthropic route is never called.
#[tokio::test]
async fn a_streaming_gated_turn_fails_over_along_the_weak_chain() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, efficient, judge, down) = (
        MockServer::start().await,
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
    messages_mock(
        sse_reply(anthropic_sse(EFFICIENT_UPSTREAM_MODEL, "WEAK-ANSWER")),
        1,
    )
    .mount(&efficient)
    .await;
    messages_mock(judge_text(DECLINE), 1).mount(&judge).await;
    let gateway = chained_gateway(&strong, &efficient, &judge, &down, 503).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_weak"
    );
    let body = response.text().await.unwrap();
    assert_eq!(streamed_text(&sse_events(&body)), "WEAK-ANSWER", "{body}");
    assert_eq!(
        body.matches("event: message_start").count(),
        1,
        "one start, the winner's: {body}"
    );
}

/// A streaming gated turn whose whole weak chain refuses is relayed as that
/// refusal, as the ordered loop relays it. The committed chain stream answers
/// an exhausted chain with a `200` and one `error` frame; read as a cut turn,
/// that would bill the strong tier and hide the upstream's `429`.
///
/// Non-vacuity: stop `retain_stream` from reading the chain's exhaustion and
/// the header comes back `escalation_fallback` with a `200`, and the strong
/// mock's `expect(0)` goes red.
#[tokio::test]
async fn an_exhausted_streaming_weak_chain_relays_its_refusal() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, efficient, judge, down) = (
        MockServer::start().await,
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
    messages_mock(
        wiremock::ResponseTemplate::new(429).set_body_json(json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "weak tier is saturated"},
        })),
        1,
    )
    .mount(&efficient)
    .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let gateway = chained_gateway(&strong, &efficient, &judge, &down, 503).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&response, "x-gateway-route-source"), "gated_error");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error", "{body}");
}

/// A weak chain that refuses on its Responses route is relayed with the status
/// that route's adapter gives its own client, whichever arm ends the committed
/// stream: an attempt that fails terminally (a `418`, which does not advance)
/// or a chain that runs out with that route's `505` as its remembered failure.
/// Neither status is in the Responses passthrough set, so both reach the
/// caller as `502`, as they do on the ordered loop.
///
/// Non-vacuity: drop the terminal arm's `record_refusal` and the `418` case
/// comes back `escalation_fallback` with a `200`; relay the raw status instead
/// of `client_status` and the cases come back `418` and `505`.
#[tokio::test]
async fn a_refused_streaming_weak_chain_relays_the_client_facing_status() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    // (the Responses route's status, whether the Anthropic route is reached)
    for (down_status, reaches_efficient) in [(418, false), (505, true)] {
        let (strong, efficient, judge, down) = (
            MockServer::start().await,
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
        messages_mock(
            wiremock::ResponseTemplate::new(500),
            u64::from(reaches_efficient),
        )
        .mount(&efficient)
        .await;
        messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
        let gateway = chained_gateway(&strong, &efficient, &judge, &down, down_status).await;

        let response = post(&gateway, true).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{down_status}");
        assert_eq!(header(&response, "x-gateway-route-source"), "gated_error");
    }
}

/// An escalation gateway whose weak alias `chained-alias` maps two upstreams:
/// an HTTP Responses route on `down` that answers `down_status`, then the
/// Anthropic route on `efficient`. That is the shape the live path sends
/// through the committed chain stream.
async fn chained_gateway(
    strong: &MockServer,
    efficient: &MockServer,
    judge: &MockServer,
    down: &MockServer,
    down_status: u16,
) -> judge_harness::TestGateway {
    Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(down_status))
        .expect(1)
        .mount(down)
        .await;
    // Never called: the chain holds no Responses tier of the harness's own.
    let responses = MockServer::start().await;
    let tiers = Tiers::of(strong, efficient, &responses, judge.uri());
    let mut config = harness::unvalidated_gated_config(&tiers, &escalation_router("chained-alias"));
    let mut weak_responses = judge_harness::api_key("down", down.uri(), JUDGE_KEY_ENV);
    weak_responses.kind = Some(shunt::config::ProviderKind::Responses);
    // Ahead of `efficient`: the chain follows `[[upstreams]]` order.
    let at = config
        .upstreams
        .iter()
        .position(|upstream| upstream.name == "efficient")
        .expect("the harness has an efficient upstream");
    config.upstreams.insert(at, weak_responses);
    let mut chained = judge_harness::alias("chained-alias", "efficient", EFFICIENT_UPSTREAM_MODEL);
    chained
        .upstream_model
        .as_mut()
        .expect("an alias maps its upstream")
        .insert("down".to_string(), "upstream-down".to_string());
    config.models.push(chained);
    start_gateway(
        config
            .validate()
            .expect("the chained config is well formed"),
    )
    .await
}
