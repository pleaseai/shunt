//! The Codex CLI's built-in web-search tool on the inbound Codex endpoint
//! (`[server.codex_endpoint]`) — the companion to
//! `tests/inbound_codex_endpoint.rs`, which covers Responses turns.
//!
//! The CLI posts its `web.run` tool to `{base_url}/alpha/search`. These tests
//! pin that every base-URL form reaches the backend's `/codex/alpha/search`
//! over the same account pool as a turn (credential swap, verbatim body and
//! reply, failover, `[server.auth]`), that a search never follows a
//! `[[server.codex_endpoint.routes]]` model route, and that the route is
//! HTTP-only.

use std::{io::ErrorKind, net::SocketAddr};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, CodexEndpointConfig, CodexRouteConfig, Config, InboundAuthConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string, header, method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

mod common;

/// A Codex CLI `web.run` request body. Its exact shape is the CLI's business:
/// shunt relays it verbatim, so the tests only assert byte fidelity. It names a
/// model so the routing test can show that a model route is never followed.
const SEARCH_BODY: &str = r#"{"model":"gpt-5.6-sol","search_query":[{"q":"national cad standard sheet organization"}],"response_length":"short"}"#;

const SEARCH_PATHS: [&str; 3] = [
    "/backend-api/codex/alpha/search",
    "/alpha/search",
    "/v1/alpha/search",
];

const FAR_FUTURE_EXP: u64 = 4_102_444_800;

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

fn account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        ..Default::default()
    }
}

/// Fake ChatGPT access token carrying the `chatgpt_account_id` claim shunt reads.
fn chatgpt_token(account_id: &str) -> String {
    let payload = serde_json::json!({
        "exp": FAR_FUTURE_EXP,
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

async fn post_search(
    gateway: &TestGateway,
    endpoint_path: &str,
    client_token: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{}", gateway.base_url, endpoint_path))
        .header("content-type", "application/json")
        .header("authorization", "Bearer client-would-be-forwarded")
        .body(SEARCH_BODY);
    if let Some(token) = client_token {
        request = request.header("x-shunt-token", token);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn every_search_path_relays_to_the_backend_search_route_over_the_pool() {
    // Every base-URL form must reach the backend's `/codex/alpha/search` with
    // the POOL account's bearer and account id, the body verbatim, and the
    // upstream reply relayed unchanged — never the Responses route.
    if !can_bind_loopback() {
        return;
    }
    let token = chatgpt_token("acct-search");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_SEARCH_POOL", &token);

    let upstream_body = r#"{"results":[{"url":"https://example.com","title":"t"}]}"#;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .and(BearerToken(token.clone()))
        .and(header("chatgpt-account-id", "acct-search"))
        .and(body_string(SEARCH_BODY))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream_body, "application/json"))
        .expect(3)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-search", "SHUNT_TEST_SEARCH_POOL")],
    ))
    .await;

    for endpoint_path in SEARCH_PATHS {
        let response = post_search(&gateway, endpoint_path, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{endpoint_path}");
        assert_eq!(
            response.headers().get("x-shunt-account").unwrap(),
            "account-search",
            "{endpoint_path}"
        );
        assert_eq!(response.text().await.unwrap(), upstream_body);
    }
    upstream.verify().await;
}

#[tokio::test]
async fn search_is_gated_by_inbound_auth_with_an_openai_shaped_error() {
    // A search injects a pool credential exactly like a turn, so `[server.auth]`
    // gates it the same way: no client token, no upstream call, and the
    // rejection uses the OpenAI error envelope the Codex CLI parses. The shunt
    // token itself is never forwarded.
    if !can_bind_loopback() {
        return;
    }
    let token = chatgpt_token("acct-search-auth");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_SEARCH_AUTH_POOL", &token);
    let tokens_env = format!("SHUNT_TEST_SEARCH_CLIENT_TOKENS_{}", std::process::id());
    vars.set(&tokens_env, "cli:secret-token");

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .and(HeaderAbsent("x-shunt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut config = test_config(
        &upstream.uri(),
        vec![account(
            "account-search-auth",
            "SHUNT_TEST_SEARCH_AUTH_POOL",
        )],
    );
    config.server.auth = Some(InboundAuthConfig {
        jwt: Vec::new(),
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway_with(config).await;

    let unauthenticated = post_search(&gateway, "/v1/alpha/search", None).await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = unauthenticated.json().await.unwrap();
    assert!(body["error"].is_object(), "{body}");
    assert!(
        body.get("type").is_none(),
        "Anthropic envelope leaked: {body}"
    );

    let authenticated = post_search(&gateway, "/v1/alpha/search", Some("secret-token")).await;
    assert_eq!(authenticated.status(), StatusCode::OK);
    upstream.verify().await;
}

#[tokio::test]
async fn a_rate_limited_account_fails_over_to_the_next_one() {
    // The relay reuses the pool's failover: a 429 from one account rotates the
    // same search to the next account instead of failing the tool call.
    if !can_bind_loopback() {
        return;
    }
    let limited = chatgpt_token("acct-limited");
    let healthy = chatgpt_token("acct-healthy");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_SEARCH_LIMITED", &limited);
    vars.set("SHUNT_TEST_SEARCH_HEALTHY", &healthy);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .and(BearerToken(limited.clone()))
        .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"error":"limited"}"#))
        .expect(0..=1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .and(BearerToken(healthy.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![
            account("account-limited", "SHUNT_TEST_SEARCH_LIMITED"),
            account("account-healthy", "SHUNT_TEST_SEARCH_HEALTHY"),
        ],
    ))
    .await;

    let response = post_search(&gateway, "/alpha/search", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-shunt-account").unwrap(),
        "account-healthy"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn a_search_never_follows_a_model_route() {
    // `[[server.codex_endpoint.routes]]` maps Responses models to providers. A
    // search is not a model turn, so even a body naming a routed model stays on
    // the endpoint's own ChatGPT/Codex pool.
    if !can_bind_loopback() {
        return;
    }
    let token = chatgpt_token("acct-unrouted");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_SEARCH_UNROUTED", &token);
    vars.set("SHUNT_TEST_SEARCH_ROUTED_KEY", "routed-key");

    let pool = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .and(BearerToken(token.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&pool)
        .await;
    let routed = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&routed)
        .await;

    let mut config = test_config(
        &pool.uri(),
        vec![account("account-unrouted", "SHUNT_TEST_SEARCH_UNROUTED")],
    );
    let third_party = config.providers.get_mut("openai").unwrap();
    third_party.base_url = routed.uri();
    third_party.api_key_env = Some("SHUNT_TEST_SEARCH_ROUTED_KEY".to_string());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: "gpt-5.6-sol".to_string(),
            provider: "openai".to_string(),
            upstream_model: None,
        }],
    });
    let gateway = start_gateway_with(config).await;

    let response = post_search(&gateway, "/v1/alpha/search", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    pool.verify().await;
    routed.verify().await;
}

#[tokio::test]
async fn search_paths_are_post_only() {
    // The web-search tool has no WebSocket form: unlike `/responses`, a GET
    // (and so a WebSocket upgrade) on a search path is a 405, not an upgrade.
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    let _vars = common::env_lock().await;
    let gateway = start_gateway_with(test_config(&upstream.uri(), Vec::new())).await;

    for endpoint_path in SEARCH_PATHS {
        let response = reqwest::Client::new()
            .get(format!("{}{}", gateway.base_url, endpoint_path))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{endpoint_path}"
        );
        assert_eq!(response.headers().get("allow").unwrap(), "POST");
    }
}

#[tokio::test]
async fn responses_turns_still_reach_the_responses_route() {
    // Adding the search operation must not move a turn: `/v1/responses` still
    // relays to `/codex/responses`, never to the search route.
    if !can_bind_loopback() {
        return;
    }
    let token = chatgpt_token("acct-turn");
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_SEARCH_TURN", &token);

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/alpha/search"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;

    let gateway = start_gateway_with(test_config(
        &upstream.uri(),
        vec![account("account-turn", "SHUNT_TEST_SEARCH_TURN")],
    ))
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", gateway.base_url))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5.6-sol","input":"hi","stream":false}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream.verify().await;
}
