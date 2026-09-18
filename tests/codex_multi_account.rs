//! Codex/ChatGPT account-pool failover (M10) — mirrors `tests/multi_account.rs`
//! (the Anthropic account pool) for the `codex` provider's Responses adapter.
//!
//! Two behaviors set this suite apart from the Anthropic one, both driven by
//! `accounts::classify_codex` (see `src/accounts.rs`):
//!
//! - Codex 429 responses carry no per-response retry-after signal that maps to
//!   `PauseSame`, so every 429 rotates to the next account — there is no
//!   `PauseSame` sub-case, and no analog of the Anthropic suite's
//!   `plain_429_retries_the_same_account_...` or
//!   `pause_same_retry_succeeds_and_relays_...` tests. (Since issue #195 the
//!   recorded `x-codex-*` quota headers do feed proactive selection ordering —
//!   see `codex_quota_headers_drive_proactive_rotation` below — they are just
//!   not a same-account pause signal.)
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
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use shunt::{
    config::{AccountConfig, Config, PoolConfig, RouteConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

mod common;

struct BearerToken(String);

struct LogWriter {
    output: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reprobe_log_subscriber(output: &Arc<Mutex<Vec<u8>>>) -> impl tracing::Subscriber + Send + Sync {
    let writer_output = Arc::clone(output);
    tracing_subscriber::fmt()
        .with_writer(move || LogWriter {
            output: Arc::clone(&writer_output),
        })
        .with_ansi(false)
        .without_time()
        .finish()
}

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
    /// The router's pool state, so a test can read health verdicts (such as
    /// `needs_relogin`) that the wire response does not carry.
    state: server::AppState,
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

fn future_exp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_add(3_600)
}

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
        service_tier: None,
    });
    config
}

fn write_stale_pool_state(path: &std::path::Path, account_id: &str) {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 61;
    let state = serde_json::json!({
        "version": 2,
        "accounts": [{
            "key": {
                "store_family": "chatgpt",
                "identity": {"kind": "verified", "id": account_id}
            },
            "quota": {
                "utilization_5h": 0.9,
                "observed_at_5h": observed_at
            }
        }]
    });
    fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
}

fn test_config_with_pool_state(
    upstream_base_url: &str,
    first: AccountConfig,
    second: AccountConfig,
    state_path: PathBuf,
) -> Config {
    let mut config = test_config(upstream_base_url, first, second);
    config.server.pool = Some(PoolConfig {
        default_threshold: Some(0.5),
        reprobe_seconds: Some(60),
        state_path: Some(state_path),
        ..Default::default()
    });
    config
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    shunt::state_persist::restore(&state).await;
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestGateway {
        base_url: format!("http://{addr}"),
        state,
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

/// Find `message_start`'s `usage.input_tokens` in a gateway SSE response body,
/// mirroring the identically-named helper in `tests/codex_websocket_fallback.rs`
/// (integration test binaries do not share code across files, so this is
/// intentionally duplicated rather than imported).
fn message_start_input_tokens(sse: &str) -> u64 {
    for line in sse.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if value["type"] == "message_start" {
            return value["message"]["usage"]["input_tokens"]
                .as_u64()
                .expect("message_start usage.input_tokens must be an integer");
        }
    }
    panic!("no message_start event found in gateway SSE:\n{sse}");
}

async fn post_messages(gateway: &TestGateway, session_id: Option<&str>) -> reqwest::Response {
    post_messages_with_content(gateway, session_id, "hi").await
}

async fn post_messages_with_content(
    gateway: &TestGateway,
    session_id: Option<&str>,
    content: &str,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "pooled-codex-model",
                "max_tokens": 16,
                "stream": false,
                "messages": [{"role": "user", "content": content}]
            })
            .to_string(),
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
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_UNAUTH_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_UNAUTH_B", &token_b);

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
}

#[tokio::test]
async fn refresh_retry_refreshes_then_succeeds_on_401() {
    // A refreshable store account whose upstream returns 401 forces a token
    // refresh; the retry with the refreshed token then succeeds.
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers below can
    // tell the pre-refresh and post-refresh requests apart.
    let expires_at = future_exp();
    let stale = chatgpt_token(expires_at, "acct-a");
    let fresh = chatgpt_token(expires_at + 1, "acct-a");

    let accounts_dir = unique_temp_dir("succeeds");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

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

    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn refresh_retry_non_success_rotates_to_next_account() {
    // If the refreshed retry still fails with a non-401/non-2xx status (5xx),
    // the pool must fail over to the next account instead of relaying it.
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers below can
    // tell the pre-refresh and post-refresh requests apart.
    let expires_at = future_exp();
    let stale = chatgpt_token(expires_at, "acct-a");
    let fresh = chatgpt_token(expires_at + 1, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-rotate-b");
    vars.set("SHUNT_TEST_CODEX_ROTATE_B", &token_b);

    let accounts_dir = unique_temp_dir("rotates");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

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

    fs::remove_dir_all(&accounts_dir).ok();
}

/// Issue #251: the initial account attempt, its 401 refresh retry, and the
/// next-account attempt must all receive the byte-identical translated body.
#[tokio::test]
async fn refresh_retry_and_rotation_reuse_the_identical_serialized_body() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let expires_at = future_exp();
    let stale = chatgpt_token(expires_at, "acct-body-a");
    let fresh = chatgpt_token(expires_at + 1, "acct-body-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-body-b");
    vars.set("SHUNT_TEST_CODEX_BODY_B", &token_b);

    let accounts_dir = unique_temp_dir("identical-body");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

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
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"account a throttled"}"#),
        )
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
        account("account-b", "SHUNT_TEST_CODEX_BODY_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    let response_body = response.text().await.unwrap();
    assert!(response_body.contains("account b served"));
    upstream.verify().await;
    auth.verify().await;

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let body = requests[0].body.as_slice();
    assert!(!body.is_empty());
    assert_eq!(requests[1].body.as_slice(), body);
    assert_eq!(requests[2].body.as_slice(), body);
    let translated: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(translated["model"], "pooled-codex-model");
    assert_eq!(translated["input"][0]["content"][0]["text"], "hi");

    fs::remove_dir_all(&accounts_dir).ok();
}

/// Issue #285's compressed sibling to `refresh_retry_and_rotation_reuse_the_
/// identical_serialized_body` above: once the serialized body clears the 1 KiB
/// compression floor, the initial account attempt, its 401 refresh retry, and
/// the next-account attempt must all carry `content-encoding: zstd` and reuse
/// the identical compressed bytes — `prepare_body`'s memoization must hold
/// whether or not compression ran.
#[tokio::test]
async fn refresh_retry_and_rotation_reuse_the_identical_compressed_body() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    let expires_at = future_exp();
    let stale = chatgpt_token(expires_at, "acct-zbody-a");
    let fresh = chatgpt_token(expires_at + 1, "acct-zbody-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-zbody-b");
    vars.set("SHUNT_TEST_CODEX_ZBODY_B", &token_b);

    let accounts_dir = unique_temp_dir("identical-zstd-body");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

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
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string(r#"{"error":"account a throttled"}"#),
        )
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
        account("account-b", "SHUNT_TEST_CODEX_ZBODY_B"),
    ))
    .await;

    // A long conversational turn: comfortably over the 1 KiB compression floor
    // once wrapped in the Responses request shape, and repetitive enough that
    // zstd actually shrinks it (proving the header is not sent unconditionally).
    let large_content = "the quick brown fox reviews a diff and updates the schema; ".repeat(40);
    assert!(large_content.len() > 1024);

    let response = post_messages_with_content(&gateway, None, &large_content).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    let response_body = response.text().await.unwrap();
    assert!(response_body.contains("account b served"));
    upstream.verify().await;
    auth.verify().await;

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(
            request
                .headers
                .get("content-encoding")
                .and_then(|value| value.to_str().ok()),
            Some("zstd"),
            "every attempt must announce the compressed body it actually sent"
        );
    }
    let body = requests[0].body.as_slice();
    assert!(!body.is_empty());
    assert_eq!(
        requests[1].body.as_slice(),
        body,
        "the refresh retry must reuse the identical compressed bytes"
    );
    assert_eq!(
        requests[2].body.as_slice(),
        body,
        "the rotated account must reuse the identical compressed bytes"
    );

    let decoded = zstd::stream::decode_all(body).expect("upstream body must be valid zstd");
    let translated: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(translated["model"], "pooled-codex-model");
    assert_eq!(translated["input"][0]["content"][0]["text"], large_content);

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
    let mut vars = common::env_lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers below can
    // tell the pre-refresh and post-refresh requests apart.
    let expires_at = future_exp();
    let stale = chatgpt_token(expires_at, "acct-a");
    let fresh = chatgpt_token(expires_at + 1, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-still401-b");
    vars.set("SHUNT_TEST_CODEX_STILL401_B", &token_b);

    let accounts_dir = unique_temp_dir("still401");
    write_store_account(&accounts_dir, "account-a", &stale, "refresh-token-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

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
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-resolve-b");
    let mut vars = common::env_lock().await;
    // Unset is this test's precondition, not cleanup: the account is
    // unresolvable *because* the name is absent. It has to hold under the
    // guard, which also restores it afterwards.
    vars.unset("SHUNT_TEST_CODEX_MISSING_A");
    vars.set("SHUNT_TEST_CODEX_RESOLVE_B", &token_b);

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
}

#[tokio::test]
async fn all_accounts_unresolvable_returns_bad_gateway() {
    // When every account fails to resolve, the pool never reaches an
    // upstream: the proxy exhausts its one-element chain and synthesizes the
    // contract's 502 in shunt's own error envelope; the upstream is never called.
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.unset("SHUNT_TEST_CODEX_MISSING_ALL_A");
    vars.unset("SHUNT_TEST_CODEX_MISSING_ALL_B");

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
        "all upstreams failed (1 attempted)"
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
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_SERVER_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_SERVER_B", &token_b);

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
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_THROTTLE_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_THROTTLE_B", &token_b);

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
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_EXHAUST_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_EXHAUST_B", &token_b);

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
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_WSPOOL_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_WSPOOL_B", &token_b);

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
    // Regression for the account-pool path silently dropping the message_start
    // tiktoken estimate (#112 only threaded it through the single-account
    // forward_http/forward_websocket paths, not forward_chatgpt_oauth): after a
    // pre-stream websocket failure falls back to HTTP for this SAME account,
    // message_start must still carry a nonzero estimate.
    assert!(
        message_start_input_tokens(&body) > 0,
        "message_start must carry the tiktoken estimate on the pool ws->http fallback path; got:\n{body}"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn pool_http_dispatch_seeds_message_start_input_token_estimate() {
    // Regression for the account-pool path bypassing the message_start
    // tiktoken estimate #112 added (see `relay_success` in
    // `src/adapters/responses/pool.rs`, which used to hardcode `0`): a
    // streaming turn relayed through the plain HTTP pool dispatch (no
    // websocket involved) must seed a nonzero `usage.input_tokens` in
    // `message_start`, exactly like the single-account
    // `forward_http`/`forward_websocket` paths already do (see
    // `tests/passthrough.rs::message_start_seeds_tiktoken_estimate_for_streaming_responses_model`).
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-estimate-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-estimate-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_ESTIMATE_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_ESTIMATE_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("pool http streamed")))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_ESTIMATE_A"),
        account("account-b", "SHUNT_TEST_CODEX_ESTIMATE_B"),
    ))
    .await;

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
    // The streaming pool path commits the response (and thus its headers)
    // before the winning account is known, so `x-shunt-account` cannot ride
    // the header — the winner is attributed with an `event: account` frame
    // before its first relayed frame instead.
    assert!(
        response.headers().get("x-shunt-account").is_none(),
        "early-committed streaming responses cannot carry the winning account header"
    );
    let body = response.text().await.unwrap();
    assert!(
        message_start_input_tokens(&body) > 0,
        "message_start must carry the tiktoken estimate on the pool HTTP dispatch path; got:\n{body}"
    );
    // The winning account's attribution frame precedes its relayed content.
    let data_index = body
        .lines()
        .position(|line| line.trim() == "event: account")
        .expect("the stream must attribute the winning account");
    let data_line = body.lines().nth(data_index + 1).unwrap();
    assert_eq!(data_line, "data: \"account-a\"");
    let content_index = body
        .lines()
        .position(|line| line.starts_with("data: ") && line.contains("pool http streamed"))
        .unwrap();
    assert!(
        data_index < content_index,
        "the account frame must precede the relayed content; got:\n{body}"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn pool_rotation_still_seeds_input_token_estimate_after_401() {
    // Same regression as `pool_http_dispatch_seeds_message_start_input_token_estimate`,
    // but exercising the lazily-spawned estimate handle across an account
    // rotation (pool.rs's `estimate_handle`, mirroring the existing `http_body`
    // lazy-serialize-once pattern): account-a's 401 rotates to account-b
    // without a refresh (a `token_env` account has nothing to refresh), and
    // the SAME shared estimate handle spawned before account-a's request must
    // still be the one consumed when account-b's retry succeeds.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-estimate-rot-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-estimate-rot-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_ESTIMATE_ROT_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_ESTIMATE_ROT_B", &token_b);

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
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b streamed")))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_ESTIMATE_ROT_A"),
        account("account-b", "SHUNT_TEST_CODEX_ESTIMATE_ROT_B"),
    ))
    .await;

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
    // The early-committed streaming response cannot carry the winning account
    // header (see `pool_http_dispatch_seeds_message_start_input_token_estimate`).
    assert!(
        response.headers().get("x-shunt-account").is_none(),
        "early-committed streaming responses cannot carry the winning account header"
    );
    let body = response.text().await.unwrap();
    assert!(
        message_start_input_tokens(&body) > 0,
        "message_start must carry the tiktoken estimate after a pool account rotation; got:\n{body}"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn codex_quota_headers_drive_proactive_rotation() {
    // Issue #195: recorded `x-codex-*` quota windows feed `select_order`, so a
    // sticky account whose observed 5h utilization crosses the threshold is
    // rotated off proactively — before it ever returns a 429.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-quota-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-quota-b");
    let reset_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_add(16_200);
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_QUOTA_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_QUOTA_B", &token_b);

    let upstream = MockServer::start().await;
    // Account-a succeeds but reports its 5h window at 99% used (>= the legacy
    // 0.98 near-quota threshold), with a far-future reset so the recorded
    // window is not expired away before the next selection.
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-primary-window-minutes", "300")
                .insert_header("x-codex-primary-used-percent", "99")
                .insert_header("x-codex-primary-reset-at", reset_at.to_string().as_str())
                .set_body_string(sse_body("account a served")),
        )
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
        account("account-a", "SHUNT_TEST_CODEX_QUOTA_A"),
        account("account-b", "SHUNT_TEST_CODEX_QUOTA_B"),
    ))
    .await;

    // Both requests carry a session id that hashes to account-a, so absent the
    // quota signal the pool would stay sticky on account-a for both.
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );

    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b",
        "a near-quota sticky account should be rotated off proactively"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn storm_control_spills_concurrent_request_to_next_account() {
    // Issue #195 storm control: with `ramp_initial_concurrency = 1`, a second
    // concurrent request for the same sticky account is denied admission and
    // spills to the next account instead of piling onto the first.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-storm-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-storm-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_STORM_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_STORM_B", &token_b);

    let upstream = MockServer::start().await;
    // Account-a's turn is slow, so it is still in flight (holding its single
    // admission slot) when the second request arrives.
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(500))
                .set_body_string(sse_body("account a served")),
        )
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

    let mut config = test_config(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_STORM_A"),
        account("account-b", "SHUNT_TEST_CODEX_STORM_B"),
    );
    config.server.pool = Some(shunt::config::PoolConfig {
        ramp_initial_concurrency: Some(1),
        ..Default::default()
    });
    let gateway = start_gateway_with(config).await;

    let session_id = session_id_for_account(0, 2);
    let first = {
        let gateway_url = gateway.base_url.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{gateway_url}/v1/messages"))
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", session_id)
                .body(
                    r#"{"model":"pooled-codex-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
                )
                .send()
                .await
                .unwrap()
        })
    };
    // Give the first request time to reach the upstream and occupy the slot.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let second = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second.headers().get("x-shunt-account").unwrap(),
        "account-b",
        "a gated concurrent request should spill to the next account"
    );

    let first = first.await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers().get("x-shunt-account").unwrap(), "account-a");
    upstream.verify().await;
}

#[tokio::test]
async fn storm_control_last_candidate_is_always_admitted() {
    // The gate must defer, never fail: a pool whose only (and therefore last)
    // candidate is at its admission cap still serves every concurrent request.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-storm-solo");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_STORM_SOLO", &token_a);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(300))
                .set_body_string(sse_body("solo account served")),
        )
        .expect(2)
        .mount(&upstream)
        .await;

    let mut config = Config::default();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = upstream.uri();
    provider.accounts = vec![account("account-solo", "SHUNT_TEST_CODEX_STORM_SOLO")];
    config.routes.push(RouteConfig {
        model: "pooled-codex-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
        service_tier: None,
    });
    config.server.pool = Some(shunt::config::PoolConfig {
        ramp_initial_concurrency: Some(1),
        ..Default::default()
    });
    let gateway = start_gateway_with(config).await;

    let first = {
        let gateway_url = gateway.base_url.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{gateway_url}/v1/messages"))
                .header("content-type", "application/json")
                .body(
                    r#"{"model":"pooled-codex-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
                )
                .send()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The solo account is over its allowance of 1, but as the last remaining
    // candidate it is force-admitted rather than exhausting the pool.
    let second = post_messages(&gateway, None).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second.headers().get("x-shunt-account").unwrap(),
        "account-solo"
    );

    let first = first.await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn codex_quota_rotation_with_empty_reset_header() {
    // Reproduces the incident this change fixes: a deployed multi-account
    // codex pool sent a valid window-minutes group with near-quota
    // utilization but a blank `x-codex-primary-reset-at`. Proactive rotation
    // must still trigger off the utilization alone — a missing reset must not
    // suppress the recorded quota signal. (Re-entry once the mark ages out is
    // covered by the unit tests `account_reenters_selection_after_reset_passes`
    // and `account_reenters_selection_after_reset_less_mark_ages_out` in
    // src/accounts.rs, not here, to avoid a sleep-based flaky wait in this
    // integration test.)
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-quota-noreset-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-quota-noreset-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_QUOTA_NORESET_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_QUOTA_NORESET_B", &token_b);

    let upstream = MockServer::start().await;
    // Account-a succeeds but reports its 5h window at 99% used with an empty
    // reset-at header — no reset instant is ever recorded for this window.
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-primary-window-minutes", "300")
                .insert_header("x-codex-primary-used-percent", "99")
                .insert_header("x-codex-primary-reset-at", "")
                .set_body_string(sse_body("account a served")),
        )
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
        account("account-a", "SHUNT_TEST_CODEX_QUOTA_NORESET_A"),
        account("account-b", "SHUNT_TEST_CODEX_QUOTA_NORESET_B"),
    ))
    .await;

    // Both requests carry a session id that hashes to account-a, so absent the
    // quota signal the pool would stay sticky on account-a for both.
    let session_id = session_id_for_account(0, 2);
    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );

    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b",
        "a near-quota sticky account with an empty reset header should still rotate off"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn restored_stale_quota_reprobes_once_then_uses_healthy_account() {
    // A v2 snapshot keys the stale quota by the verified ChatGPT account id,
    // which is resolved from each token_env JWT before pool selection. The first
    // request must therefore probe account-a even though the state predates this
    // process, while the next request in the 60-second interval must prefer the
    // healthy account-b. Neither response carries quota headers, so the second
    // selection proves the dispatch stamp, rather than fresh upstream quota, is
    // what suppresses another probe.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-restored-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-restored-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_RESTORED_A", &token_a);
    vars.set("SHUNT_TEST_CODEX_RESTORED_B", &token_b);

    let state_dir = unique_temp_dir("restored-stale");
    let state_path = state_dir.join("pool-state.json");
    write_stale_pool_state(&state_path, "acct-restored-a");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account a served")))
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

    let gateway = start_gateway_with(test_config_with_pool_state(
        &upstream.uri(),
        account("account-a", "SHUNT_TEST_CODEX_RESTORED_A"),
        account("account-b", "SHUNT_TEST_CODEX_RESTORED_B"),
        state_path,
    ))
    .await;
    let session_id = session_id_for_account(0, 2);
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = reprobe_log_subscriber(&logs);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a",
        "the restored stale account is promoted for its first live probe"
    );
    assert!(response.text().await.unwrap().contains("account a served"));

    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b",
        "a second request inside the reprobe interval must use the healthy account"
    );
    assert!(response.text().await.unwrap().contains("account b served"));
    drop(_subscriber_guard);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("opportunistically re-probing a stale near-quota account")
            .count(),
        1,
        "only the first dispatched request is recorded as a probe: {logs}"
    );
    upstream.verify().await;

    fs::remove_dir_all(state_dir).ok();
}

#[tokio::test]
async fn missing_stale_probe_token_cancels_before_healthy_fallback() {
    // Credential resolution fails before the upstream dispatch boundary. The
    // stale account's reservation must be cancelled, and only the healthy
    // fallback may reach the upstream.
    if !can_bind_loopback() {
        return;
    }
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-missing-fallback-b");
    let mut vars = common::env_lock().await;
    // Unset is this test's precondition, not cleanup: the account is
    // unresolvable *because* the name is absent. It has to hold under the
    // guard, which also restores it afterwards.
    vars.unset("SHUNT_TEST_CODEX_MISSING_A");
    vars.set("SHUNT_TEST_CODEX_MISSING_B", &token_b);

    let state_dir = unique_temp_dir("missing-stale-token");
    let state_path = state_dir.join("pool-state.json");
    write_stale_pool_state(&state_path, "acct-missing-fallback-a");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut stale = account("account-a", "SHUNT_TEST_CODEX_MISSING_A");
    stale.uuid = Some("acct-missing-fallback-a".to_string());
    let gateway = start_gateway_with(test_config_with_pool_state(
        &upstream.uri(),
        stale,
        account("account-b", "SHUNT_TEST_CODEX_MISSING_B"),
        state_path,
    ))
    .await;
    let session_id = session_id_for_account(0, 2);
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = reprobe_log_subscriber(&logs);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let response = post_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b",
        "a missing stale credential must fall back to the healthy account"
    );
    assert!(response.text().await.unwrap().contains("account b served"));
    drop(_subscriber_guard);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("opportunistically re-probing a stale near-quota account")
            .count(),
        0,
        "a credential failure before dispatch must not record a probe: {logs}"
    );
    upstream.verify().await;

    fs::remove_dir_all(state_dir).ok();
}

// ---------------------------------------------------------------------------
// Streaming (early-commit) arm of the responses pool: `stream: true` requests
// with the Codex websocket path disabled drive `pool_events_stream` instead
// of the buffered loop, covering the relay / rotate / refresh / exhaustion
// branches of the committed-stream loop (PR #549).
// ---------------------------------------------------------------------------

/// POST /v1/messages with `stream: true` so the responses adapter takes the
/// early-commit streaming arm (`pool_events_stream`) instead of the buffered
/// loop.
async fn post_streaming_messages(
    gateway: &TestGateway,
    session_id: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "pooled-codex-model",
                "max_tokens": 16,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        );
    if let Some(session_id) = session_id {
        request = request.header("x-claude-code-session-id", session_id);
    }
    request.send().await.unwrap()
}

/// A Responses SSE success fixture for one account's bearer token.
fn sse_ok_mock(token: &str, text: &str) -> Mock {
    Mock::given(BearerToken(token.to_string()))
        .and(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body(text).into_bytes(), "text/event-stream"),
        )
}

/// A bare status-code fixture for one account's bearer token.
fn status_mock(token: &str, status: u16) -> Mock {
    Mock::given(BearerToken(token.to_string()))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(status))
}

#[tokio::test]
async fn streaming_relays_synthetic_start_and_upstream_events() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let _env = common::set_env(&[
        ("SHUNT_CODEX_STREAM_A", token_a.as_str()),
        ("SHUNT_CODEX_STREAM_B", token_b.as_str()),
    ])
    .await;
    let upstream = MockServer::start().await;
    sse_ok_mock(&token_a, "hello from a")
        .expect(1)
        .mount(&upstream)
        .await;
    sse_ok_mock(&token_b, "hello from b")
        .expect(0)
        .mount(&upstream)
        .await;
    let config = test_config(
        &upstream.uri(),
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    let gateway = start_gateway_with(config).await;
    let session_id = session_id_for_account(0, 2);
    let response = post_streaming_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(
        message_start_input_tokens(&body) > 0,
        "message_start must carry the tiktoken estimate; got:\n{body}"
    );
    assert!(body.contains("hello from a"), "body: {body}");
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_429_rotates_to_second_account() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let _env = common::set_env(&[
        ("SHUNT_CODEX_STREAM_A", token_a.as_str()),
        ("SHUNT_CODEX_STREAM_B", token_b.as_str()),
    ])
    .await;
    let upstream = MockServer::start().await;
    // The 429 answers once; a pool that re-requests account a past the
    // rotation reaches the success fixture instead, so a's content would
    // appear in the relayed stream and trip the assertion below.
    status_mock(&token_a, 429)
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&upstream)
        .await;
    sse_ok_mock(&token_a, "hello from a")
        .with_priority(2)
        .expect(0)
        .mount(&upstream)
        .await;
    sse_ok_mock(&token_b, "hello from b")
        .expect(1)
        .mount(&upstream)
        .await;
    let config = test_config(
        &upstream.uri(),
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    let gateway = start_gateway_with(config).await;
    let session_id = session_id_for_account(0, 2);
    let response = post_streaming_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(
        message_start_input_tokens(&body) > 0,
        "message_start must carry the tiktoken estimate; got:\n{body}"
    );
    assert!(body.contains("hello from b"), "body: {body}");
    assert!(
        !body.contains("hello from a"),
        "the rotated-away account must not serve the stream; body: {body}"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_exhausted_pool_emits_error_envelope() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let _env = common::set_env(&[
        ("SHUNT_CODEX_STREAM_A", token_a.as_str()),
        ("SHUNT_CODEX_STREAM_B", token_b.as_str()),
    ])
    .await;
    let upstream = MockServer::start().await;
    status_mock(&token_a, 429).expect(1).mount(&upstream).await;
    status_mock(&token_b, 429).expect(1).mount(&upstream).await;
    let config = test_config(
        &upstream.uri(),
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    let gateway = start_gateway_with(config).await;
    let response = post_streaming_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"type\":\"error\""), "body: {body}");
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_transport_failures_exhaust_pool_with_envelope() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let _env = common::set_env(&[
        ("SHUNT_CODEX_STREAM_A", token_a.as_str()),
        ("SHUNT_CODEX_STREAM_B", token_b.as_str()),
    ])
    .await;
    // Port 0 never accepts a connection, so every upstream send fails at
    // the transport layer deterministically — a released ephemeral port
    // could be rebound by a sibling test.
    let mut config = test_config(
        "http://127.0.0.1:0",
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    config.providers.get_mut("codex").unwrap().retry.max_retries = 0;
    let gateway = start_gateway_with(config).await;
    let response = post_streaming_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"type\":\"error\""), "body: {body}");
}

#[tokio::test]
async fn streaming_non_failover_4xx_emits_error_envelope() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let _env = common::set_env(&[
        ("SHUNT_CODEX_STREAM_A", token_a.as_str()),
        ("SHUNT_CODEX_STREAM_B", token_b.as_str()),
    ])
    .await;
    let upstream = MockServer::start().await;
    status_mock(&token_a, 400).expect(1).mount(&upstream).await;
    sse_ok_mock(&token_b, "hello from b")
        .expect(0)
        .mount(&upstream)
        .await;
    let config = test_config(
        &upstream.uri(),
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    let gateway = start_gateway_with(config).await;
    let session_id = session_id_for_account(0, 2);
    let response = post_streaming_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"type\":\"error\""), "body: {body}");
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_admission_failure_moves_to_next_account() {
    if !can_bind_loopback() {
        return;
    }
    // Account a's token env is unset: its admission/resolution fails, the
    // loop cancels its reprobe reservation and moves on to account b. The
    // unset is this test's precondition, not cleanup — it must hold under the
    // env guard even when the host already defines the variable.
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let mut vars = common::env_lock().await;
    vars.unset("SHUNT_CODEX_STREAM_A");
    vars.set("SHUNT_CODEX_STREAM_B", &token_b);
    let upstream = MockServer::start().await;
    sse_ok_mock(&token_b, "hello from b").mount(&upstream).await;
    let config = test_config(
        &upstream.uri(),
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    let gateway = start_gateway_with(config).await;
    let session_id = session_id_for_account(0, 2);
    let response = post_streaming_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("hello from b"), "body: {body}");
}

#[tokio::test]
async fn streaming_failed_refresh_cools_down_and_moves_on() {
    if !can_bind_loopback() {
        return;
    }
    let dir = unique_temp_dir("stream-refresh-fail");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", dir.to_str().unwrap());
    let access_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let access_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    write_store_account(
        &dir,
        "stream-refresh-a",
        &access_a,
        &chatgpt_token(FAR_FUTURE_EXP, "acct-a"),
    );
    write_store_account(
        &dir,
        "stream-refresh-b",
        &access_b,
        &chatgpt_token(FAR_FUTURE_EXP, "acct-b"),
    );
    let upstream = MockServer::start().await;
    // The failed refresh must hit the mock, never the real ChatGPT token
    // endpoint.
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", upstream.uri()));
    status_mock(&access_a, 401).mount(&upstream).await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_string_contains("refresh_token"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&upstream)
        .await;
    sse_ok_mock(&access_b, "hello from b")
        .mount(&upstream)
        .await;
    let config = test_config(
        &upstream.uri(),
        store_account("stream-refresh-a"),
        store_account("stream-refresh-b"),
    );
    let gateway = start_gateway_with(config).await;
    let session_id = session_id_for_account(0, 2);
    let response = post_streaming_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("hello from b"), "body: {body}");
    drop(gateway);
    // The store dir is a unique temp dir: clean it up like every other
    // temp-dir user in this file so CI runs don't accumulate
    // `shunt-codex-multi-*` dirs.
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn streaming_ttfb_timeout_emits_error_envelope() {
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    let _env = common::set_env(&[
        ("SHUNT_CODEX_STREAM_A", token_a.as_str()),
        ("SHUNT_CODEX_STREAM_B", token_b.as_str()),
    ])
    .await;
    let upstream = MockServer::start().await;
    Mock::given(BearerToken(token_a.clone()))
        .and(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body("too late").into_bytes(), "text/event-stream")
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&upstream)
        .await;
    let mut config = test_config(
        &upstream.uri(),
        account("stream-a", "SHUNT_CODEX_STREAM_A"),
        account("stream-b", "SHUNT_CODEX_STREAM_B"),
    );
    config.server.timeouts.upstream_ttfb_ms = 500;
    config.providers.get_mut("codex").unwrap().retry.max_retries = 0;
    let gateway = start_gateway_with(config).await;
    let session_id = session_id_for_account(0, 2);
    let response = post_streaming_messages(&gateway, Some(&session_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"type\":\"error\""), "body: {body}");
}

/// A store account whose access token expired long ago, so the pool has to
/// refresh it on read: the resolution path, not the upstream-401 path, is
/// what carries the token endpoint's verdict here (#616).
fn write_expired_store_account(dir: &std::path::Path, name: &str, account_id: &str) {
    let expired = chatgpt_token(1_000_000, account_id);
    write_store_account(dir, name, &expired, &format!("refresh-token-{name}"));
}

#[tokio::test]
async fn terminal_refresh_rejection_marks_needs_relogin() {
    // The token endpoint rejects the stored refresh grant outright
    // (`invalid_grant`): no retry can recover it, so besides the cooldown and
    // the rotation the account must be marked `needs_relogin` — otherwise it
    // cycles through the five-minute cooldown forever, reported as live.
    if !can_bind_loopback() {
        return;
    }
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-terminal-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_TERMINAL_B", &token_b);

    let accounts_dir = unique_temp_dir("terminal");
    write_expired_store_account(&accounts_dir, "account-a", "acct-terminal-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid_grant"}"#))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(1)
        .mount(&upstream)
        .await;

    let account_a = store_account("account-a");
    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account_a.clone(),
        account("account-b", "SHUNT_TEST_CODEX_TERMINAL_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    assert!(
        gateway.state.accounts.needs_relogin("codex", &account_a),
        "an invalid_grant refresh rejection must mark the account needs_relogin"
    );
    upstream.verify().await;
    auth.verify().await;

    fs::remove_dir_all(&accounts_dir).ok();
}

#[tokio::test]
async fn transient_refresh_failure_does_not_mark_needs_relogin() {
    // A 503 from the token endpoint says nothing about the grant: the account
    // is cooled down and the pool rotates, but a healthy account must not be
    // reported as dead on a momentary provider blip.
    if !can_bind_loopback() {
        return;
    }
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-transient-b");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_CODEX_TRANSIENT_B", &token_b);

    let accounts_dir = unique_temp_dir("transient");
    write_expired_store_account(&accounts_dir, "account-a", "acct-transient-a");
    vars.set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .expect(1)
        .mount(&auth)
        .await;
    vars.set("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("account b served")))
        .expect(1)
        .mount(&upstream)
        .await;

    let account_a = store_account("account-a");
    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        account_a.clone(),
        account("account-b", "SHUNT_TEST_CODEX_TRANSIENT_B"),
    ))
    .await;

    let response = post_messages(&gateway, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    assert!(
        !gateway.state.accounts.needs_relogin("codex", &account_a),
        "a transient token-endpoint failure must only cool the account down"
    );
    upstream.verify().await;
    auth.verify().await;

    fs::remove_dir_all(&accounts_dir).ok();
}
