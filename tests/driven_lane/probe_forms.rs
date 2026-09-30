//! The `count_tokens` probe on the three driven forms `probe.rs` does not
//! drive (issue #647): a composite router's retained tier, a classifier
//! overlay's `(session, agent)` assignment, and an escalation latch — each set
//! by a real turn through the gateway, then read by a probe on the same
//! identity. Split out of `tests/driven_lane.rs` for the 500-line ceiling; the
//! module doc there covers this file.
//!
//! In every test the target the real turn leaves retained differs from the
//! form's no-record target, so which upstream answers the probe says which of
//! the two it resolved to. A `count_tokens` response carries no route headers,
//! which is why these read the mock expectations instead.

use reqwest::StatusCode;
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::judge_harness::driven::{
    captured_capability_reply, captured_custom_reply, captured_escalation_reply, judge_reply_mock,
    lane_config, Entry, Judge, CLASSIFIER_OVERLAY, COMPOSITE_ROUTER, ESCALATION_ROUTER,
};
use super::judge_harness::{
    self, can_bind_loopback, env, start_gateway, tier_mock, CAPABLE_UPSTREAM_MODEL,
    EFFICIENT_UPSTREAM_MODEL, SESSION,
};
use super::probe::{assert_counted, count_tokens_mock, probe, probe_with, OTHER_SESSION};
use super::{post, post_with, source, user_turn};

/// The delegated agent the overlay test classifies, and a sibling it does not.
const AGENT: &str = "a7a11c2e22e29e67a";
const OTHER_AGENT: &str = "acf659811cf929e90";

/// Composite. A user turn the judge raises to the capable tier leaves that
/// tier retained, and a probe on the session answers from it rather than from
/// the picker default (`stage.efficient_target`) a record-less probe — here on
/// a session no turn has classified — takes.
#[tokio::test]
async fn a_composite_probe_resolves_to_the_tier_the_judge_set() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    count_tokens_mock(1).mount(&capable).await;
    count_tokens_mock(1).mount(&efficient).await;
    // p_solve under the threshold: the judge raises the tier to capable.
    judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.1), 1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Router(COMPOSITE_ROUTER),
    ))
    .await;

    let turn = post(&gateway, user_turn()).await;
    assert_eq!(turn.status(), StatusCode::OK);
    assert_eq!(source(&turn), "llm-classifier");
    assert_eq!(turn.headers()["x-gateway-routed-model"], "capable-alias");

    assert_counted(probe(&gateway, SESSION).await).await;
    assert_counted(probe(&gateway, OTHER_SESSION).await).await;

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// Classifier overlay. A delegated turn classified to the capable group leaves
/// that child's assignment retained: a delegated probe with the same session
/// and agent answers from it, while a sibling agent the gateway never
/// classified answers from `default_target`'s first model.
#[tokio::test]
async fn a_delegated_probe_resolves_to_its_agents_overlay_assignment() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    count_tokens_mock(1).mount(&capable).await;
    count_tokens_mock(1).mount(&efficient).await;
    judge_reply_mock(captured_custom_reply("upstream-judge-a", "capable"), 1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Overlay(CLASSIFIER_OVERLAY),
    ))
    .await;

    let child = [
        ("x-claude-code-session-id", SESSION),
        ("x-claude-code-agent-id", AGENT),
    ];
    let turn = post_with(&gateway, user_turn(), &child).await;
    assert_eq!(turn.status(), StatusCode::OK);
    assert_eq!(source(&turn), "llm-classifier");
    assert_eq!(turn.headers()["x-gateway-routed-model"], "capable-alias");

    assert_counted(probe_with(&gateway, &child).await).await;
    assert_counted(
        probe_with(
            &gateway,
            &[
                ("x-claude-code-session-id", SESSION),
                ("x-claude-code-agent-id", OTHER_AGENT),
            ],
        )
        .await,
    )
    .await;

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// Escalation. A weak turn the judge confirms an escalation on latches the
/// session onto `strong_target`, so a probe on it — its parent's or a delegated
/// child's — answers from the strong tier; a probe on a session that never
/// latched answers from `weak_target`.
#[tokio::test]
async fn an_escalation_probe_resolves_to_the_strong_target_once_latched() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    // The weak turn is the gated one: it must be a complete turn, or the
    // judge is never asked and escalation falls back instead of latching.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(judge_harness::ModelIs(EFFICIENT_UPSTREAM_MODEL))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_weak",
            "type": "message",
            "role": "assistant",
            "model": EFFICIENT_UPSTREAM_MODEL,
            "content": [{"type": "text", "text": "WEAK-ANSWER"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3},
        })))
        .expect(1)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    count_tokens_mock(2).mount(&capable).await;
    count_tokens_mock(1).mount(&efficient).await;
    judge_reply_mock(captured_escalation_reply("upstream-judge-a", true), 1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Router(ESCALATION_ROUTER),
    ))
    .await;

    let turn = post(&gateway, user_turn()).await;
    assert_eq!(turn.status(), StatusCode::OK);
    assert_eq!(source(&turn), "escalation_latch");
    assert_eq!(turn.headers()["x-gateway-routed-model"], "capable-alias");

    assert_counted(probe(&gateway, SESSION).await).await;
    // The latch is the session's, so a delegated child of it reads it too.
    assert_counted(
        probe_with(
            &gateway,
            &[
                ("x-claude-code-session-id", SESSION),
                ("x-claude-code-agent-id", AGENT),
            ],
        )
        .await,
    )
    .await;
    assert_counted(probe(&gateway, OTHER_SESSION).await).await;

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}
