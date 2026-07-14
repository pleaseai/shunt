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

use std::{io::ErrorKind, net::SocketAddr};

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
        credentials: None,
        token_env: Some(token_env.to_string()),
        uuid: None,
    }
}

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
    let (app, _shared) = server::build_router(config).unwrap();
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
    // them from a hardcoded `codex_cli_rs/0.144.1` / `responses=experimental`, so a
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
        .and(header("openai-beta", "responses=custom-99"))
        .and(header("x-codex-window-id", "win-xyz:7"))
        .and(header("session-id", "sess-verbatim"))
        // The shunt client token must never reach the backend.
        .and(HeaderAbsent("x-shunt-token"))
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
        .header("openai-beta", "responses=custom-99")
        .header("x-codex-window-id", "win-xyz:7")
        .header("session-id", "sess-verbatim")
        .header("x-shunt-token", "hdr-secret")
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
    upstream.verify().await;

    std::env::remove_var("SHUNT_TEST_INBOUND_RESP_HDR");
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
    ] {
        let response = post_responses(&gateway, endpoint_path, None, None).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "path {endpoint_path} must not exist without opt-in"
        );
    }
}
