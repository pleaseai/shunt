//! The overlay, probe, and budget halves of the driven-lane definition of
//! done (ADR-0005 §8 PR 5) — split out of `tests/driven_lane.rs` only to keep
//! each file under the repo's 500-line ceiling. The module doc there covers
//! both files, non-vacuity notes included.

use reqwest::StatusCode;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::judge_harness::driven::{
    captured_capability_reply, captured_custom_reply, judge_reply_mock, lane_config, Entry, Judge,
    CAPABILITY_ROUTER, CAPABILITY_ROUTER_ONE_CALL, CLASSIFIER_OVERLAY, PARENT_UPSTREAM_MODEL,
};
use super::judge_harness::{
    self, can_bind_loopback, client, env, start_gateway, tier_mock, undecided_messages,
    TestGateway, CAPABLE_UPSTREAM_MODEL, CLIENT_TOKEN, EFFICIENT_UPSTREAM_MODEL, ROUTER_ID,
    SESSION,
};
use super::{post, post_with, source, user_turn};

/// DoD 5. The overlay form. The parent is never classified — it resolves the
/// entry as if the table were absent — and a child is classified once per
/// `(session, agent)`, which is what `new_session` means here.
#[tokio::test]
async fn a_delegated_turn_is_classified_once_per_session_and_agent() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 3).mount(&capable).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(judge_harness::ModelIs(PARENT_UPSTREAM_MODEL))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({"id": "msg_1", "type": "message", "model": PARENT_UPSTREAM_MODEL}).to_string(),
        ))
        .expect(1)
        .mount(&efficient)
        .await;
    judge_reply_mock(captured_custom_reply("upstream-judge-a", "capable"), 2)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Overlay(CLASSIFIER_OVERLAY),
    ))
    .await;

    let parent = post(&gateway, user_turn()).await;
    assert_eq!(parent.status(), StatusCode::OK);
    assert!(
        parent.headers().get("x-gateway-routed-model").is_none(),
        "a parent turn is not diverted, so no router decided it: {:?}",
        parent.headers()
    );

    async fn child(gateway: &TestGateway, agent: &str) -> reqwest::Response {
        post_with(
            gateway,
            user_turn(),
            &[
                ("x-claude-code-session-id", SESSION),
                ("x-claude-code-agent-id", agent),
            ],
        )
        .await
    }

    let first = child(&gateway, "a7a11c2e22e29e67a").await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(source(&first), "llm-classifier");
    assert_eq!(first.headers()["x-gateway-routed-model"], "capable-alias");

    // The same (session, agent): libsy's affinity replays the assignment, so
    // no second judge call.
    let again = child(&gateway, "a7a11c2e22e29e67a").await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(
        again.headers()["x-gateway-routed-model"],
        "capable-alias",
        "the child keeps the group it was classified into"
    );
    assert_eq!(
        source(&again),
        "classifier_retained",
        "a replayed assignment is reported as retained, not as a fresh verdict"
    );

    // A different child of the same session is its own identity.
    let sibling = child(&gateway, "acf659811cf929e90").await;
    assert_eq!(sibling.status(), StatusCode::OK);
    assert_eq!(source(&sibling), "llm-classifier");

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// The other half of DoD 5: the overlay's judge is inside the envelope a
/// delegated turn is admitted against, so an unauthenticated child is refused
/// before the gateway spends a judge call on it.
#[tokio::test]
async fn an_unauthenticated_delegated_turn_is_refused_before_the_overlay_judge_is_called() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    judge_reply_mock(captured_custom_reply("upstream-judge-a", "capable"), 0)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Overlay(CLASSIFIER_OVERLAY),
    ))
    .await;

    let refused = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .header("x-claude-code-agent-id", "a7a11c2e22e29e67a")
        .body(json!({"model": ROUTER_ID, "max_tokens": 16, "messages": user_turn()}).to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    judge.verify().await;
}

/// DoD 6. `classify_trigger` on the *stage* classifier, which is the pure
/// lane's judge: a tool continuation is not a human turn, so it takes the
/// picker's fall-open tier with no call at all.
#[tokio::test]
async fn a_stage_classifier_with_user_turn_trigger_skips_tool_continuations() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    judge_harness::judge_mock(0.1, 0).mount(&judge).await;
    let config = judge_harness::driven_config_with(&capable, &efficient, judge.uri(), |config| {
        let router = config.models[0]
            .router
            .as_mut()
            .expect("the harness entry carries a router");
        if let shunt::config::RouterConfig::StageRouter(stage) = router {
            stage
                .classifier
                .as_mut()
                .expect("the harness entry carries a classifier")
                .classify_trigger = shunt::config::ClassifyTrigger::UserTurn;
        }
    });
    let gateway = start_gateway(config).await;

    let response = judge_harness::post(&gateway, undecided_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        source(&response),
        "fall_open",
        "the trigger did not fire, so the picker's default stands"
    );
    efficient.verify().await;
    judge.verify().await;
}

/// DoD 7. ADR-0005 §3: a `count_tokens` probe resolves to the algorithm's
/// no-model-call decision and makes zero judge calls.
#[tokio::test]
async fn a_count_tokens_probe_on_a_classifier_entry_makes_no_judge_call() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.1), 0)
        .mount(&judge)
        .await;
    // The probe lands on the fail-open target — the capability mode's strong
    // tier — and is answered there, without a judge call.
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .expect(1)
        .mount(&capable)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Router(CAPABILITY_ROUTER),
    ))
    .await;

    let probe = client()
        .post(format!("{}/v1/messages/count_tokens", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", CLIENT_TOKEN)
        .header("x-claude-code-session-id", SESSION)
        .body(json!({"model": ROUTER_ID, "messages": user_turn()}).to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(probe.status(), StatusCode::OK);
    let counted: Value = probe.json().await.unwrap();
    assert_eq!(counted["input_tokens"], 7);
    // The destinations are the assertion: the probe was answered by the
    // fail-open target's upstream and the judge was never called. A
    // `count_tokens` response carries no `x-gateway-route-source`, which is
    // why this reads the mock expectations rather than a header.
    judge.verify().await;
    capable.verify().await;
}

/// DoD 8. `max_judge_calls` is a ceiling on calls *made*: the first turn
/// spends the budget, the second finds it empty and is answered by the
/// algorithm's fail-open target without a call.
#[tokio::test]
async fn the_budget_is_honoured_for_a_classifier_entry() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    // p_solve 0.82 clears the threshold, so a judged turn lands efficient and
    // a budget-exhausted one lands on the capability fail-open, which is
    // strong — the two turns are told apart by destination as well as header.
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.82), 1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Router(CAPABILITY_ROUTER_ONE_CALL),
    ))
    .await;

    let first = post(&gateway, user_turn()).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(source(&first), "llm-classifier");
    assert_eq!(first.headers()["x-gateway-routed-model"], "efficient-alias");

    let second = post(&gateway, user_turn()).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        source(&second),
        "classifier_fail_open",
        "the budget was spent, so no verdict decided this turn"
    );
    assert_eq!(second.headers()["x-gateway-routed-model"], "capable-alias");

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}
