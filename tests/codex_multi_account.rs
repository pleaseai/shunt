//! Codex/ChatGPT account-pool failover (M10) — mirrors `tests/multi_account.rs`
//! (the Anthropic account pool) for the `codex` provider's Responses adapter.
//!
//! Two behaviors set this suite apart from the Anthropic one, both driven by
//! `accounts::classify_codex` (see `src/accounts.rs`):
//!
//! - Codex quota/rejection headers are display-only, so every 429 rotates to
//!   the next account — there is no `PauseSame` sub-case, and no analog of the
//!   Anthropic suite's `plain_429_retries_the_same_account_...`
//!   or `pause_same_retry_succeeds_and_relays_...` tests.
//! - A `token_env` (static) account has no store-file "setup token" marker
//!   concept: the `RefreshRetry` check is `account.token_env.is_some()` only,
//!   so there is no analog of `static_setup_token_account_cools_down_...`.
//!
//! The other cross-cutting difference is response shape: the Responses
//! adapter always parses the upstream body as an SSE event stream (see
//! `json_response`/`AnthropicSseMachine` in `src/adapters/responses/mod.rs`)
//! regardless of the client's `stream` preference, and a failed turn is
//! re-shaped into an Anthropic-style error envelope rather than relayed
//! verbatim — unlike the Anthropic adapter's raw passthrough. Every success
//! fixture here is SSE-formatted (`sse_body`), and the exhausted-pool test
//! asserts the translated envelope rather than a byte-identical upstream body.

use std::{
    fs,
    io::ErrorKind,
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use shunt::{
    config::{AccountConfig, Config, RouteConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

struct BearerToken(String);

impl Match for BearerToken {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {}", self.0).as_str())
    }
}

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A name-only, `token_env`-backed pool entry. Codex accounts carry no `uuid`
/// concept (the account id lives inside the ChatGPT access token, not the
/// pool entry), unlike the Anthropic `account()` helper this mirrors.
fn account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        ..Default::default()
    }
}

/// A name-only pool entry that resolves against the shunt account store
/// (`SHUNT_CODEX_ACCOUNTS_DIR/<name>.json`).
fn store_account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        ..Default::default()
    }
}

/// Serializes the refresh-path tests, which set the process-global
/// `SHUNT_CODEX_ACCOUNTS_DIR` / `SHUNT_CODEX_TOKEN_URL` env vars.
static REFRESH_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn unique_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-codex-multi-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A far-future expiry (year 2100) for tokens that must read as locally valid.
const FAR_FUTURE_EXP: u64 = 4_102_444_800;

/// Build a fake ChatGPT access token carrying the `chatgpt_account_id` claim
/// `codex::auth::jwt_account_id` reads (mirrors the `token()` helper in
/// `src/auth/codex/auth.rs`'s own test module). A far-future `exp` keeps a
/// store account's initial token "locally valid" so `get_valid_chatgpt` uses
/// it verbatim on the first POST — the upstream 401 (not a local expiry
/// check) is what drives the `RefreshRetry` path under test, and the
/// refreshed token must carry this same claim since `RefreshResponse::
/// to_credential` reads the account id from the JWT only, with no stored
/// `account_id` field to fall back on.
fn chatgpt_token(exp: u64, account_id: &str) -> String {
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// Write a refreshable store account file (`{"auth_mode":"ChatGPT","tokens":
/// {...}}`, copied verbatim by `CodexAuthStore` — see `src/auth/codex/
/// store.rs`) whose access token is valid far into the future, so it is used
/// verbatim on the first upstream POST rather than being refreshed on read.
fn write_store_account(dir: &std::path::Path, name: &str, access: &str, refresh: &str) {
    let body = serde_json::json!({
        "auth_mode": "ChatGPT",
        "tokens": {
            "access_token": access,
            "refresh_token": refresh
        }
    });
    fs::write(dir.join(format!("{name}.json")), body.to_string()).unwrap();
}

/// A minimal Responses SSE stream carrying `text` as the assistant's message
/// content (mirrors `codex_websocket_fallback.rs`'s `RESPONSES_SSE`).
/// `json_response` always parses the upstream body as SSE regardless of the
/// client's `stream` preference, so every success fixture in this suite must
/// be shaped this way rather than as a bare JSON object.
fn sse_body(text: &str) -> String {
    format!(
        "event: response.created\n\
         data: {{\"response\":{{\"id\":\"resp_1\",\"usage\":{{\"output_tokens\":0}}}}}}\n\n\
         event: response.output_item.added\n\
         data: {{\"item\":{{\"type\":\"message\"}}}}\n\n\
         event: response.output_text.delta\n\
         data: {{\"delta\":\"{text}\"}}\n\n\
         event: response.output_text.done\n\
         data: {{}}\n\n\
         event: response.completed\n\
         data: {{\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":4}}}}}}\n\n\
         data: [DONE]\n\n"
    )
}

fn test_config(upstream_base_url: &str, first: AccountConfig, second: AccountConfig) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = upstream_base_url.to_string();
    provider.accounts = vec![first, second];
    config.routes.push(RouteConfig {
        model: "pooled-codex-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
    });
    config
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

/// Brute-force a session id that maps to `index` under the SAME bucket
/// assignment production uses (`accounts::stable_session_index`): the first 8
/// bytes of `SHA-256(session_id)` as a big-endian u64, mod the account count.
/// Hashing with `DefaultHasher` here would pick an id that lands on a different
/// account under the real SHA-256 algorithm, so the per-test "hashes to
/// account-a" comments would not actually hold.
fn session_id_for_account(index: usize, account_count: usize) -> String {
    (0..1000)
        .map(|candidate| format!("session-{candidate}"))
        .find(|session_id| {
            let digest = Sha256::digest(session_id.as_bytes());
            let prefix = u64::from_be_bytes(digest[..8].try_into().unwrap());
            (prefix % account_count as u64) as usize == index
        })
        .expect("a session id should map to the requested account")
}

async fn post_messages(gateway: &TestGateway, session_id: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"pooled-codex-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        );
    if let Some(session_id) = session_id {
        request = request.header("x-claude-code-session-id", session_id);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn token_env_401_cools_down_and_rotates_without_refresh() {
    // A 401 classifies as RefreshRetry, but a token_env (static) account has
    // no store file to refresh — the check short-circuits to a cooldown and
    // rotation, with no attempt to reach a refresh endpoint at all.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-unauth-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-unauth-b");
    std::env::set_var("SHUNT_TEST_CODEX_UNAUTH_A", &token_a);
    std::env::set_var("SHUNT_TEST_CODEX_UNAUTH_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":"account a token revoked"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(2)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_UNAUTH_A"),
        account("account-b", "SHUNT_TEST_CODEX_UNAUTH_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("account b served"));

    // A session that hashes to account-a still lands on account-b because
    // account-a is now cooled down (so the upstream never sees a second call).
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_CODEX_UNAUTH_A");
    std::env::remove_var("SHUNT_TEST_CODEX_UNAUTH_B");
}

#[tokio::test]
async fn refresh_retry_refreshes_then_succeeds_on_401() {
    // A refreshable store account whose upstream returns 401 forces a token
    // refresh; the retry with the refreshed token then succeeds.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers below can
    // tell the pre-refresh and post-refresh requests apart.
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let fresh = chatgpt_token(FAR_FUTURE_EXP + 1, "acct-a");

    let accounts_dir = unique_temp_dir("succeeds");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(fresh.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_body("account a served after refresh")),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_CODEX_REFRESH_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("account a served after refresh"));
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn refresh_retry_non_success_rotates_to_next_account() {
    // If the refreshed retry still fails with a non-401/non-2xx status (5xx),
    // the pool must fail over to the next account instead of relaying it.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers below can
    // tell the pre-refresh and post-refresh requests apart.
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let fresh = chatgpt_token(FAR_FUTURE_EXP + 1, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-rotate-b");
    std::env::set_var("SHUNT_TEST_CODEX_ROTATE_B", &token_b);

    let accounts_dir = unique_temp_dir("rotates");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"upstream down"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_CODEX_ROTATE_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_CODEX_ROTATE_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn refresh_retry_still_unauthorized_cools_down_and_rotates() {
    // Refresh succeeds but the refreshed token is still rejected with 401: the
    // account is genuinely broken, so it is cooled down and the pool rotates
    // rather than relaying the second 401 to the client.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers below can
    // tell the pre-refresh and post-refresh requests apart.
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let fresh = chatgpt_token(FAR_FUTURE_EXP + 1, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-still401-b");
    std::env::set_var("SHUNT_TEST_CODEX_STILL401_B", &token_b);

    let accounts_dir = unique_temp_dir("still401");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    std::env::set_var("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"still revoked"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        store_account("account-a"),
        account("account-b", "SHUNT_TEST_CODEX_STILL401_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_CODEX_STILL401_B");
    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn unresolvable_account_cools_down_and_rotates() {
    // An account whose token_env is unset cannot be resolved: the pool must
    // cool it down and rotate to the next account rather than failing the
    // request.
    if !can_bind_loopback() {
        return;
    }
    std::env::remove_var("SHUNT_TEST_CODEX_MISSING_A");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-resolve-b");
    std::env::set_var("SHUNT_TEST_CODEX_RESOLVE_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_MISSING_A"),
        account("account-b", "SHUNT_TEST_CODEX_RESOLVE_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_CODEX_RESOLVE_B");
}

#[tokio::test]
async fn all_accounts_unresolvable_returns_bad_gateway() {
    // When every account fails to resolve, the pool never reaches an
    // upstream: it surfaces a 502 in shunt's own error envelope and the
    // upstream is never called.
    if !can_bind_loopback() {
        return;
    }
    std::env::remove_var("SHUNT_TEST_CODEX_MISSING_ALL_A");
    std::env::remove_var("SHUNT_TEST_CODEX_MISSING_ALL_B");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"unexpected":true}"#))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_MISSING_ALL_A"),
        account("account-b", "SHUNT_TEST_CODEX_MISSING_ALL_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let text = response.text().await.unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(
        body["error"]["message"],
        "all Codex OAuth accounts failed before receiving an upstream response"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn server_error_rotates_and_cools_down_the_failing_account() {
    // A 5xx classifies as Rotate: the account is cooled down for the fixed
    // non-throttle window and the pool moves to the next one.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-server-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-server-b");
    std::env::set_var("SHUNT_TEST_CODEX_SERVER_A", &token_a);
    std::env::set_var("SHUNT_TEST_CODEX_SERVER_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(500).set_body_string(r#"{"error":"account a upstream error"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(2)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_SERVER_A"),
        account("account-b", "SHUNT_TEST_CODEX_SERVER_B"),
    ))
    .await;

    // First request rotates off the 500'd account to the healthy one.
    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    // A session that hashes to account-a still lands on account-b because
    // account-a is cooled down (the upstream never sees a second a call).
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_CODEX_SERVER_A");
    std::env::remove_var("SHUNT_TEST_CODEX_SERVER_B");
}

#[tokio::test]
async fn a_429_always_rotates_unlike_the_anthropic_pause_same_case() {
    // Codex has no per-account quota-rejection header, so classify_codex
    // rotates on every 429 rather than pausing/retrying the same account (the
    // Anthropic pool's behavior for a "plain" 429). One rejection is enough to
    // permanently cool account-a down for this request and the next.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-throttle-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-throttle-b");
    std::env::set_var("SHUNT_TEST_CODEX_THROTTLE_A", &token_a);
    std::env::set_var("SHUNT_TEST_CODEX_THROTTLE_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"temporary throttle on account a"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(2)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_THROTTLE_A"),
        account("account-b", "SHUNT_TEST_CODEX_THROTTLE_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );

    // Even a session that sticks to account-a lands on account-b, because a
    // Codex 429 always rotates rather than pausing and retrying in place.
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_CODEX_THROTTLE_A");
    std::env::remove_var("SHUNT_TEST_CODEX_THROTTLE_B");
}

#[tokio::test]
async fn exhausted_pool_relays_translated_error_envelope() {
    // Unlike the Anthropic adapter's byte-verbatim relay, the Responses
    // adapter always re-shapes an upstream failure into an Anthropic-style
    // error envelope (see build_upstream_error). When every account is
    // exhausted, the last account's failure is surfaced that way rather than
    // passed through unchanged.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-exhaust-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-exhaust-b");
    std::env::set_var("SHUNT_TEST_CODEX_EXHAUST_A", &token_a);
    std::env::set_var("SHUNT_TEST_CODEX_EXHAUST_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"first account exhausted"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"second account exhausted"}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_EXHAUST_A"),
        account("account-b", "SHUNT_TEST_CODEX_EXHAUST_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let text = response.text().await.unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": "second account exhausted"
            }
        })
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_CODEX_EXHAUST_A");
    std::env::remove_var("SHUNT_TEST_CODEX_EXHAUST_B");
}

#[tokio::test]
async fn websocket_enabled_pool_falls_back_to_http_and_streams() {
    // Opting a pooled account into the websocket transport must still build the
    // per-account `ForwardOptions` (see `forward_chatgpt_oauth`'s `if ws_enabled`
    // arm) before attempting it. The mock upstream here speaks HTTP only, so
    // account-a's websocket handshake fails and the pool falls back to HTTP for
    // that SAME account — exactly the single-account pattern already proven by
    // `codex_websocket_fallback.rs::websocket_handshake_failure_falls_back_to_http`
    // — and because the client asks for `stream:true`, the resulting success is
    // relayed over `relay_success`'s streaming branch (`stream_response`) rather
    // than its collected-JSON one.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-wspool-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-wspool-b");
    std::env::set_var("SHUNT_TEST_CODEX_WSPOOL_A", &token_a);
    std::env::set_var("SHUNT_TEST_CODEX_WSPOOL_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("ws pool streamed")))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut config = test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_WSPOOL_A"),
        account("account-b", "SHUNT_TEST_CODEX_WSPOOL_B"),
    );
    // Opt in to the ws transport (mirrors tests/codex_websocket_fallback.rs) —
    // the mock upstream has no websocket endpoint, so account-a's ws attempt
    // fails its handshake and falls back to HTTP for the same account without
    // ever reaching account-b.
    config.providers.get_mut("codex").unwrap().websocket = true;
    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"pooled-codex-model","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "a streaming client request must relay over the SSE streaming branch; got content-type: {content_type}"
    );
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("ws pool streamed"),
        "the streamed body should carry the upstream's translated text; got: {body}"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_CODEX_WSPOOL_A");
    std::env::remove_var("SHUNT_TEST_CODEX_WSPOOL_B");
}
