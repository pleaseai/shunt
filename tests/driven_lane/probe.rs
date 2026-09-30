//! The `count_tokens` probe on a driven entry (ADR-0005 §3, issue #647): it
//! resolves to the session's retained target, or to the algorithm's
//! no-model-call decision when the session has none, and never drives.
//! Split out of `tests/driven_lane.rs` for the 500-line ceiling; the module
//! doc there covers this file, non-vacuity notes included.
//!
//! Every test runs [`CAPABILITY_ROUTER_NEW_SESSION`], whose judged tier
//! (`efficient-alias`) differs from its fail-open target (`capable-alias`), so
//! which upstream answers the probe says which of the two it resolved to. A
//! `count_tokens` response carries no route headers, which is why these read
//! the mock expectations instead.

use reqwest::StatusCode;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::judge_harness::driven::{
    captured_capability_reply, judge_reply_mock, lane_config, Entry, Judge,
    CAPABILITY_ROUTER_NEW_SESSION,
};
use super::judge_harness::{
    can_bind_loopback, client, env, start_gateway, tier_mock, TestGateway, CAPABLE_UPSTREAM_MODEL,
    CLIENT_TOKEN, EFFICIENT_UPSTREAM_MODEL, ROUTER_ID, SESSION,
};
use super::{post, post_with, source, user_turn};

/// A second session id, for a real turn that must not classify [`SESSION`].
pub(super) const OTHER_SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5f";

/// One `count_tokens` probe on `session`.
pub(super) async fn probe(gateway: &TestGateway, session: &str) -> reqwest::Response {
    probe_with(gateway, &[("x-claude-code-session-id", session)]).await
}

/// One `count_tokens` probe with whatever hint headers the test needs.
pub(super) async fn probe_with(
    gateway: &TestGateway,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = client()
        .post(format!("{}/v1/messages/count_tokens", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", CLIENT_TOKEN);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .body(json!({"model": ROUTER_ID, "messages": user_turn()}).to_string())
        .send()
        .await
        .unwrap()
}

/// A `count_tokens` answer from one upstream, expected `expect` times.
pub(super) fn count_tokens_mock(expect: u64) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .expect(expect)
}

pub(super) async fn assert_counted(response: reqwest::Response) {
    assert_eq!(response.status(), StatusCode::OK);
    let counted: Value = response.json().await.unwrap();
    assert_eq!(counted["input_tokens"], 7);
}

/// Three upstreams and a gateway running the probe tests' entry.
async fn deployment() -> (MockServer, MockServer, MockServer) {
    (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    )
}

async fn gateway_for(
    capable: &MockServer,
    efficient: &MockServer,
    judge: &MockServer,
) -> TestGateway {
    start_gateway(lane_config(
        capable,
        efficient,
        &[Judge::anthropic("judge-a", judge)],
        Entry::Router(CAPABILITY_ROUTER_NEW_SESSION),
    ))
    .await
}

/// Issue #647. A session classified to the weak tier is probed on the weak
/// tier: the probe reads the session's retained target rather than the
/// entry's fail-open one, and makes no judge call of its own.
#[tokio::test]
async fn a_count_tokens_probe_resolves_to_the_sessions_retained_target() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (capable, efficient, judge) = deployment().await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    count_tokens_mock(1).mount(&efficient).await;
    count_tokens_mock(0).mount(&capable).await;
    judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.82), 1)
        .mount(&judge)
        .await;
    let gateway = gateway_for(&capable, &efficient, &judge).await;

    let turn = post(&gateway, user_turn()).await;
    assert_eq!(turn.status(), StatusCode::OK);
    assert_eq!(source(&turn), "llm-classifier");
    assert_eq!(turn.headers()["x-gateway-routed-model"], "efficient-alias");

    assert_counted(probe(&gateway, SESSION).await).await;

    efficient.verify().await;
    capable.verify().await;
    judge.verify().await;
}

/// A session no real turn has classified has no retained target, so its probe
/// takes the no-model-call decision — the fail-open target — both before any
/// turn exists and after a turn on a *different* session was classified.
#[tokio::test]
async fn a_probe_on_an_unclassified_session_takes_the_no_model_call_decision() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (capable, efficient, judge) = deployment().await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    count_tokens_mock(2).mount(&capable).await;
    count_tokens_mock(0).mount(&efficient).await;
    judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.82), 1)
        .mount(&judge)
        .await;
    let gateway = gateway_for(&capable, &efficient, &judge).await;

    assert_counted(probe(&gateway, SESSION).await).await;
    assert!(
        judge
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the probe made no judge call"
    );

    let other = post_with(
        &gateway,
        user_turn(),
        &[("x-claude-code-session-id", OTHER_SESSION)],
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    assert_eq!(other.headers()["x-gateway-routed-model"], "efficient-alias");

    assert_counted(probe(&gateway, SESSION).await).await;

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// A probe changes nothing the next real turn sees: with or without a probe
/// first, the session's first real turn is judged and served on the judged
/// tier. A probe that drove would spend the one-call budget or latch the
/// session's affinity, and the real turn would then report `retained` or fail
/// open instead.
#[tokio::test]
async fn a_probe_leaves_the_next_real_turn_unchanged() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let mut arms = Vec::new();
    for probe_first in [false, true] {
        let (capable, efficient, judge) = deployment().await;
        tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
            .mount(&efficient)
            .await;
        tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
        count_tokens_mock(u64::from(probe_first))
            .mount(&capable)
            .await;
        count_tokens_mock(0).mount(&efficient).await;
        judge_reply_mock(captured_capability_reply("upstream-judge-a", 0.82), 1)
            .mount(&judge)
            .await;
        let gateway = gateway_for(&capable, &efficient, &judge).await;

        if probe_first {
            assert_counted(probe(&gateway, SESSION).await).await;
        }
        let turn = post(&gateway, user_turn()).await;
        assert_eq!(turn.status(), StatusCode::OK, "probe_first={probe_first}");
        let routed = turn.headers()["x-gateway-routed-model"]
            .to_str()
            .unwrap()
            .to_string();
        let turn_source = source(&turn).to_string();
        assert_eq!(routed, "efficient-alias", "probe_first={probe_first}");
        assert_eq!(turn_source, "llm-classifier", "probe_first={probe_first}");
        arms.push((routed, turn_source));

        capable.verify().await;
        efficient.verify().await;
        judge.verify().await;
    }
    assert_eq!(arms[0], arms[1], "the probe changed the next real turn");
}
