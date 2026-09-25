//! End-to-end coverage for the driven lane: `[models.router] type =
//! "llm_classifier"` / `"composite"` and the classifier form of
//! `[models.subagents]` (ADR-0005 §8 PR 5, issue #595).
//!
//! `tests/router_judge.rs` covers the *stage* judge, where shunt owns the
//! session state and the fallback. Here libsy owns both, so what these pin is
//! the boundary between the two: what shunt sends the judge, which of libsy's
//! outcomes reaches the client as which `x-gateway-route-source`, and that
//! every path that does not produce a verdict still answers `200`.
//!
//! The judge replies are the captured shapes, not invented ones
//! (`docs/notes/adr-0005-routing-live-captures.md`, "Fact (c), re-captured"):
//! a single `text` block with `stop_reason: end_turn` for the verdict, and the
//! verbatim `invalid_request_error` body for the rejection. A mock that
//! answers in a shape Anthropic does not send would pass while the production
//! parse path failed.
//!
//! Non-vacuity: delete the `Router`/`Overlay` dispatch in `proxy::failover`
//! and every routing assertion here goes red on the mock expectations; make
//! `routing::driven::drive` ignore `entry.budget` and
//! `the_budget_is_honoured_for_a_classifier_entry` goes red on the judge's
//! call count; drop the `read_only` guard in `resolve_chain`'s driven arm and
//! `a_count_tokens_probe_on_a_classifier_entry_makes_no_judge_call` goes red;
//! return the classifier form from `routing::subagents::select` and
//! `a_delegated_turn_is_classified_once_per_session_and_agent` goes red on the
//! judge never being called.

mod common;
mod judge_harness;

// The rest of the definition of done, split only for the 500-line ceiling.
#[path = "driven_lane/budget.rs"]
mod budget;
#[path = "driven_lane/overlay.rs"]
mod overlay;

use reqwest::StatusCode;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use judge_harness::driven::{
    captured_capability_reply, captured_custom_reply, contains_key, judge_reply_mock, lane_config,
    only_request_body, responses_judge_mock, responses_verdict_sse, Entry, Judge,
    CAPABILITY_ROUTER, CAPTURED_SCHEMA_REJECTION, COMPOSITE_ROUTER, CUSTOM_ROUTER,
    CUSTOM_ROUTER_TWO_JUDGES,
};
use judge_harness::{
    can_bind_loopback, client, env, start_gateway, tier_mock, undecided_messages, TestGateway,
    CAPABLE_UPSTREAM_MODEL, CLIENT_TOKEN, EFFICIENT_UPSTREAM_MODEL, ROUTER_ID, SESSION,
};

/// A human user turn — what `classify_trigger = "user_turn"` opens on.
pub(crate) fn user_turn() -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": "add a --json flag"}]}])
}

/// One request, with whatever hint headers the test needs.
pub(crate) async fn post_with(
    gateway: &TestGateway,
    messages: Value,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", CLIENT_TOKEN);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .body(json!({"model": ROUTER_ID, "max_tokens": 16, "messages": messages}).to_string())
        .send()
        .await
        .unwrap()
}

pub(crate) async fn post(gateway: &TestGateway, messages: Value) -> reqwest::Response {
    post_with(gateway, messages, &[("x-claude-code-session-id", SESSION)]).await
}

pub(crate) fn source(response: &reqwest::Response) -> &str {
    response.headers()["x-gateway-route-source"]
        .to_str()
        .expect("the source header is ASCII")
}

/// DoD 1. The whole lane in one turn: the judge answers in the captured
/// Anthropic shape and its `p_solve` — against `base_threshold` — is what
/// picks the tier. The second run is the same reply with the other verdict, so
/// the assertion is on the threshold rather than on a constant.
///
/// The request assertions are the other half: the body shunt hands the judge
/// must be the shape the live capture recorded, because a schema the grammar
/// refuses is a `400` on the first call (test 3) and a `stream` key is a reply
/// this path cannot parse.
#[tokio::test]
async fn a_capability_verdict_from_a_real_anthropic_reply_shape_routes_the_turn() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;

    // p_solve 0.82 clears base_threshold 0.5: the weak tier is trusted.
    for (p_solve, upstream_model, expected_tier) in [
        (0.82, EFFICIENT_UPSTREAM_MODEL, "efficient-alias"),
        (0.10, CAPABLE_UPSTREAM_MODEL, "capable-alias"),
    ] {
        let capable = MockServer::start().await;
        let efficient = MockServer::start().await;
        let judge = MockServer::start().await;
        tier_mock(
            CAPABLE_UPSTREAM_MODEL,
            u64::from(upstream_model == CAPABLE_UPSTREAM_MODEL),
        )
        .mount(&capable)
        .await;
        tier_mock(
            EFFICIENT_UPSTREAM_MODEL,
            u64::from(upstream_model == EFFICIENT_UPSTREAM_MODEL),
        )
        .mount(&efficient)
        .await;
        judge_reply_mock(captured_capability_reply("upstream-judge-a", p_solve), 1)
            .mount(&judge)
            .await;
        let gateway = start_gateway(lane_config(
            &capable,
            &efficient,
            &[Judge::anthropic("judge-a", &judge)],
            Entry::Router(CAPABILITY_ROUTER),
        ))
        .await;

        let response = post(&gateway, user_turn()).await;

        assert_eq!(response.status(), StatusCode::OK, "p_solve={p_solve}");
        assert_eq!(source(&response), "llm-classifier", "p_solve={p_solve}");
        assert_eq!(
            response.headers()["x-gateway-routed-model"],
            expected_tier,
            "p_solve={p_solve}"
        );

        let judged = only_request_body(&judge).await;
        assert_eq!(
            judged
                .pointer("/output_config/format/type")
                .and_then(Value::as_str),
            Some("json_schema"),
            "the judge request carries the structured-output field the capture recorded: {judged}"
        );
        assert!(judged.get("tools").is_none(), "no tools: {judged}");
        assert!(
            judged.get("tool_choice").is_none(),
            "no tool_choice: {judged}"
        );
        assert!(
            judged.get("system").is_some_and(Value::is_string),
            "the packaged prompt joins into a string system: {judged}"
        );
        assert_eq!(
            judged.get("max_tokens").and_then(Value::as_u64),
            Some(4_096)
        );
        assert!(
            judged.get("stream").is_none(),
            "a judge answer is collected whole: {judged}"
        );
        let schema = judged
            .pointer("/output_config/format/schema")
            .expect("the packaged schema is sent");
        for keyword in ["minimum", "maximum"] {
            assert!(
                !contains_key(schema, keyword),
                "{keyword} is the keyword Anthropic 400s on, stripped by the codec: {schema}"
            );
        }

        capable.verify().await;
        efficient.verify().await;
        judge.verify().await;
    }
}

/// DoD 2. A judge on a Responses provider. The verdict arrives as an SSE
/// stream and the schema has to have survived as `text.format` — without that
/// translation the judge answers in prose and every turn fails open silently.
#[tokio::test]
async fn a_custom_verdict_from_an_openai_json_schema_reply_routes_the_turn() {
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
    responses_judge_mock(responses_verdict_sse("capable"), 1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::responses("judge-a", &judge)],
        Entry::Router(CUSTOM_ROUTER),
    ))
    .await;

    let response = post(&gateway, user_turn()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(source(&response), "llm-classifier");
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "capable-alias",
        "the verdict named the capable group, whose first model serves the turn"
    );

    let judged = only_request_body(&judge).await;
    assert_eq!(
        judged.pointer("/text/format/type").and_then(Value::as_str),
        Some("json_schema"),
        "the Anthropic output_config.format became the Responses text.format: {judged}"
    );
    assert!(
        judged.pointer("/text/format/schema").is_some(),
        "the schema travels with it: {judged}"
    );

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// DoD 3. The captured `400`, verbatim. libsy folds every call failure into
/// "no verdict" and closes on its own default, and it makes exactly one
/// `CallModel` with the whole judge list — so a failing first judge is not
/// retried against the second, and the client's turn is still served.
#[tokio::test]
async fn a_400_from_the_judge_is_a_classifier_failure_with_no_further_judge_calls() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge_a = MockServer::start().await;
    let judge_b = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string(CAPTURED_SCHEMA_REJECTION))
        .expect(1)
        .mount(&judge_a)
        .await;
    judge_reply_mock(captured_custom_reply("upstream-judge-b", "capable"), 0)
        .mount(&judge_b)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[
            Judge::anthropic("judge-a", &judge_a),
            Judge::anthropic("judge-b", &judge_b),
        ],
        Entry::Router(CUSTOM_ROUTER_TWO_JUDGES),
    ))
    .await;

    let response = post(&gateway, user_turn()).await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a judge that 400s must not take the caller's turn down with it"
    );
    assert_eq!(source(&response), "classifier_fail_open");
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "efficient-alias",
        "default_target's first model"
    );

    capable.verify().await;
    efficient.verify().await;
    judge_a.verify().await;
    judge_b.verify().await;
}

/// The same failure in capability mode, where the fail-open target is the
/// *strong* tier: libsy's capability route closes on `Category::Capable`.
#[tokio::test]
async fn a_400_from_a_capability_judge_serves_the_strong_target() {
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
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string(CAPTURED_SCHEMA_REJECTION))
        .expect(1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(lane_config(
        &capable,
        &efficient,
        &[Judge::anthropic("judge-a", &judge)],
        Entry::Router(CAPABILITY_ROUTER),
    ))
    .await;

    let response = post(&gateway, user_turn()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(source(&response), "classifier_fail_open");
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "capable-alias"
    );

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// DoD 4. `classify_trigger = "user_turn"` is what makes a composite entry
/// cheaper than a judge on every turn: the human turn is judged, the tool
/// continuation that follows replays the tier it set.
#[tokio::test]
async fn a_composite_user_turn_consults_the_judge_and_a_tool_continuation_does_not() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 2).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    // p_solve under the threshold: the efficient tier is not trusted, so the
    // judge raises the fall-open tier to capable for the whole turn.
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

    let opened = post(&gateway, user_turn()).await;
    assert_eq!(opened.status(), StatusCode::OK);
    assert_eq!(opened.headers()["x-gateway-routed-model"], "capable-alias");

    // The continuation carries the same session and no new human turn, so the
    // trigger does not fire and the retained tier serves it.
    let mut continued_messages = user_turn().as_array().unwrap().clone();
    continued_messages.extend(undecided_messages().as_array().unwrap().clone());
    let continued = post(&gateway, Value::Array(continued_messages)).await;

    assert_eq!(continued.status(), StatusCode::OK);
    assert_eq!(
        continued.headers()["x-gateway-routed-model"],
        "capable-alias",
        "the tier the judge set is retained across the tool call"
    );
    assert_ne!(
        source(&continued),
        "classifier_default",
        "the second turn was still driven, just without a call"
    );

    // The assertion that matters: one judge call for both turns.
    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}
