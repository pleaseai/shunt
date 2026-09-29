//! A context-window overflow on the gated call (issue #654): typed as libsy's
//! own client types it, so escalation falls back to the strong target and an
//! advisor relays the refusal — and every other `400` stays a relayed refusal.

use reqwest::StatusCode;
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

use super::harness::{
    anthropic_sse, escalation_router, gated_config, header, judge_text, messages_mock, post,
    sse_events, sse_reply, streamed_text, Tiers, ADVISOR_ROUTER,
};
use super::judge_harness::{can_bind_loopback, env, start_gateway, CAPABLE_UPSTREAM_MODEL};
use super::{Weak, DECLINE};

/// Anthropic's own wording for the refusal, as the passthrough adapter relays it.
fn anthropic_overflow() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "prompt is too long: 250000 tokens > 200000 maximum",
        },
    }))
}

/// OpenAI's wording, which the Responses adapter rewrites into Anthropic's.
fn responses_overflow() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "error": {
            "message": "Your input exceeds the context window of this model. Please adjust your input and try again.",
            "type": "invalid_request_error",
            "param": "input",
            "code": "context_length_exceeded",
        },
    }))
}

/// A `400` that is about the request, not its size, on each adapter.
fn plain_400(weak: Weak) -> ResponseTemplate {
    let body = match weak {
        Weak::Anthropic => json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "tools.0.name: String should match pattern"},
        }),
        Weak::Responses => json!({
            "error": {"message": "Invalid value for 'tool_choice'.", "type": "invalid_request_error", "code": "invalid_value"},
        }),
    };
    ResponseTemplate::new(400).set_body_json(body)
}

/// Mount `reply` as the weak tier's answer on the adapter `weak` names.
async fn mount_weak_reply(
    weak: Weak,
    reply: ResponseTemplate,
    efficient: &MockServer,
    responses: &MockServer,
) {
    match weak {
        Weak::Anthropic => messages_mock(reply, 1).mount(efficient).await,
        Weak::Responses => {
            Mock::given(method("POST"))
                .respond_with(reply)
                .expect(1)
                .mount(responses)
                .await;
        }
    }
}

/// The weak tier refuses the turn as too long for its context window: the
/// strong target answers it live, with no judge call, on both adapters. The
/// Anthropic case reaches the capture as a relayed `400`; the Responses case
/// as the adapter's own error, whose body has to be read to be classified.
///
/// Non-vacuity: report the refusal to libsy as `UpstreamHttp` (the pre-#654
/// typing) and both cases come back `400` / `gated_error` with the strong
/// mock's `expect(1)` red; leave a failed chain's `400` body unread and the
/// Responses case alone goes red the same way.
#[tokio::test]
async fn a_weak_context_overflow_escalates_to_the_strong_tier_on_both_adapters() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    for (weak, stream, reply) in [
        (Weak::Anthropic, true, anthropic_overflow()),
        (Weak::Responses, false, responses_overflow()),
    ] {
        let (strong, efficient, responses, judge) = (
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
        );
        let strong_reply = if stream {
            sse_reply(anthropic_sse(CAPABLE_UPSTREAM_MODEL, "STRONG"))
        } else {
            super::harness::anthropic_json(CAPABLE_UPSTREAM_MODEL, "STRONG")
        };
        messages_mock(strong_reply, 1).mount(&strong).await;
        mount_weak_reply(weak, reply, &efficient, &responses).await;
        messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
        let tiers = Tiers::of(&strong, &efficient, &responses, judge.uri());
        let gateway = start_gateway(gated_config(&tiers, &escalation_router(weak.alias()))).await;

        let response = post(&gateway, stream).await;
        assert_eq!(response.status(), StatusCode::OK, "{weak:?}");
        assert_eq!(
            header(&response, "x-gateway-route-source"),
            "escalation_fallback",
            "{weak:?}"
        );
        assert_eq!(header(&response, "x-gateway-routed-model"), "capable-alias");
        let body = response.text().await.unwrap();
        let text = if stream {
            streamed_text(&sse_events(&body))
        } else {
            let message: Value = serde_json::from_str(&body).unwrap();
            message["content"][0]["text"].as_str().unwrap().to_string()
        };
        assert_eq!(text, "STRONG", "{weak:?}: {body}");
    }
}

/// The positive twin: a `400` about anything but the context window is still
/// the upstream's refusal, relayed with its status, and the strong tier is
/// never billed for it.
#[tokio::test]
async fn a_weak_400_that_is_not_an_overflow_is_still_relayed() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    for (weak, stream) in [(Weak::Anthropic, true), (Weak::Responses, false)] {
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
        mount_weak_reply(weak, plain_400(weak), &efficient, &responses).await;
        messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
        let tiers = Tiers::of(&strong, &efficient, &responses, judge.uri());
        let gateway = start_gateway(gated_config(&tiers, &escalation_router(weak.alias()))).await;

        let response = post(&gateway, stream).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{weak:?}");
        assert_eq!(
            header(&response, "x-gateway-route-source"),
            "gated_error",
            "{weak:?}"
        );
    }
}

/// The advisor's executor overflows: libsy propagates the typed error rather
/// than falling back (an advisor entry has no other target), so the caller
/// gets the refusal itself — a `400` a harness compacts on — and no review.
#[tokio::test]
async fn an_advisor_executor_overflow_is_relayed_as_the_refusal() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, executor, responses, advisor) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(anthropic_overflow(), 1)
        .mount(&executor)
        .await;
    messages_mock(judge_text("APPROVE"), 0)
        .mount(&advisor)
        .await;
    let tiers = Tiers::of(&strong, &executor, &responses, advisor.uri());
    let gateway = start_gateway(gated_config(&tiers, ADVISOR_ROUTER)).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, "x-gateway-route-source"), "gated_error");
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("prompt is too long")),
        "{body}"
    );
}
