//! The advisor half of the definition of done: REDO, a stalled review, and a
//! nonterminal executor turn.

use reqwest::StatusCode;
use serde_json::Value;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer,
};

use super::harness::{
    anthropic_sse, anthropic_sse_truncated, bodies, gated_config, header, judge_text,
    messages_mock, post, post_in_session, sse_events, sse_reply, streamed_text, Tiers,
    ADVISOR_ROUTER,
};
use super::judge_harness::{
    can_bind_loopback, env, stall_mock, start_gateway, Stall, EFFICIENT_UPSTREAM_MODEL,
};

const PLAN: &str = "Run the tests before finishing.";

/// The text of one outbound message, whichever content form the codec chose.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

/// DoD 3. The advisor answers REDO: the draft turn is discarded, the executor
/// is dispatched again with the draft echoed back and the plan as feedback,
/// and only that second turn reaches the client.
///
/// Non-vacuity: replay the retained turn on `Dispatch` and `DRAFT-ANSWER`
/// reaches the client; drop `append_messages` and the second executor
/// request's tail assertions go red; map `redo` to any other source and the
/// header goes red.
#[tokio::test]
async fn an_advisor_redo_serves_only_the_second_executor_turn() {
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
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_reply(anthropic_sse(
            EFFICIENT_UPSTREAM_MODEL,
            "DRAFT-ANSWER",
        )))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&executor)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_reply(anthropic_sse(
            EFFICIENT_UPSTREAM_MODEL,
            "FINAL-ANSWER",
        )))
        .with_priority(2)
        .mount(&executor)
        .await;
    messages_mock(judge_text(&format!("REDO\n{PLAN}")), 1)
        .mount(&advisor)
        .await;
    let tiers = Tiers::of(&strong, &executor, &responses, advisor.uri());
    let gateway = start_gateway(gated_config(&tiers, ADVISOR_ROUTER)).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-gateway-route-source"), "advisor_redo");
    assert_eq!(
        header(&response, "x-gateway-routed-model"),
        "efficient-alias"
    );
    let body = response.text().await.unwrap();
    assert!(!body.contains("DRAFT-ANSWER"), "the draft leaked: {body}");
    assert_eq!(streamed_text(&sse_events(&body)), "FINAL-ANSWER");

    let sent = bodies(&executor).await;
    assert_eq!(sent.len(), 2, "the executor is called exactly twice");
    let messages = sent[1]["messages"].as_array().expect("messages");
    let (echo, feedback) = (&messages[messages.len() - 2], &messages[messages.len() - 1]);
    assert_eq!(echo["role"], "assistant");
    assert!(message_text(echo).contains("DRAFT-ANSWER"), "{echo}");
    assert_eq!(feedback["role"], "user");
    assert!(message_text(feedback).contains(PLAN), "{feedback}");
    assert_eq!(
        message_text(&messages[0]),
        "add a --json flag",
        "the caller's turn stays at the head"
    );
}

/// DoD 4b. The advisor's review commits `200` and then hangs: the review
/// fails open and the executor turn already retained is served.
///
/// Non-vacuity: drop the judge deadline and the request hangs; map the
/// advisor's `fail_open` to anything but `AdvisorFailOpen` and the header goes
/// red; dispatch the executor live instead of replaying and its `expect(1)`
/// goes red.
#[tokio::test]
async fn a_stalled_advisor_review_replays_the_executor_turn() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let (strong, executor, responses) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(
        sse_reply(anthropic_sse(EFFICIENT_UPSTREAM_MODEL, "EXECUTOR-ANSWER")),
        1,
    )
    .mount(&executor)
    .await;
    let (advisor, advisor_closed) = stall_mock(Stall::AfterHeaders).await;
    let tiers = Tiers::of(&strong, &executor, &responses, advisor);
    let gateway = start_gateway(gated_config(&tiers, ADVISOR_ROUTER)).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "advisor_fail_open"
    );
    let events = sse_events(&response.text().await.unwrap());
    assert_eq!(streamed_text(&events), "EXECUTOR-ANSWER");
    assert_eq!(
        events.last().map(|(event, _)| event.as_str()),
        Some("message_stop")
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), advisor_closed)
        .await
        .expect("the stalled review was cancelled")
        .ok();
}

/// DoD 6. An executor turn cut before `message_stop` is a gateway failure,
/// not a turn: the advisor has no fallback tier, so the client gets a `502` in
/// the Anthropic error shape — never a truncated stream — and the advisor is
/// not consulted over a turn that did not finish.
///
/// Non-vacuity: drop the `GatedExit::Fail` stamp and the header assertion goes
/// red; build the failure in any shape but `api_error` and the body assertion
/// goes red. The terminal-marker check itself is *not* what holds this one:
/// with it removed, libsy's own decode of a stream that never reached
/// `message_stop` refuses the turn too (observed), so this stays green — the
/// check is pinned by
/// `a_weak_turn_cut_before_message_stop_falls_back_to_the_strong_tier`, where
/// the two guards produce different outcomes.
#[tokio::test]
async fn a_nonterminal_executor_turn_is_a_gateway_error() {
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
    messages_mock(
        sse_reply(anthropic_sse_truncated(
            EFFICIENT_UPSTREAM_MODEL,
            "EXECUTOR-PARTIAL",
        )),
        1,
    )
    .mount(&executor)
    .await;
    messages_mock(judge_text("APPROVE"), 0)
        .mount(&advisor)
        .await;
    let tiers = Tiers::of(&strong, &executor, &responses, advisor.uri());
    let gateway = start_gateway(gated_config(&tiers, ADVISOR_ROUTER)).await;

    let response = post(&gateway, true).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(header(&response, "x-gateway-route-source"), "gated_error");
    assert!(
        !header(&response, "content-type").starts_with("text/event-stream"),
        "a cut turn is never streamed"
    );
    let error: Value = response.json().await.expect("an Anthropic error body");
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "api_error");
    assert!(
        !error.to_string().contains("EXECUTOR-PARTIAL"),
        "the cut turn leaked: {error}"
    );
}

/// The review sources two advisor turns report, one after the other, with the
/// session header set to each `session` in turn (or left off).
async fn two_advisor_turns(sessions: [Option<&str>; 2], reviews: u64) -> [String; 2] {
    let (strong, executor, responses, advisor) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_reply(anthropic_sse(
            EFFICIENT_UPSTREAM_MODEL,
            "EXECUTOR-ANSWER",
        )))
        .mount(&executor)
        .await;
    messages_mock(judge_text("APPROVE"), reviews)
        .mount(&advisor)
        .await;
    let tiers = Tiers::of(&strong, &executor, &responses, advisor.uri());
    let gateway = start_gateway(gated_config(&tiers, ADVISOR_ROUTER)).await;

    let mut sources = Vec::new();
    for session in sessions {
        let response = post_in_session(&gateway, true, session).await;
        assert_eq!(response.status(), StatusCode::OK);
        sources.push(header(&response, "x-gateway-route-source").to_string());
        let body = response.text().await.unwrap();
        assert_eq!(streamed_text(&sse_events(&body)), "EXECUTOR-ANSWER");
    }
    sources.try_into().expect("two turns")
}

/// `max_reviews` is per session, and a caller that sends no session is not
/// one shared session: two unrelated sessionless turns are each reviewed.
/// The pinned `AdvisorGate` would otherwise fold both into its instance-wide
/// scope, and the first caller's single review would turn review off for
/// every later sessionless caller until the config reloads.
///
/// Non-vacuity: drop `scope_sessionless_advisor` and the second turn comes
/// back `advisor_exhausted` with the advisor called once. The budget does
/// bite — see `one_session_spends_its_review_budget` — so this is not green
/// merely because a review is never refused.
#[tokio::test]
async fn sessionless_advisor_turns_do_not_share_one_review_budget() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let sources = two_advisor_turns([None, None], 2).await;
    assert_eq!(sources, ["advisor_approve", "advisor_approve"]);
}

/// The positive twin: within one session the default `max_reviews = 1` is
/// spent by the first turn, and the second answers live without a review.
#[tokio::test]
async fn one_session_spends_its_review_budget() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let sources = two_advisor_turns([Some("session-a"), Some("session-a")], 1).await;
    assert_eq!(sources, ["advisor_approve", "advisor_exhausted"]);
}
