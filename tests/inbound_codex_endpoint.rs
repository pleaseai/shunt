//! Inbound OpenAI Responses (Codex) endpoint (`[server.codex_endpoint]`, M11) —
//! the raw-passthrough counterpart to `tests/codex_multi_account.rs`.
//!
//! Where the Anthropic Messages path (`/v1/messages`) translates a request into
//! the Responses shape and re-shapes the reply, this endpoint forwards the
//! inbound Responses body upstream **verbatim** and relays the upstream response
//! **verbatim**, reusing only the M10 account-pool machinery. These tests assert
//! that byte-for-byte fidelity plus the pool behaviors that carry over
//! (session-sticky selection, 429 rotation, credential injection, `[server.auth]`
//! gating) and the passthrough-specific exhaustion behavior (the last upstream
//! response is relayed unchanged, not wrapped in an Anthropic error envelope).

use std::{
    fs,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use shunt::{
    config::{AccountConfig, CodexEndpointConfig, Config, InboundAuthConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string, header, method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

/// A raw OpenAI Responses request body, exactly as the Codex CLI would send it —
/// note `input`/`instructions` (Responses shape), not `messages` (Anthropic). It
/// must reach the upstream byte-identical to prove no translation happened.
const INBOUND_BODY: &str = r#"{"model":"gpt-5.6-sol","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":false,"store":false}"#;

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

/// Asserts a header the passthrough must strip never reaches the upstream (e.g.
/// the shunt client-token header).
struct HeaderAbsent(&'static str);

impl Match for HeaderAbsent {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
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

/// A name-only, `token_env`-backed pool entry (Codex accounts carry no `uuid`).
fn account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        ..Default::default()
    }
}

/// A store-backed pool entry whose credential (and refresh) resolve against an
/// explicit `credentials` store file, so the refresh-path tests below avoid the
/// process-global `SHUNT_CODEX_ACCOUNTS_DIR` that codex_multi_account.rs uses.
fn store_account_at(name: &str, credentials: &Path) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        credentials: Some(credentials.to_string_lossy().into_owned()),
        ..Default::default()
    }
}

/// Write a refreshable store account file (`{"auth_mode":"ChatGPT","tokens":
/// {...}}`, read verbatim by `CodexAuthStore`). The far-future access token is
/// used verbatim on the first upstream POST, so an upstream 401 — not a local
/// expiry — drives the refresh path under test.
fn write_store_file(path: &Path, access: &str, refresh: &str) {
    let body = serde_json::json!({
        "auth_mode": "ChatGPT",
        "tokens": { "access_token": access, "refresh_token": refresh }
    });
    fs::write(path, body.to_string()).unwrap();
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-inbound-codex-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Serializes the refresh-path tests, which set the process-global
/// `SHUNT_CODEX_TOKEN_URL` (the refresh endpoint).
static REFRESH_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const FAR_FUTURE_EXP: u64 = 4_102_444_800;

/// Fake ChatGPT access token carrying the `chatgpt_account_id` claim shunt reads.
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

/// A config that opts into the inbound codex endpoint and points the built-in
/// `codex` provider at the mock upstream with the given pool accounts.
fn test_config(upstream_base_url: &str, accounts: Vec<AccountConfig>) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = upstream_base_url.to_string();
    provider.accounts = accounts;
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
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

/// Brute-force a `session-id` that maps to `index` under production's bucket
/// assignment (`accounts::stable_session_index`).
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

/// POST a raw Responses request to one of the inbound codex paths.
async fn post_responses(
    gateway: &TestGateway,
    endpoint_path: &str,
    session_id: Option<&str>,
    client_token: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{}", gateway.base_url, endpoint_path))
        .header("content-type", "application/json")
        // A bogus client credential: the endpoint must inject the pool account's
        // bearer instead of forwarding this one upstream.
        .header("authorization", "Bearer client-would-be-forwarded")
        .body(INBOUND_BODY);
    if let Some(session_id) = session_id {
        request = request.header("session-id", session_id);
    }
    if let Some(token) = client_token {
        request = request.header("x-shunt-token", token);
    }
    request.send().await.unwrap()
}

async fn post_analytics(
    gateway: &TestGateway,
    endpoint_path: &str,
    body: &str,
    client_token: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{}", gateway.base_url, endpoint_path))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(token) = client_token {
        request = request.header("x-shunt-token", token);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn analytics_sink_accepts_upstream_batch_shape() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway_with(test_config("http://127.0.0.1:1", vec![])).await;
    let body = serde_json::json!({
        "events": [
            {"event_type": "codex.turn_started", "event_params": {"private": "discarded"}},
            {"event_type": "codex.turn_completed", "event_params": {}}
        ]
    });

    let response = post_analytics(
        &gateway,
        "/backend-api/codex/analytics-events/events",
        &body.to_string(),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(response.text().await.unwrap(), "{}");
}

#[tokio::test]
async fn analytics_sink_accepts_malformed_json() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway_with(test_config("http://127.0.0.1:1", vec![])).await;

    let response = post_analytics(
        &gateway,
        "/backend-api/codex/analytics-events/events",
        "{not-json",
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "{}");
}

#[tokio::test]
async fn both_analytics_sink_paths_are_registered() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway_with(test_config("http://127.0.0.1:1", vec![])).await;
    let body = r#"{"events":[{"event_type":"codex.turn_started","event_params":{}}]}"#;

    for endpoint_path in [
        "/backend-api/codex/analytics-events/events",
        "/codex/analytics-events/events",
    ] {
        let response = post_analytics(&gateway, endpoint_path, body, None).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "path {endpoint_path} should be routed"
        );
        assert_eq!(response.text().await.unwrap(), "{}");
    }
}

#[tokio::test]
async fn analytics_sink_matches_inbound_auth_posture() {
    if !can_bind_loopback() {
        return;
    }
    let tokens_env = format!("SHUNT_TEST_INBOUND_ANALYTICS_TOKENS_{}", std::process::id());
    std::env::set_var(&tokens_env, "cli:analytics-secret");
    let mut config = test_config("http://127.0.0.1:1", vec![]);
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway_with(config).await;
    let body = r#"{"events":[{"event_type":"codex.turn_started","event_params":{}}]}"#;

    let unauthenticated =
        post_analytics(&gateway, "/codex/analytics-events/events", body, None).await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    // A gateway-owned auth failure on this endpoint must reach the OpenAI
    // Responses client in its own error envelope, not the Anthropic one.
    let missing_token_body: serde_json::Value =
        serde_json::from_str(&unauthenticated.text().await.unwrap()).unwrap();
    assert_openai_error_shape(&missing_token_body, "authentication_error");

    // A wrong (non-empty) token is rejected the same way — auth is not merely a
    // presence check.
    let invalid = post_analytics(
        &gateway,
        "/codex/analytics-events/events",
        body,
        Some("wrong-secret"),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    let invalid_body: serde_json::Value =
        serde_json::from_str(&invalid.text().await.unwrap()).unwrap();
    assert_openai_error_shape(&invalid_body, "authentication_error");

    let authenticated = post_analytics(
        &gateway,
        "/codex/analytics-events/events",
        body,
        Some("analytics-secret"),
    )
    .await;
    assert_eq!(authenticated.status(), StatusCode::OK);
    assert_eq!(authenticated.text().await.unwrap(), "{}");

    std::env::remove_var(&tokens_env);
}

#[tokio::test]
async fn forwards_body_verbatim_and_injects_pool_credential() {
    // The inbound Responses body reaches the upstream byte-identical, carrying the
    // POOL account's bearer (not the client's), and the upstream JSON is relayed
    // back verbatim with its own content-type — proving no Anthropic translation.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-a");
    std::env::set_var("SHUNT_TEST_INBOUND_A", &token_a);

    let upstream_body = r#"{"id":"resp_1","object":"response","status":"completed","output":[]}"#;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .and(body_string(INBOUND_BODY))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream_body, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-a", "SHUNT_TEST_INBOUND_A")],
    ))
    .await;

    let response = post_responses(&gateway, "/backend-api/codex/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    // Relayed verbatim: the raw Responses body, not an Anthropic message.
    assert_eq!(response.text().await.unwrap(), upstream_body);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_A");
}

#[tokio::test]
async fn all_three_inbound_paths_are_registered() {
    // The Codex CLI appends /responses to whatever base_url it is pointed at, so
    // all three forms must reach the same passthrough handler.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-paths");
    std::env::set_var("SHUNT_TEST_INBOUND_PATHS", &token_a);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(3)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-paths", "SHUNT_TEST_INBOUND_PATHS")],
    ))
    .await;

    for endpoint_path in [
        "/backend-api/codex/responses",
        "/responses",
        "/v1/responses",
    ] {
        let response = post_responses(&gateway, endpoint_path, None, None).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "path {endpoint_path} should be routed"
        );
    }
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_PATHS");
}

#[tokio::test]
async fn sse_response_is_relayed_verbatim_without_translation() {
    // A streamed Responses SSE body passes through byte-for-byte: the client sees
    // raw `response.output_text.delta` events, NOT Anthropic `content_block_delta`
    // — the defining difference from the translating /v1/messages path.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-sse");
    std::env::set_var("SHUNT_TEST_INBOUND_SSE", &token_a);

    let sse = "event: response.output_text.delta\n\
               data: {\"delta\":\"raw-passthrough-token\"}\n\n\
               event: response.completed\n\
               data: {\"response\":{\"id\":\"resp_1\"}}\n\n";
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-sse", "SHUNT_TEST_INBOUND_SSE")],
    ))
    .await;

    let response = post_responses(&gateway, "/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = response.text().await.unwrap();
    assert_eq!(body, sse);
    assert!(!body.contains("content_block_delta"));
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_SSE");
}

#[tokio::test]
async fn rotates_on_429_then_relays_last_upstream_verbatim_on_exhaustion() {
    // Every Codex 429 rotates (no PauseSame). When both accounts are exhausted the
    // LAST upstream response is relayed verbatim — status AND body unchanged —
    // rather than re-shaped into an Anthropic rate_limit_error envelope.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-exhaust-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-exhaust-b");
    std::env::set_var("SHUNT_TEST_INBOUND_EXHAUST_A", &token_a);
    std::env::set_var("SHUNT_TEST_INBOUND_EXHAUST_B", &token_b);

    let last_body =
        r#"{"error":{"type":"rate_limit_exceeded","message":"second account exhausted"}}"#;
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
                .insert_header("content-type", "application/json")
                .insert_header("retry-after", "0")
                .set_body_string(last_body),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            account("account-a", "SHUNT_TEST_INBOUND_EXHAUST_A"),
            account("account-b", "SHUNT_TEST_INBOUND_EXHAUST_B"),
        ],
    ))
    .await;

    let response = post_responses(&gateway, "/v1/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // retry-after is preserved so the Codex CLI can back off correctly.
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("0")
    );
    // Verbatim upstream error body — NOT an Anthropic envelope.
    assert_eq!(response.text().await.unwrap(), last_body);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_EXHAUST_A");
    std::env::remove_var("SHUNT_TEST_INBOUND_EXHAUST_B");
}

#[tokio::test]
async fn session_id_header_sticks_to_one_account() {
    // The Codex CLI `session-id` header is the pool sticky key: the same session
    // maps to the same account across requests (SHA-256 bucket assignment).
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-sticky-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-sticky-b");
    std::env::set_var("SHUNT_TEST_INBOUND_STICKY_A", &token_a);
    std::env::set_var("SHUNT_TEST_INBOUND_STICKY_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":"b"}"#))
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            account("account-a", "SHUNT_TEST_INBOUND_STICKY_A"),
            account("account-b", "SHUNT_TEST_INBOUND_STICKY_B"),
        ],
    ))
    .await;

    // A session id that hashes to account-b (index 1); every request with it must
    // land on account-b.
    let session_id = session_id_for_account(1, 2);
    for _ in 0..3 {
        let response = post_responses(&gateway, "/responses", Some(&session_id), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-shunt-account").unwrap(),
            "account-b"
        );
    }

    std::env::remove_var("SHUNT_TEST_INBOUND_STICKY_A");
    std::env::remove_var("SHUNT_TEST_INBOUND_STICKY_B");
}

#[tokio::test]
async fn inbound_auth_gates_the_endpoint() {
    // With [server.auth] configured, the endpoint injects a Codex bearer, so a
    // request without a valid client token is rejected before any upstream call;
    // a request with the token is forwarded.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-auth");
    std::env::set_var("SHUNT_TEST_INBOUND_AUTH", &token_a);
    let tokens_env = format!("SHUNT_TEST_INBOUND_CLIENT_TOKENS_{}", std::process::id());
    std::env::set_var(&tokens_env, "cli:secret-token");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut config = test_config(
        &upstream.uri(),
        vec![account("account-auth", "SHUNT_TEST_INBOUND_AUTH")],
    );
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway_with(config).await;

    // Missing client token → 401, and the upstream is never called.
    let unauth = post_responses(&gateway, "/responses", None, None).await;
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Correct client token → forwarded.
    let authed = post_responses(&gateway, "/responses", None, Some("secret-token")).await;
    assert_eq!(authed.status(), StatusCode::OK);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_AUTH");
    std::env::remove_var(&tokens_env);
}

#[tokio::test]
async fn authorization_bearer_authenticates_the_endpoint() {
    // The OpenAI / LiteLLM / llmgateway idiom: a Codex CLI pointed at shunt with
    // `OPENAI_API_KEY` (or a custom provider's `env_key`) sends the shunt token as
    // `Authorization: Bearer <token>` — no custom header. It authenticates the
    // endpoint, and that client bearer is NOT forwarded upstream (the pool
    // account's bearer is injected instead).
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-bearer");
    std::env::set_var("SHUNT_TEST_INBOUND_BEARER", &token_a);
    let tokens_env = format!("SHUNT_TEST_INBOUND_BEARER_TOKENS_{}", std::process::id());
    std::env::set_var(&tokens_env, "cli:bearer-secret");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        // Upstream sees the POOL account's bearer, never the client's "bearer-secret".
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"ok":true}"#, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut config = test_config(
        &upstream.uri(),
        vec![account("account-bearer", "SHUNT_TEST_INBOUND_BEARER")],
    );
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway_with(config).await;

    // The shunt token presented as an OpenAI-style Bearer key → authenticated.
    let authed = reqwest::Client::new()
        .post(format!("{}/v1/responses", gateway.base_url))
        .header("content-type", "application/json")
        .header("authorization", "Bearer bearer-secret")
        .body(INBOUND_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);

    // A wrong Bearer value → 401 before any upstream call.
    let unauth = reqwest::Client::new()
        .post(format!("{}/v1/responses", gateway.base_url))
        .header("content-type", "application/json")
        .header("authorization", "Bearer wrong-key")
        .body(INBOUND_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_BEARER");
    std::env::remove_var(&tokens_env);
}

#[tokio::test]
async fn forwards_client_identity_headers_verbatim_and_strips_shunt_token() {
    // codex -> shunt -> codex swaps ONLY the credential headers. The Codex CLI's
    // own identity headers reach the backend verbatim — shunt does NOT resynthesize
    // them from a hardcoded `codex_cli_rs/0.144.4` / `responses=experimental`, so a
    // newer client's real version drives model version gating — while the shunt
    // client-token header is stripped and never leaks upstream.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-hdr");
    std::env::set_var("SHUNT_TEST_INBOUND_HDR", &token_a);
    let tokens_env = format!("SHUNT_TEST_INBOUND_HDR_TOKENS_{}", std::process::id());
    std::env::set_var(&tokens_env, "cli:hdr-secret");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        // Pool account's bearer is injected (not the client's bogus one).
        .and(BearerToken(token_a.clone()))
        // Client identity headers forwarded verbatim — NOT shunt's hardcoded ones.
        .and(header("version", "0.999.0"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("user-agent", "codex_cli_rs/0.999.0"))
        .and(header("openai-beta", "responses=custom-99"))
        .and(header("x-codex-window-id", "win-xyz:7"))
        .and(header("session-id", "sess-verbatim"))
        // The shunt client token must never reach the backend.
        .and(HeaderAbsent("x-shunt-token"))
        // A client-supplied internal client-identity label must be stripped, not
        // forwarded (spoofing guard, matches the main proxy path).
        .and(HeaderAbsent("x-shunt-inbound-client"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"ok":true}"#, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut config = test_config(
        &upstream.uri(),
        vec![account("account-hdr", "SHUNT_TEST_INBOUND_HDR")],
    );
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", gateway.base_url))
        .header("content-type", "application/json")
        // A bogus client credential that must be stripped, not forwarded.
        .header("authorization", "Bearer client-would-be-forwarded")
        .header("version", "0.999.0")
        .header("originator", "codex_cli_rs")
        .header("user-agent", "codex_cli_rs/0.999.0")
        .header("openai-beta", "responses=custom-99")
        .header("x-codex-window-id", "win-xyz:7")
        .header("session-id", "sess-verbatim")
        .header("x-shunt-token", "hdr-secret")
        // A spoofed internal client label the passthrough must strip.
        .header("x-shunt-inbound-client", "spoofed-client")
        .body(INBOUND_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_HDR");
    std::env::remove_var(&tokens_env);
}

#[tokio::test]
async fn strips_default_shunt_token_even_without_inbound_auth() {
    // The documented guarantee — the shunt client-token header never reaches the
    // Codex backend — holds unconditionally, including on an ungated endpoint (no
    // `[server.auth]`). A client that habitually sends `x-shunt-token` must not
    // have it forwarded upstream just because auth is off.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-open");
    std::env::set_var("SHUNT_TEST_INBOUND_OPEN", &token_a);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        // The default shunt token header is stripped even with no auth configured.
        .and(HeaderAbsent("x-shunt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"ok":true}"#, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    // No `config.server.auth` — the endpoint is open.
    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-open", "SHUNT_TEST_INBOUND_OPEN")],
    ))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/responses", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", "leftover-token")
        .body(INBOUND_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_OPEN");
}

#[tokio::test]
async fn upstream_response_headers_are_relayed_verbatim() {
    // The relay preserves upstream response headers the Codex CLI relies on —
    // `x-codex-turn-state` (turn continuity) and observability ids — not just
    // content-type/retry-after.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-resp-hdr");
    std::env::set_var("SHUNT_TEST_INBOUND_RESP_HDR", &token_a);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "turn-state-abc")
                .insert_header("x-request-id", "req-xyz")
                // An upstream/edge session cookie must NOT be relayed to the client
                // — it is bound to shunt's server-side egress.
                .insert_header("set-cookie", "cf_clearance=egress-secret; Path=/")
                .set_body_raw(r#"{"ok":true}"#, "application/json"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-resp-hdr", "SHUNT_TEST_INBOUND_RESP_HDR")],
    ))
    .await;

    let response = post_responses(&gateway, "/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok()),
        Some("turn-state-abc")
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("req-xyz")
    );
    // The upstream session cookie is stripped, never leaked to the inbound client.
    assert!(
        response.headers().get("set-cookie").is_none(),
        "upstream set-cookie must not be relayed to the client"
    );
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_RESP_HDR");
}

#[tokio::test]
async fn token_env_401_cools_down_and_rotates_to_next_account() {
    // A 401 classifies as RefreshRetry. A `token_env` account's bearer is used
    // verbatim and cannot be refreshed, so the account is cooled down and the turn
    // rotates to the next pooled account, whose success is relayed verbatim. This
    // exercises the RefreshRetry failover arm the 429-only tests never reach.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-401-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-401-b");
    std::env::set_var("SHUNT_TEST_INBOUND_401_A", &token_a);
    std::env::set_var("SHUNT_TEST_INBOUND_401_B", &token_b);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_a.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"ok":"b"}"#, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            account("account-a", "SHUNT_TEST_INBOUND_401_A"),
            account("account-b", "SHUNT_TEST_INBOUND_401_B"),
        ],
    ))
    .await;

    // A session id that maps to account-a (index 0) so the 401 account is tried
    // first, then the turn rotates to account-b.
    let session_id = session_id_for_account(0, 2);
    let response = post_responses(&gateway, "/responses", Some(&session_id), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    assert_eq!(response.text().await.unwrap(), r#"{"ok":"b"}"#);
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_401_A");
    std::env::remove_var("SHUNT_TEST_INBOUND_401_B");
}

#[tokio::test]
async fn single_credential_fallback_when_no_accounts_configured() {
    // With no [[accounts]] configured and an empty store, forward_codex_inbound
    // falls back to the single default `~/.codex/auth.json` ($CODEX_AUTH_FILE)
    // credential — no pool, no failover, and no `x-shunt-account` header — and
    // still forwards the body and relays the upstream response verbatim.
    if !can_bind_loopback() {
        return;
    }
    // Point the store scan at an empty dir so the accounts list resolves empty
    // (taking the single-credential branch), and the default credential at a temp
    // auth file carrying a valid ChatGPT token.
    let unique = format!("{}-{}", std::process::id(), FAR_FUTURE_EXP);
    let accounts_dir = std::env::temp_dir().join(format!("shunt-inbound-single-accts-{unique}"));
    std::fs::create_dir_all(&accounts_dir).unwrap();
    let auth_file = std::env::temp_dir().join(format!("shunt-inbound-single-auth-{unique}.json"));
    let token = chatgpt_token(FAR_FUTURE_EXP, "acct-single");
    std::fs::write(
        &auth_file,
        serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "access_token": token,
                "refresh_token": "refresh-single",
                "account_id": "acct-single"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);
    std::env::set_var("CODEX_AUTH_FILE", &auth_file);

    let upstream_body =
        r#"{"id":"resp_single","object":"response","status":"completed","output":[]}"#;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token.clone()))
        .and(body_string(INBOUND_BODY))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream_body, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(&upstream.uri(), vec![])).await;

    let response = post_responses(&gateway, "/v1/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    // The single-credential path does not attach x-shunt-account (no pool).
    assert!(response.headers().get("x-shunt-account").is_none());
    assert_eq!(response.text().await.unwrap(), upstream_body);
    upstream.verify().await;

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("CODEX_AUTH_FILE");
    let _ = std::fs::remove_dir_all(&accounts_dir);
    let _ = std::fs::remove_file(&auth_file);
}

#[tokio::test]
async fn endpoint_is_absent_without_opt_in_config() {
    // Without [server.codex_endpoint] the routes are not registered at all — the
    // default HTTP surface is unchanged.
    if !can_bind_loopback() {
        return;
    }
    let mut config = Config::default();
    // A loopback codex base_url keeps the config valid without a real backend.
    config.providers.get_mut("codex").unwrap().base_url = "http://127.0.0.1:1".to_string();
    let gateway = start_gateway_with(config).await;

    for endpoint_path in [
        "/backend-api/codex/responses",
        "/responses",
        "/v1/responses",
        "/backend-api/codex/analytics-events/events",
        "/codex/analytics-events/events",
    ] {
        let response = post_responses(&gateway, endpoint_path, None, None).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "path {endpoint_path} must not exist without opt-in"
        );
    }
}

#[tokio::test]
async fn refresh_retry_refreshes_then_relays_verbatim() {
    // A refreshable store account whose upstream returns 401 forces a token
    // refresh; the retry with the refreshed token relays the upstream body
    // verbatim — the passthrough counterpart to codex_multi_account.rs's
    // refresh_retry_refreshes_then_succeeds_on_401.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    // `stale` and `fresh` must differ so the BearerToken matchers can tell the
    // pre-refresh and post-refresh upstream requests apart.
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-refresh");
    let fresh = chatgpt_token(FAR_FUTURE_EXP + 1, "acct-refresh");

    let dir = unique_temp_dir("refresh-ok");
    let store_path = dir.join("account-a.json");
    write_store_file(&store_path, &stale, "refresh-token-a");

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

    let upstream_body = r#"{"id":"resp_1","object":"response","status":"completed","output":[]}"#;
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
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream_body, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![store_account_at("account-a", &store_path)],
    ))
    .await;

    let response = post_responses(&gateway, "/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-a"
    );
    // Relayed verbatim after the refresh — the raw Responses body, not an
    // Anthropic message.
    assert_eq!(response.text().await.unwrap(), upstream_body);
    upstream.verify().await;
    auth.verify().await;

    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn refresh_failure_cools_down_and_rotates_to_next_account() {
    // When the refresh itself fails (token endpoint 5xx), the account is cooled
    // down and the pool rotates to the next account, whose response is relayed
    // verbatim.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-refresh-fail-a");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-served-b");
    std::env::set_var("SHUNT_TEST_INBOUND_REFRESH_FAIL_B", &token_b);

    let dir = unique_temp_dir("refresh-fail");
    let store_path = dir.join("account-a.json");
    write_store_file(&store_path, &stale, "refresh-token-a");

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("refresh boom"))
        .mount(&auth)
        .await;
    std::env::set_var("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let served = r#"{"id":"resp_b","object":"response","status":"completed","output":[]}"#;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired token"}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(served, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    // Pin the store account first so its refresh-failure rotation is exercised.
    let session = session_id_for_account(0, 2);
    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            store_account_at("account-a", &store_path),
            account("account-b", "SHUNT_TEST_INBOUND_REFRESH_FAIL_B"),
        ],
    ))
    .await;

    let response = post_responses(&gateway, "/responses", Some(&session), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    assert_eq!(response.text().await.unwrap(), served);
    upstream.verify().await;

    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_INBOUND_REFRESH_FAIL_B");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn refresh_retry_still_unauthorized_rotates_to_next_account() {
    // Refresh succeeds but the refreshed token is still rejected with 401: the
    // account is cooled down and the pool rotates rather than relaying the second
    // 401 — the passthrough counterpart to codex_multi_account.rs's
    // refresh_retry_still_unauthorized_cools_down_and_rotates.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-still401-stale");
    let fresh = chatgpt_token(FAR_FUTURE_EXP + 1, "acct-still401-fresh");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-still401-b");
    std::env::set_var("SHUNT_TEST_INBOUND_STILL401_B", &token_b);

    let dir = unique_temp_dir("still401");
    let store_path = dir.join("account-a.json");
    write_store_file(&store_path, &stale, "refresh-token-a");

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .mount(&auth)
        .await;
    std::env::set_var("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let served = r#"{"id":"resp_b","object":"response","status":"completed","output":[]}"#;
    let upstream = MockServer::start().await;
    // account-a is rejected 401 both before and after the refresh.
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired"}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"still revoked"}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(served, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let session = session_id_for_account(0, 2);
    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            store_account_at("account-a", &store_path),
            account("account-b", "SHUNT_TEST_INBOUND_STILL401_B"),
        ],
    ))
    .await;

    let response = post_responses(&gateway, "/responses", Some(&session), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    assert_eq!(response.text().await.unwrap(), served);
    upstream.verify().await;

    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_INBOUND_STILL401_B");
    fs::remove_dir_all(&dir).ok();
}

/// Assert a gateway-owned error body is OpenAI Responses-shaped
/// (`{"error":{"message","type","code":null}}`) and NOT the Anthropic envelope
/// (`{"type":"error","error":{...}}`) — the distinction issue #127 turns on.
fn assert_openai_error_shape(body: &serde_json::Value, expected_type: &str) {
    // Anthropic errors carry a top-level `"type":"error"`; the OpenAI shape does not.
    assert!(
        body.get("type").is_none(),
        "expected OpenAI shape without a top-level `type`, got {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "expected a non-empty error.message, got {body}"
    );
    assert_eq!(body["error"]["type"], expected_type);
    // `Value` indexing returns `Null` for a missing key, so require `code` to be
    // present AND null — otherwise a regression that dropped the field entirely
    // (e.g. `skip_serializing_if`) would still pass.
    assert!(
        body["error"]
            .get("code")
            .is_some_and(serde_json::Value::is_null),
        "expected error.code to be present and null, got {body}"
    );
}

#[tokio::test]
async fn gateway_owned_401_body_is_openai_shaped() {
    // A gateway-owned auth failure (bad/missing client token) must reach an OpenAI
    // Responses client in its own error envelope, not the Anthropic one, so the
    // Codex CLI surfaces a meaningful message. Status stays 401.
    if !can_bind_loopback() {
        return;
    }
    let token_a = chatgpt_token(FAR_FUTURE_EXP, "acct-401-shape");
    std::env::set_var("SHUNT_TEST_INBOUND_401_SHAPE", &token_a);
    let tokens_env = format!("SHUNT_TEST_INBOUND_401_SHAPE_TOKENS_{}", std::process::id());
    std::env::set_var(&tokens_env, "cli:secret-token");

    // The upstream is never contacted — auth fails first — so no mock is needed.
    let upstream = MockServer::start().await;
    let mut config = test_config(
        &upstream.uri(),
        vec![account("account-401-shape", "SHUNT_TEST_INBOUND_401_SHAPE")],
    );
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway_with(config).await;

    // No client token → 401 before any upstream call.
    let response = post_responses(&gateway, "/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_openai_error_shape(&body, "authentication_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("client token"),
        "expected the auth failure message, got {body}"
    );

    std::env::remove_var("SHUNT_TEST_INBOUND_401_SHAPE");
    std::env::remove_var(&tokens_env);
}

#[tokio::test]
async fn gateway_owned_502_body_is_openai_shaped() {
    // When every pooled account fails to resolve *before* any upstream response,
    // there is no upstream body to relay, so shunt returns its own 502 — which must
    // be OpenAI-shaped on this endpoint, not the Anthropic envelope.
    if !can_bind_loopback() {
        return;
    }
    // A single account whose `token_env` is deliberately unset: resolution fails,
    // the account cools down, and with no other account the pool is exhausted with
    // no upstream ever contacted → the gateway-owned 502.
    let missing_env = format!("SHUNT_TEST_INBOUND_502_MISSING_{}", std::process::id());
    std::env::remove_var(&missing_env);

    // Resolution fails before any upstream call, so base_url is never used; point
    // it at an unreachable loopback address as a backstop, so no real request could
    // escape even if that assumption regressed.
    let gateway = start_gateway_with(test_config(
        "http://127.0.0.1:1",
        vec![account("account-502-shape", &missing_env)],
    ))
    .await;

    let response = post_responses(&gateway, "/v1/responses", None, None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_openai_error_shape(&body, "api_error");
    assert_eq!(
        body["error"]["message"],
        "all Codex OAuth accounts failed before receiving an upstream response"
    );
}

#[tokio::test]
async fn refresh_retry_non_success_rotates_to_next_account() {
    // Refresh succeeds but the refreshed retry fails with a non-401 non-2xx (5xx):
    // classify_codex maps it to Rotate, so the account cools down and the pool
    // moves to the next account.
    if !can_bind_loopback() {
        return;
    }
    let _env = REFRESH_ENV_LOCK.lock().await;
    let stale = chatgpt_token(FAR_FUTURE_EXP, "acct-retry5xx-stale");
    let fresh = chatgpt_token(FAR_FUTURE_EXP + 1, "acct-retry5xx-fresh");
    let token_b = chatgpt_token(FAR_FUTURE_EXP, "acct-retry5xx-b");
    std::env::set_var("SHUNT_TEST_INBOUND_RETRY5XX_B", &token_b);

    let dir = unique_temp_dir("retry5xx");
    let store_path = dir.join("account-a.json");
    write_store_file(&store_path, &stale, "refresh-token-a");

    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"access_token":"{fresh}","refresh_token":"refresh-token-a-2"}}"#
        )))
        .mount(&auth)
        .await;
    std::env::set_var("SHUNT_CODEX_TOKEN_URL", format!("{}/token", auth.uri()));

    let served = r#"{"id":"resp_b","object":"response","status":"completed","output":[]}"#;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(stale.clone()))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"expired"}"#))
        .mount(&upstream)
        .await;
    // The refreshed retry gets a 5xx → Rotate (not a relayable status).
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(fresh.clone()))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"upstream down"}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token_b.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(served, "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;

    let session = session_id_for_account(0, 2);
    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            store_account_at("account-a", &store_path),
            account("account-b", "SHUNT_TEST_INBOUND_RETRY5XX_B"),
        ],
    ))
    .await;

    let response = post_responses(&gateway, "/responses", Some(&session), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-b"
    );
    assert_eq!(response.text().await.unwrap(), served);
    upstream.verify().await;

    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_INBOUND_RETRY5XX_B");
    fs::remove_dir_all(&dir).ok();
}
