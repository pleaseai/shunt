//! How `max_judge_calls` is keyed and charged on the driven lane (issues #648
//! and #649) — split out of `tests/driven_lane.rs` only to keep each file
//! under the repo's 500-line ceiling. The module doc there covers every file
//! of this binary.

use reqwest::StatusCode;
use wiremock::MockServer;

use super::judge_harness::driven::{
    captured_capability_reply, captured_custom_reply, judge_reply_mock, lane_config, Entry, Judge,
};
use super::judge_harness::{
    can_bind_loopback, env, start_gateway, tier_mock, CAPABLE_UPSTREAM_MODEL,
    EFFICIENT_UPSTREAM_MODEL, SESSION,
};
use super::{post_with, source, user_turn};

/// The classifier overlay on `new_session`, budgeted to a single call per
/// `(session, agent)`: the trigger alone keeps a child to one call, and the
/// budget is exactly that one.
const NEW_SESSION_OVERLAY_ONE_CALL: &str = r#"
type = "llm_classifier"
mode = "custom"
default_target = "efficient"
prompt = "Reply with the group that should serve this delegated turn."
response_schema = '{"type":"object","properties":{"target":{"type":"string","enum":["capable","efficient"]}},"required":["target"],"additionalProperties":false}'
classify_trigger = "new_session"
policy = { type = "target_selector", selector = "/target" }
models = { judge = ["judge-a"], capable = ["capable-alias"], efficient = ["efficient-alias"], any = ["capable-alias", "efficient-alias"] }
judge_timeout_ms = 500
max_judge_calls = 1
"#;

/// A capability router that judges every turn, budgeted to two calls per key.
const CAPABILITY_ROUTER_TWO_CALLS: &str = r#"
type = "llm_classifier"
mode = "capability"
classifier_target = "judge-a"
strong_target = "capable-alias"
weak_target = "efficient-alias"
base_threshold = 0.5
classify_trigger = "every_request"
judge_timeout_ms = 500
max_judge_calls = 2
"#;

/// Issue #648: `max_judge_calls` bounds calls, not turns. A `new_session`
/// child is classified once; every later turn of the same `(session, agent)`
/// replays that assignment through libsy's affinity and makes no call, so a
/// spent budget has nothing to refuse. Rejecting the turn before the drive
/// throws away the assignment the session already paid for and falls open.
#[tokio::test]
async fn a_spent_budget_still_replays_a_retained_assignment() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 3).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    judge_reply_mock(captured_custom_reply("upstream-judge-a", "capable"), 1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Overlay(NEW_SESSION_OVERLAY_ONE_CALL),
    ))
    .await;

    let child = [
        ("x-claude-code-session-id", SESSION),
        ("x-claude-code-agent-id", "a7a11c2e22e29e67a"),
    ];
    let first = post_with(&gateway, user_turn(), &child).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(source(&first), "llm-classifier");
    assert_eq!(first.headers()["x-gateway-routed-model"], "capable-alias");

    for turn in 2..=3 {
        let again = post_with(&gateway, user_turn(), &child).await;
        assert_eq!(again.status(), StatusCode::OK, "turn {turn}");
        assert_eq!(
            source(&again),
            "classifier_retained",
            "turn {turn} replays the assignment for free, so a spent budget has nothing to refuse"
        );
        assert_eq!(
            again.headers()["x-gateway-routed-model"],
            "capable-alias",
            "turn {turn} keeps the group it was classified into"
        );
    }

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// Issue #649: a delegated turn that sends no agent id has no identity of its
/// own, but it must not draw on its parent's budget either. Here one parent
/// turn is judged, then three agent-id-less `subagent` turns fan out, then the
/// parent turns again: the parent's second turn is still classified, while the
/// anonymous children are bounded by an allowance of their own — the third is
/// refused.
///
/// The requests run first and the assertions after, parent first, so the
/// failure a shared bucket produces is the one this test is about.
#[tokio::test]
async fn an_anonymous_delegated_fan_out_does_not_spend_the_parent_budget() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    // p_solve 0.82 clears the threshold, so a judged turn lands efficient and
    // a budget-refused one lands on the capability fail-open, which is strong.
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 4)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    // Two for the parent's key, two for the anonymous children's.
    judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.82), 4)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Router(CAPABILITY_ROUTER_TWO_CALLS),
    ))
    .await;

    let parent = [("x-claude-code-session-id", SESSION)];
    let anonymous_child = [
        ("x-claude-code-session-id", SESSION),
        ("x-claude-code-request-class", "subagent"),
    ];

    let parent_first = post_with(&gateway, user_turn(), &parent).await;
    let mut children = Vec::new();
    for _ in 0..3 {
        children.push(post_with(&gateway, user_turn(), &anonymous_child).await);
    }
    let parent_second = post_with(&gateway, user_turn(), &parent).await;

    assert_eq!(parent_first.status(), StatusCode::OK);
    assert_eq!(source(&parent_first), "llm-classifier");
    assert_eq!(parent_second.status(), StatusCode::OK);
    assert_eq!(
        source(&parent_second),
        "llm-classifier",
        "the parent's second turn is judged: agent-id-less children must not spend its budget"
    );
    assert_eq!(
        parent_second.headers()["x-gateway-routed-model"],
        "efficient-alias"
    );

    for (turn, child) in children.iter().enumerate() {
        assert_eq!(child.status(), StatusCode::OK, "child turn {}", turn + 1);
    }
    for (turn, child) in children[..2].iter().enumerate() {
        assert_eq!(
            source(child),
            "llm-classifier",
            "child turn {} is inside the anonymous children's own allowance",
            turn + 1
        );
    }
    assert_eq!(
        source(&children[2]),
        "classifier_fail_open",
        "the anonymous children are still bounded: their third turn is refused"
    );
    assert_eq!(
        children[2].headers()["x-gateway-routed-model"],
        "capable-alias"
    );

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}
