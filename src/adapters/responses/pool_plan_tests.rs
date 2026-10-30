//! Module-level tests for the chatgpt-plan pre-content check
//! (`pool_plan_peek.rs`) and its pool wiring (`pool_plan.rs`): an in-stream
//! SIWC failure before any output is classified like its HTTP status in both
//! pool loops, and one after output ends the stream and cools the account.
//!
//! The accounts are `token_env` plan accounts, so resolution needs no store
//! file; the 401 refresh-then-retry path that needs a real store runs in
//! `tests/chatgpt_plan_multi_account.rs`.

// The env lock serializes tests that set token env vars across awaits.
#![allow(clippy::await_holding_lock)]

use std::time::Duration;

use axum::http::StatusCode;
use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::context::{PoolForward, TurnOptions};
use super::super::pool::forward_chatgpt_oauth;
use super::super::pool_plan_peek::{peek_pre_content, PEEK_LIMIT};
use super::USAGE_SETTINGS_URL;
use crate::{
    accounts::StoreFamily,
    config::{AccountConfig, AuthMode, Config},
    routing::{AdapterKind, Route},
    server::AppState,
};

const PROVIDER: &str = "chatgpt-plan";

const CREATED: &str = "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n";
const IN_PROGRESS: &str =
    "event: response.in_progress\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n";
const QUEUED: &str = "event: response.queued\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n";
const DELTA: &str =
    "event: response.output_item.added\ndata: {\"item\":{\"type\":\"message\"}}\n\n\
     event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n";
const COMPLETED: &str = "event: response.output_text.done\ndata: {}\n\n\
     event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

fn failed(code: &str) -> String {
    let data = json!({
        "type": "response.failed",
        "response": {"id": "resp_1", "status": "failed", "error": {
            "code": code,
            "message": format!("{code} from upstream"),
        }},
    });
    format!("event: response.failed\ndata: {data}\n\n")
}

fn completed_turn(text: &str) -> String {
    format!(
        "{CREATED}event: response.output_item.added\ndata: {{\"item\":{{\"type\":\"message\"}}}}\n\n\
         event: response.output_text.delta\ndata: {{\"delta\":\"{text}\"}}\n\n{COMPLETED}"
    )
}

/// A 200 event stream served chunk by chunk.
fn chunked(chunks: Vec<Vec<u8>>) -> reqwest::Response {
    streamed(stream::iter(
        chunks
            .into_iter()
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::from(chunk))),
    ))
}

fn streamed(
    body: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
) -> reqwest::Response {
    reqwest::Response::from(
        axum::http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("x-upstream", "kept")
            .body(reqwest::Body::wrap_stream(body))
            .unwrap(),
    )
}

async fn replayed_bytes(response: reqwest::Response) -> Vec<u8> {
    response.bytes().await.expect("replay is readable").to_vec()
}

#[tokio::test]
async fn a_usage_limit_after_lifecycle_frames_becomes_a_429_carrying_the_event() {
    let body = format!(
        "{CREATED}{IN_PROGRESS}{QUEUED}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    let response = peek_pre_content(chunked(vec![body.into_bytes()])).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["x-upstream"], "kept");
    let event: Value = response.json().await.unwrap();
    assert_eq!(event["type"], "response.failed", "{event}");
    assert_eq!(
        event["response"]["error"]["code"], "subscription_sharing_usage_limit_exceeded",
        "{event}"
    );
}

#[tokio::test]
async fn each_mapped_code_takes_its_status() {
    for (code, status) in [
        ("subscription_sharing_invalid_user", 401),
        ("subscription_sharing_usage_unavailable", 503),
        ("subscription_sharing_user_not_eligible", 403),
        ("chatpass_v2_scope_not_authorized", 403),
        ("subscription_sharing_unsupported_capability", 400),
    ] {
        let body = format!("{CREATED}{}", failed(code));
        let response = peek_pre_content(chunked(vec![body.into_bytes()])).await;
        assert_eq!(response.status().as_u16(), status, "{code}");
    }
}

/// A failure frame split at every byte, with CRLF framing and the code on a
/// bare top-level `error` event that has no `event:` line.
#[tokio::test]
async fn a_failure_split_across_chunks_is_still_found() {
    let error =
        json!({"type": "error", "code": "subscription_sharing_invalid_user", "message": "x"});
    let body = format!("event: response.created\r\ndata: {{}}\r\n\r\ndata: {error}\r\n\r\n");
    let chunks = body
        .into_bytes()
        .into_iter()
        .map(|byte| vec![byte])
        .collect();
    let response = peek_pre_content(chunked(chunks)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let event: Value = response.json().await.unwrap();
    assert_eq!(event, error);
}

#[tokio::test]
async fn content_before_a_failure_is_replayed_byte_identical() {
    let body = format!(
        "{CREATED}{DELTA}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    // The decisive frame lands mid-chunk, so part of the next frame is held too.
    let split = CREATED.len() + 20;
    let chunks = vec![
        body.as_bytes()[..split].to_vec(),
        body.as_bytes()[split..].to_vec(),
    ];
    let response = peek_pre_content(chunked(chunks)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(replayed_bytes(response).await, body.as_bytes());
}

#[tokio::test]
async fn an_unmapped_failure_code_is_replayed() {
    let body = format!("{CREATED}{}", failed("rate_limit_exceeded"));
    let response = peek_pre_content(chunked(vec![body.clone().into_bytes()])).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(replayed_bytes(response).await, body.as_bytes());
}

#[tokio::test]
async fn a_stream_that_ends_on_lifecycle_frames_is_replayed_byte_identical() {
    let body = format!("{CREATED}{IN_PROGRESS}: keep-alive\n\n{QUEUED}partial");
    let chunks = body.as_bytes().chunks(7).map(<[u8]>::to_vec).collect();
    let response = peek_pre_content(chunked(chunks)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(replayed_bytes(response).await, body.as_bytes());

    let empty = peek_pre_content(chunked(Vec::new())).await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert!(replayed_bytes(empty).await.is_empty());
}

/// Lifecycle frames past the 64 KiB hold limit stop the check: a failure
/// behind them is relayed, not classified, and every byte is replayed.
#[tokio::test]
async fn lifecycle_frames_past_the_hold_limit_are_replayed_byte_identical() {
    let padding = "x".repeat(1024);
    let frame = format!("event: response.in_progress\ndata: {{\"pad\":\"{padding}\"}}\n\n");
    let mut body = String::new();
    while body.len() < PEEK_LIMIT {
        body.push_str(&frame);
    }
    let held = body.len();
    body.push_str(&failed("subscription_sharing_usage_limit_exceeded"));
    let mut chunks: Vec<Vec<u8>> = body.as_bytes()[..held]
        .chunks(frame.len())
        .map(<[u8]>::to_vec)
        .collect();
    chunks.push(body.as_bytes()[held..].to_vec());
    let response = peek_pre_content(chunked(chunks)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(replayed_bytes(response).await, body.as_bytes());
}

#[tokio::test]
async fn a_transport_error_while_holding_replays_the_held_bytes_then_the_error() {
    let body = stream::iter(vec![
        Ok(Bytes::from_static(CREATED.as_bytes())),
        Err(std::io::Error::other("reset")),
    ]);
    let response = peek_pre_content(streamed(body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut replay = response.bytes_stream();
    let first = replay.next().await.unwrap().unwrap();
    assert_eq!(first, CREATED.as_bytes());
    assert!(replay.next().await.unwrap().is_err());
}

#[tokio::test]
async fn a_non_success_response_is_returned_unread() {
    let response = reqwest::Response::from(
        axum::http::Response::builder()
            .status(429)
            .body(reqwest::Body::from(failed(
                "subscription_sharing_usage_limit_exceeded",
            )))
            .unwrap(),
    );
    let response = peek_pre_content(response).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response
        .text()
        .await
        .unwrap()
        .starts_with("event: response.failed"));
}

/// NFR-1: the check returns at the first content frame, while the upstream is
/// still open and silent, and the replay yields that frame at once.
#[tokio::test]
async fn the_first_content_frame_is_released_without_waiting_for_more() {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(4);
    let body = stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|chunk| (Ok::<_, std::io::Error>(chunk), receiver))
    });
    sender
        .send(Bytes::from_static(CREATED.as_bytes()))
        .await
        .unwrap();
    sender
        .send(Bytes::from_static(DELTA.as_bytes()))
        .await
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(2), peek_pre_content(streamed(body)))
        .await
        .expect("the check does not wait past the content frame");
    let mut replay = response.bytes_stream();
    let held = tokio::time::timeout(Duration::from_secs(2), replay.next())
        .await
        .expect("held bytes are released at once")
        .unwrap()
        .unwrap();
    assert_eq!(held, format!("{CREATED}{DELTA}").as_bytes());
    drop(sender);
}

// --- pool wiring -----------------------------------------------------------

fn plan_state(base_url: &str) -> AppState {
    let mut config = Config::default();
    let mut provider = config.providers["codex"].clone();
    provider.auth = AuthMode::ChatgptPlanOauth;
    provider.base_url = format!("{base_url}/v1");
    provider.websocket = false;
    provider.accounts.clear();
    config.providers.insert(PROVIDER.to_string(), provider);
    AppState::new(config, reqwest::Client::new()).unwrap()
}

fn plan_route() -> Route {
    Route {
        provider: PROVIDER.to_string(),
        adapter: AdapterKind::Responses,
        model: "plan-model".to_string(),
        upstream_model: "gpt-6.1-sol".to_string(),
        effort: None,
        service_tier: None,
    }
}

fn plan_account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        store_family: Some(StoreFamily::ChatgptPlan),
        ..Default::default()
    }
}

fn plan_turn(accounts: Vec<AccountConfig>, stream: bool, idle: Option<Duration>) -> PoolForward {
    PoolForward {
        pool_key: None,
        session_id: None,
        compact: crate::request::CompactionMark::default(),
        delegation: None,
        upstream_body: std::sync::Arc::new(json!({"input": []})),
        accounts_config: accounts,
        turn: TurnOptions {
            attribution: crate::client_attribution::ClientAttribution::UNATTRIBUTED,
            client_wants_stream: stream,
            thinking_enabled: false,
            tool_search_native: false,
            stop_sequences: Vec::new(),
            response_bounds: crate::adapters::ResponseBounds {
                max_bytes: None,
                idle,
            },
        },
        estimate_input: None,
    }
}

fn sse_ok(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn mount(server: &MockServer, token: &str, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body is readable");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Seconds left on `account`'s pool cooldown, `None` when it is not cooling.
fn cooldown_secs(state: &AppState, account: &AccountConfig) -> Option<u64> {
    state
        .accounts
        .snapshot(PROVIDER, std::slice::from_ref(account), None, None)[0]
        .cooldown_secs_remaining
}

/// Fields drop in order: the token vars are restored before the env lock is
/// released, so the next test's vars are never cleared under it.
struct TwoAccounts {
    _tokens: [crate::auth::shared::EnvVarGuard; 2],
    _env: std::sync::MutexGuard<'static, ()>,
    a: AccountConfig,
    b: AccountConfig,
}

fn two_accounts() -> TwoAccounts {
    let env = crate::config::test_env_lock();
    TwoAccounts {
        _tokens: [
            crate::auth::shared::EnvVarGuard::set("SHUNT_PLAN_PEEK_A", "plan-a"),
            crate::auth::shared::EnvVarGuard::set("SHUNT_PLAN_PEEK_B", "plan-b"),
        ],
        _env: env,
        a: plan_account("plan-a", "SHUNT_PLAN_PEEK_A"),
        b: plan_account("plan-b", "SHUNT_PLAN_PEEK_B"),
    }
}

/// AC-023 in both loops: a pre-content in-stream usage limit cools `a` the
/// way an HTTP 429 does and serves the turn from `b`.
#[tokio::test]
async fn a_pre_content_usage_limit_rotates_in_both_loops() {
    for stream in [false, true] {
        let pool = two_accounts();
        let server = MockServer::start().await;
        let limit = format!(
            "{CREATED}{}",
            failed("subscription_sharing_usage_limit_exceeded")
        );
        mount(&server, "plan-a", sse_ok(limit), 1).await;
        mount(&server, "plan-b", sse_ok(completed_turn("from b")), 1).await;
        let state = plan_state(&server.uri());
        let accounts = vec![pool.a.clone(), pool.b.clone()];
        let (status, response) = forward_chatgpt_oauth(
            state.clone(),
            plan_route(),
            plan_turn(accounts, stream, None),
        )
        .await
        .expect("served from b");
        assert_eq!(status, StatusCode::OK, "stream={stream}");
        let text = body_text(response).await;
        assert!(text.contains("from b"), "stream={stream}: {text}");
        assert!(
            !text.contains("rate_limit_error"),
            "stream={stream}: {text}"
        );
        let cooling = cooldown_secs(&state, &pool.a).expect("a is cooling");
        assert!(
            (55..=60).contains(&cooling),
            "HTTP 429 cooldown, got {cooling}"
        );
        assert!(cooldown_secs(&state, &pool.b).is_none());
        server.verify().await;
    }
}

/// AC-024 through the stream: every account failing pre-content on its usage
/// limit ends with the HTTP path's `rate_limit_error` and usage-settings hint.
#[tokio::test]
async fn every_account_on_an_in_stream_usage_limit_points_to_usage_settings() {
    for stream in [false, true] {
        let pool = two_accounts();
        let server = MockServer::start().await;
        for token in ["plan-a", "plan-b"] {
            let limit = format!(
                "{CREATED}{}",
                failed("subscription_sharing_usage_limit_exceeded")
            );
            mount(&server, token, sse_ok(limit), 1).await;
        }
        let state = plan_state(&server.uri());
        let accounts = vec![pool.a.clone(), pool.b.clone()];
        let text =
            match forward_chatgpt_oauth(state, plan_route(), plan_turn(accounts, stream, None))
                .await
            {
                Ok((status, response)) => {
                    assert!(stream, "only the streaming loop commits: {status}");
                    body_text(response).await
                }
                Err(error) => {
                    assert!(!stream);
                    let status = error.response.status();
                    let text = body_text(*error.response).await;
                    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{text}");
                    text
                }
            };
        assert!(text.contains("rate_limit_error"), "stream={stream}: {text}");
        assert!(text.contains(USAGE_SETTINGS_URL), "stream={stream}: {text}");
        assert!(!text.contains("message_stop"), "stream={stream}: {text}");
        server.verify().await;
    }
}

/// AC-021 / AC-020 in-stream: an unavailable code rotates; a permission code
/// relays as `permission_error` without trying the next account.
#[tokio::test]
async fn in_stream_unavailable_rotates_and_permission_relays() {
    let pool = two_accounts();
    let server = MockServer::start().await;
    let unavailable = format!(
        "{CREATED}{}",
        failed("subscription_sharing_usage_unavailable")
    );
    mount(&server, "plan-a", sse_ok(unavailable), 1).await;
    mount(&server, "plan-b", sse_ok(completed_turn("from b")), 1).await;
    let state = plan_state(&server.uri());
    let accounts = vec![pool.a.clone(), pool.b.clone()];
    let (_, response) =
        match forward_chatgpt_oauth(state, plan_route(), plan_turn(accounts, false, None)).await {
            Ok(served) => served,
            Err(error) => panic!("{}", body_text(*error.response).await),
        };
    assert!(body_text(response).await.contains("from b"));
    server.verify().await;

    let server = MockServer::start().await;
    let refused = format!(
        "{CREATED}{}",
        failed("subscription_sharing_user_not_eligible")
    );
    mount(&server, "plan-a", sse_ok(refused), 1).await;
    mount(&server, "plan-b", sse_ok(completed_turn("from b")), 0).await;
    let state = plan_state(&server.uri());
    let accounts = vec![pool.a.clone(), pool.b.clone()];
    let error = forward_chatgpt_oauth(
        state.clone(),
        plan_route(),
        plan_turn(accounts, false, None),
    )
    .await
    .expect_err("relayed");
    assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
    let text = body_text(*error.response).await;
    assert!(text.contains("permission_error"), "{text}");
    assert!(cooldown_secs(&state, &pool.a).is_none());
    server.verify().await;
}

/// An in-stream invalid user on a `token_env` account asks for the refresh,
/// which a static token cannot give, so the account is marked for sign-in and
/// the turn moves on (the store-backed refresh-then-retry is AC-022's
/// integration test).
#[tokio::test]
async fn an_in_stream_invalid_user_takes_the_refresh_arm() {
    let pool = two_accounts();
    let server = MockServer::start().await;
    let invalid = format!("{CREATED}{}", failed("subscription_sharing_invalid_user"));
    mount(&server, "plan-a", sse_ok(invalid), 1).await;
    mount(&server, "plan-b", sse_ok(completed_turn("from b")), 1).await;
    let state = plan_state(&server.uri());
    let accounts = vec![pool.a.clone(), pool.b.clone()];
    let (_, response) =
        forward_chatgpt_oauth(state.clone(), plan_route(), plan_turn(accounts, true, None))
            .await
            .expect("served from b");
    assert!(body_text(response).await.contains("from b"));
    assert!(state.accounts.needs_relogin(PROVIDER, &pool.a));
    server.verify().await;
}

/// AC-019 + FR-12 in the streaming loop: a usage limit after content reached
/// the client is not rotated; the stream ends with `rate_limit_error` instead
/// of `message_stop`, and the account cools like an HTTP 429.
#[tokio::test]
async fn a_usage_limit_after_content_ends_the_stream_and_cools_the_account() {
    let pool = two_accounts();
    let server = MockServer::start().await;
    let late = format!(
        "{CREATED}{DELTA}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    mount(&server, "plan-a", sse_ok(late), 1).await;
    mount(&server, "plan-b", sse_ok(completed_turn("from b")), 0).await;
    let state = plan_state(&server.uri());
    let accounts = vec![pool.a.clone(), pool.b.clone()];
    let (status, response) =
        forward_chatgpt_oauth(state.clone(), plan_route(), plan_turn(accounts, true, None))
            .await
            .expect("committed stream");
    assert_eq!(status, StatusCode::OK);
    let text = body_text(response).await;
    assert!(text.contains("\"text\":\"hi\""), "{text}");
    let error_at = text.find("event: error").expect("error event");
    assert!(text[error_at..].contains("rate_limit_error"), "{text}");
    assert!(!text.contains("event: message_stop"), "{text}");
    let cooling = cooldown_secs(&state, &pool.a).expect("a is cooling");
    assert!(
        (55..=60).contains(&cooling),
        "HTTP 429 cooldown, got {cooling}"
    );
    server.verify().await;
}

/// A streamed usage limit after content carries the hint an HTTP 429 reply
/// for a plan account gets: where to check the plan's usage.
#[tokio::test]
async fn a_streamed_usage_limit_after_content_points_to_usage_settings() {
    let pool = two_accounts();
    let server = MockServer::start().await;
    let late = format!(
        "{CREATED}{DELTA}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    mount(&server, "plan-a", sse_ok(late), 1).await;
    let state = plan_state(&server.uri());
    let (_, response) = forward_chatgpt_oauth(
        state,
        plan_route(),
        plan_turn(vec![pool.a.clone()], true, None),
    )
    .await
    .expect("committed stream");
    let text = body_text(response).await;
    let error_at = text.find("event: error").expect("error event");
    assert!(text[error_at..].contains(USAGE_SETTINGS_URL), "{text}");
    server.verify().await;
}

/// The same late usage limit on the non-streaming loop: the reply is the
/// mapped 429 and the account cools, with no rotation.
#[tokio::test]
async fn a_usage_limit_after_content_cools_on_the_non_streaming_loop() {
    let pool = two_accounts();
    let server = MockServer::start().await;
    let late = format!(
        "{CREATED}{DELTA}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    mount(&server, "plan-a", sse_ok(late), 1).await;
    mount(&server, "plan-b", sse_ok(completed_turn("from b")), 0).await;
    let state = plan_state(&server.uri());
    let accounts = vec![pool.a.clone(), pool.b.clone()];
    let error = forward_chatgpt_oauth(
        state.clone(),
        plan_route(),
        plan_turn(accounts, false, None),
    )
    .await
    .expect_err("the backend error event is the reply");
    assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(body_text(*error.response)
        .await
        .contains("rate_limit_error"));
    assert!(cooldown_secs(&state, &pool.a).is_some());
    server.verify().await;
}

/// FR-12 for an unavailable code in both loops: after content it is not
/// rotated, the reply carries the mapped `overloaded_error`, and the account
/// cools as the pre-content path cools an HTTP 503.
#[tokio::test]
async fn an_unavailable_code_after_content_cools_in_both_loops() {
    for stream in [false, true] {
        let pool = two_accounts();
        let server = MockServer::start().await;
        let late = format!(
            "{CREATED}{DELTA}{}",
            failed("subscription_sharing_usage_unavailable")
        );
        mount(&server, "plan-a", sse_ok(late), 1).await;
        mount(&server, "plan-b", sse_ok(completed_turn("from b")), 0).await;
        let state = plan_state(&server.uri());
        let accounts = vec![pool.a.clone(), pool.b.clone()];
        let served = forward_chatgpt_oauth(
            state.clone(),
            plan_route(),
            plan_turn(accounts, stream, None),
        )
        .await;
        let text = match served {
            Ok((status, response)) => {
                assert!(stream, "the non-streaming reply is the mapped error");
                assert_eq!(status, StatusCode::OK);
                body_text(response).await
            }
            Err(error) => {
                assert!(!stream, "the stream was committed");
                assert_eq!(error.response.status(), StatusCode::SERVICE_UNAVAILABLE);
                body_text(*error.response).await
            }
        };
        assert!(text.contains("overloaded_error"), "stream={stream}: {text}");
        let cooling = cooldown_secs(&state, &pool.a).expect("a is cooling");
        assert!(
            (25..=30).contains(&cooling),
            "stream={stream}: HTTP 503 cooldown, got {cooling}"
        );
        server.verify().await;
    }
}

/// An idle-gated call skips the check: a pre-content usage limit keeps the
/// terminal in-stream error and `b` is never tried. Non-vacuity: drop the
/// `idle` and the same scripts rotate to `b`
/// (`a_pre_content_usage_limit_rotates_in_both_loops`).
#[tokio::test]
async fn an_idle_gated_call_skips_the_check() {
    let pool = two_accounts();
    let server = MockServer::start().await;
    let limit = format!(
        "{CREATED}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    mount(&server, "plan-a", sse_ok(limit), 1).await;
    mount(&server, "plan-b", sse_ok(completed_turn("from b")), 0).await;
    let state = plan_state(&server.uri());
    let accounts = vec![pool.a.clone(), pool.b.clone()];
    let turn = plan_turn(accounts, false, Some(Duration::from_secs(5)));
    let error = forward_chatgpt_oauth(state.clone(), plan_route(), turn)
        .await
        .expect_err("a's backend error event is the reply");
    assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
    let text = body_text(*error.response).await;
    assert!(text.contains("rate_limit_error"), "{text}");
    assert!(
        !text.contains(USAGE_SETTINGS_URL),
        "not the pool's exhaustion: {text}"
    );
    server.verify().await;
}

/// Codex and xAI accounts are untouched by the check: their 2xx answer comes
/// back unread, so the relay sees the in-stream failure as it always did.
#[tokio::test]
async fn other_families_are_not_peeked() {
    let body = format!(
        "{CREATED}{}",
        failed("subscription_sharing_usage_limit_exceeded")
    );
    for family in [None, Some(StoreFamily::Xai)] {
        let account = AccountConfig {
            name: "other".to_string(),
            store_family: family,
            ..Default::default()
        };
        let response =
            super::peek_if_plan(&account, chunked(vec![body.clone().into_bytes()]), None).await;
        assert_eq!(response.status(), StatusCode::OK, "{family:?}");
        assert_eq!(
            replayed_bytes(response).await,
            body.as_bytes(),
            "{family:?}"
        );
    }
    let gated = (Duration::from_secs(5), tokio::time::Instant::now());
    let plan = plan_account("plan", "UNUSED");
    let response =
        super::peek_if_plan(&plan, chunked(vec![body.clone().into_bytes()]), Some(gated)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a gated call is not peeked"
    );
    let response = super::peek_if_plan(&plan, chunked(vec![body.into_bytes()]), None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// NFR-1 end to end on the streaming loop: an upstream that sends a content
/// delta and then stalls with the connection open still gets that delta to
/// the client.
#[tokio::test]
async fn a_content_delta_reaches_the_client_while_the_upstream_stalls() {
    let pool = two_accounts();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "request ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        let mut reply =
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
                .to_vec();
        let events = format!("{CREATED}{DELTA}");
        reply.extend_from_slice(format!("{:x}\r\n{events}\r\n", events.len()).as_bytes());
        socket.write_all(&reply).await.unwrap();
        socket.flush().await.unwrap();
        // Hold the connection open and silent.
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(socket);
    });
    let state = plan_state(&format!("http://{addr}"));
    // One bound covers the commit and the delta, so a regression that waits
    // on later frames fails in 5 s instead of hanging on the silent upstream.
    let mut seen = String::new();
    let delivered = tokio::time::timeout(Duration::from_secs(5), async {
        let (_, response) = forward_chatgpt_oauth(
            state,
            plan_route(),
            plan_turn(vec![pool.a.clone()], true, None),
        )
        .await
        .map_err(|_| "the stream was not committed")?;
        let mut body = response.into_body().into_data_stream();
        while !seen.contains("\"text\":\"hi\"") {
            match body.next().await {
                Some(Ok(chunk)) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                _ => return Err("the stream ended before the delta"),
            }
        }
        Ok(())
    })
    .await;
    upstream.abort();
    match delivered {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => panic!("{reason}; got: {seen}"),
        Err(_) => panic!("the delta waited on later frames; got: {seen}"),
    }
}
