//! Module-level tests for the xAI account pool (`StoreFamily::Xai`) riding the
//! Codex Responses pool loop (docs/m18-xai-multi-account.md).
//!
//! The `xai_oauth` bearer guard (`Config::validate`) pins an xAI provider to an
//! https x.ai/grok.com host with no loopback exception, so these tests route to
//! the loopback `openai` provider and stamp each account `StoreFamily::Xai`.
//! Credential resolution and failover classification key on that stamp, so
//! they are exercised as in production.
//!
//! Fidelity gap: the route's provider auth here is `api_key`, so
//! `provider_auth` yields `ApiKey` and the gateway-owned error wording these
//! pool runs produce is the generic mapping, not the xAI one. The xAI wording
//! is asserted elsewhere: `provider_auth` on the built-in `grok` provider and
//! `exhausted_error` with `AuthMode::XaiOauth` (below), `mapped_upstream_error`
//! with `XaiOauth` in `error.rs`'s tests, and the gateway path in
//! `tests/xai_multi_account.rs`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::context::{PoolForward, TurnOptions};
use super::{exhausted_error, forward_chatgpt_oauth, no_upstream_response_message, provider_auth};
use crate::{
    accounts::StoreFamily,
    config::{AccountConfig, AuthMode, Config},
    routing::{AdapterKind, Route},
    server::AppState,
};

// `COMPLETED_TURN` from `pool.rs`'s tests.
const COMPLETED_TURN: &str = "event: response.created\ndata: {\"response\":{\"id\":\"resp_2\"}}\n\n\
     event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n\
     event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "shunt-xai-pool-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// An unsigned JWT that `is_token_valid_at` accepts until 2096, distinct per `tag`.
fn xai_token(tag: &str) -> String {
    let payload = json!({ "exp": 4_000_000_000u64, "sub": tag });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// A `StoreFamily::Xai` account backed by its own credentials file holding a
/// valid token, so resolution needs no env var and no refresh.
fn xai_account(dir: &Path, name: &str) -> (AccountConfig, String) {
    let token = xai_token(name);
    let path = dir.join(format!("{name}.json"));
    crate::auth::xai::auth::write_tokens(&path, &token, "refresh", None).unwrap();
    let account = AccountConfig {
        name: name.to_string(),
        credentials: Some(path.to_string_lossy().into_owned()),
        store_family: Some(StoreFamily::Xai),
        ..Default::default()
    };
    (account, token)
}

// `pool_state` from `pool.rs`'s tests, pointed at the `openai` provider.
fn xai_state(base_url: String) -> AppState {
    let mut config = Config::default();
    config.providers.get_mut("openai").unwrap().base_url = base_url;
    AppState::new(config, reqwest::Client::new()).unwrap()
}

// `pool_route` from `pool.rs`'s tests, with the `openai` provider and a Grok slug.
fn xai_route() -> Route {
    Route {
        provider: "openai".to_string(),
        adapter: AdapterKind::Responses,
        model: "grok-4.6".to_string(),
        upstream_model: "grok-4.6".to_string(),
        effort: None,
        service_tier: None,
    }
}

// `pool_turn` from `pool.rs`'s tests.
fn xai_turn(accounts: Vec<AccountConfig>, stream: bool) -> PoolForward {
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
            response_bounds: crate::adapters::ResponseBounds::default(),
        },
        estimate_input: None,
    }
}

/// A `StoreFamily::Xai` account whose credentials file does not exist.
fn unresolvable_xai_account(dir: &Path, name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        credentials: Some(dir.join("absent.json").to_string_lossy().into_owned()),
        store_family: Some(StoreFamily::Xai),
        ..Default::default()
    }
}

/// Answer `response` only to requests carrying `token` as their bearer, so
/// each pool account gets its own scripted upstream.
async fn mount_for_token(server: &MockServer, token: &str, response: ResponseTemplate) {
    let authorization = format!("Bearer {token}");
    Mock::given(method("POST"))
        .and(header("authorization", authorization.as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body is readable");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn no_upstream_response_message_names_the_pool_family() {
    assert_eq!(
        no_upstream_response_message(AuthMode::XaiOauth),
        "all xAI OAuth accounts failed before receiving an upstream response"
    );
    assert_eq!(
        no_upstream_response_message(AuthMode::ChatgptOauth),
        "all Codex OAuth accounts failed before receiving an upstream response"
    );
}

#[tokio::test]
async fn provider_auth_reads_xai_oauth_for_the_builtin_grok_provider() {
    let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
    let mut grok = xai_route();
    grok.provider = "grok".to_string();
    assert_eq!(provider_auth(&state, &grok), AuthMode::XaiOauth);
    assert_eq!(provider_auth(&state, &xai_route()), AuthMode::ApiKey);
}

#[tokio::test]
async fn xai_exhaustion_without_a_response_uses_the_xai_wording() {
    let error = exhausted_error(None, AuthMode::XaiOauth, None).await;
    let body = body_text(*error.response).await;
    assert!(
        body.contains("all xAI OAuth accounts failed before receiving an upstream response"),
        "{body}"
    );
}

#[tokio::test]
async fn xai_exhaustion_on_a_403_keeps_the_tier_gate_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;
    let upstream = reqwest::Client::new()
        .post(server.uri())
        .send()
        .await
        .expect("mock answers");
    let error = exhausted_error(Some(upstream), AuthMode::XaiOauth, None).await;
    assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
    let body = body_text(*error.response).await;
    assert!(body.contains("XAI_API_KEY"), "{body}");
}

#[tokio::test]
async fn xai_pool_sends_the_selected_accounts_bearer_with_grok_cli_headers() {
    let dir = temp_dir("bearer");
    let (account, token) = xai_account(&dir, "xai-a");
    let server = MockServer::start().await;
    let authorization = format!("Bearer {token}");
    Mock::given(method("POST"))
        .and(header("authorization", authorization.as_str()))
        .and(header("x-xai-token-auth", "xai-grok-cli"))
        .respond_with(ResponseTemplate::new(200).set_body_string(COMPLETED_TURN))
        .mount(&server)
        .await;
    let state = xai_state(server.uri());
    let (status, response) =
        forward_chatgpt_oauth(state, xai_route(), xai_turn(vec![account], false))
            .await
            .expect("the account serves the turn");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "xai-a");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_relays_a_403_from_the_first_account_without_rotating_or_cooling() {
    // D1 (round 2): an inference 403 may be a content-policy refusal, so it is
    // relayed from the first account — never re-sent to the rest of the pool —
    // and the account is not cooled down.
    let dir = temp_dir("relay-403");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(
        &server,
        &token_a,
        ResponseTemplate::new(403).set_body_string("forbidden"),
    )
    .await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let error = forward_chatgpt_oauth(
        state.clone(),
        xai_route(),
        xai_turn(vec![a.clone(), b], false),
    )
    .await
    .expect_err("a 403 is relayed, not rotated past");
    assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(requests.len(), 1, "the second account is never called");
    let expected = format!("Bearer {token_a}");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(expected.as_str()),
        "the one request went to the first account"
    );
    let snapshot = state
        .accounts
        .snapshot("openai", std::slice::from_ref(&a), None, None);
    assert_eq!(
        snapshot[0].cooldown_secs_remaining, None,
        "a relayed 403 sets no cooldown"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_rotates_off_a_401_without_a_same_account_refresh_retry() {
    let dir = temp_dir("rotate-401");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(
        &server,
        &token_a,
        ResponseTemplate::new(401).set_body_string("unauthorized"),
    )
    .await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let (status, response) = forward_chatgpt_oauth(
        state.clone(),
        xai_route(),
        xai_turn(vec![a.clone(), b], false),
    )
    .await
    .expect("the second account serves the turn");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "xai-b");
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(
        requests.len(),
        2,
        "one call to the 401 account, no same-account retry"
    );
    let snapshot = state
        .accounts
        .snapshot("openai", std::slice::from_ref(&a), None, None);
    assert!(
        snapshot[0].cooldown_secs_remaining.is_some(),
        "the 401 account is cooled down"
    );
    // xAI rejected a token that has not expired, so the account's next use
    // refreshes it instead of resending it until its `exp`. The pool does not
    // await the mark (it lands from a blocking task), so wait for it.
    let path_a = PathBuf::from(a.credentials.as_deref().unwrap());
    let mut marked = false;
    for _ in 0..200 {
        if crate::auth::xai::auth::force_refresh_pending(&path_a) {
            marked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(marked, "the 401 account is marked for a forced refresh");
    let path_b = dir.join("xai-b.json");
    assert!(!crate::auth::xai::auth::force_refresh_pending(&path_b));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_cools_an_unresolvable_account_for_five_minutes_and_rotates() {
    let dir = temp_dir("unresolvable");
    let a = unresolvable_xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let (status, response) = forward_chatgpt_oauth(
        state.clone(),
        xai_route(),
        xai_turn(vec![a.clone(), b], false),
    )
    .await
    .expect("the resolvable account serves the turn");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "xai-b");
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(requests.len(), 1, "the unresolvable account sends nothing");
    let snapshot = state
        .accounts
        .snapshot("openai", std::slice::from_ref(&a), None, None);
    match snapshot[0].cooldown_secs_remaining {
        Some(secs) => assert!(secs > 60, "cooldown is about five minutes, got {secs}s"),
        None => panic!("the unresolvable account is cooled down"),
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_429_cools_the_account_for_its_retry_after() {
    let dir = temp_dir("retry-after-429");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(
        &server,
        &token_a,
        ResponseTemplate::new(429).insert_header("retry-after", "120"),
    )
    .await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let (status, response) = forward_chatgpt_oauth(
        state.clone(),
        xai_route(),
        xai_turn(vec![a.clone(), b], false),
    )
    .await
    .expect("the second account serves the turn");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "xai-b");
    let snapshot = state
        .accounts
        .snapshot("openai", std::slice::from_ref(&a), None, None);
    match snapshot[0].cooldown_secs_remaining {
        Some(secs) => assert!(
            (61..=120).contains(&secs),
            "cooldown follows retry-after: 120s, got {secs}s"
        ),
        None => panic!("the 429 account is cooled down"),
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_rotates_off_a_5xx() {
    let dir = temp_dir("rotate-5xx");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(&server, &token_a, ResponseTemplate::new(503)).await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let (status, response) = forward_chatgpt_oauth(state, xai_route(), xai_turn(vec![a, b], false))
        .await
        .expect("the second account serves the turn");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "xai-b");
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(requests.len(), 2, "one call to each account");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_relays_a_400_unread_and_never_as_a_model_refusal() {
    let dir = temp_dir("relay-400");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(
        &server,
        &token_a,
        ResponseTemplate::new(400).set_body_json(json!({
            "detail": "The 'grok-4.6' model is not supported when using Codex with a ChatGPT account."
        })),
    )
    .await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let error = forward_chatgpt_oauth(
        state.clone(),
        xai_route(),
        xai_turn(vec![a.clone(), b], false),
    )
    .await
    .expect_err("a 400 is relayed, not rotated past");
    assert_eq!(error.response.status(), StatusCode::BAD_REQUEST);
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(requests.len(), 1, "the second account is never called");
    let body = body_text(*error.response).await;
    assert!(body.contains("model is not supported"), "{body}");
    let snapshot = state
        .accounts
        .snapshot("openai", std::slice::from_ref(&a), None, None);
    assert_eq!(
        snapshot[0].cooldown_secs_remaining, None,
        "a relayed 400 sets no cooldown"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_exhausted_by_402s_relays_the_last_one() {
    let dir = temp_dir("exhausted-402");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(&server, &token_a, ResponseTemplate::new(402)).await;
    mount_for_token(&server, &token_b, ResponseTemplate::new(402)).await;
    let state = xai_state(server.uri());
    let error = forward_chatgpt_oauth(state, xai_route(), xai_turn(vec![a, b], false))
        .await
        .expect_err("every account returned 402");
    // 402 is outside the client-facing status table in `mapped_error`, so the
    // exhausted pool surfaces as a gateway error.
    assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(requests.len(), 2, "each account was tried once");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xai_pool_streaming_arm_rotates_and_attributes_the_winner() {
    let dir = temp_dir("stream-402");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(&server, &token_a, ResponseTemplate::new(402)).await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let (_, response) = forward_chatgpt_oauth(state, xai_route(), xai_turn(vec![a, b], true))
        .await
        .expect("the second account serves the streamed turn");
    let text = body_text(response).await;
    assert_eq!(text.matches("event: message_start").count(), 1, "{text}");
    assert!(text.contains("event: account"), "{text}");
    assert!(text.contains("xai-b"), "{text}");
    assert!(text.contains("\"text\":\"hi\""), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}
