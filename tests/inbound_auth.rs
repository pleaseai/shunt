//! M4 inbound client authentication — gateway-level behavior.
//!
//! Injected-credential routes require a valid client token when
//! `[server.auth]` is configured; passthrough routes stay open; the token
//! header is always stripped before forwarding.

use std::{io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use shunt::{
    config::{
        ApiKeyHeader, AuthMode, Config, CountTokens, InboundAuthConfig, ProviderConfig,
        ProviderKind, RouteConfig,
    },
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

/// Asserts a header is absent from the forwarded request.
struct NoHeader(&'static str);

impl Match for NoHeader {
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

/// Gateway with a `mapped` provider that injects an API key (read from
/// `api_key_env`) and forwards to the mock upstream, plus the default
/// `anthropic` passthrough pointed at the same upstream.
fn test_config(upstream_base_url: &str, api_key_env: &'static str) -> Config {
    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream_base_url.to_string();
    config.providers.insert(
        "mapped".to_string(),
        ProviderConfig {
            kind: ProviderKind::Anthropic,
            base_url: upstream_base_url.to_string(),
            auth: AuthMode::ApiKey,
            api_key_env: Some(api_key_env.to_string()),
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            count_tokens: CountTokens::default(),
            websocket: false,
            tool_search: false,
            accounts: Vec::new(),
        },
    );
    config.routes.push(RouteConfig {
        model: "mapped-model".to_string(),
        provider: "mapped".to_string(),
        upstream_model: None,
        effort: None,
    });
    config
}

fn with_inbound_auth(mut config: Config, tokens_env: &'static str) -> Config {
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.to_string(),
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

async fn post_messages(
    gateway: &TestGateway,
    model: &str,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"model":"{model}","max_tokens":16,"messages":[{{"role":"user","content":"hi"}}]}}"#
        ));
    if let Some(token) = token {
        request = request.header("x-shunt-token", token);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn mapped_route_without_token_is_401_and_upstream_is_never_called() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_M4_KEY_A", "upstream-key");
    std::env::set_var("SHUNT_TEST_M4_TOKENS_A", "alice:tok-a");
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(0)
        .mount(&upstream)
        .await;
    let config = with_inbound_auth(
        test_config(&upstream.uri(), "SHUNT_TEST_M4_KEY_A"),
        "SHUNT_TEST_M4_TOKENS_A",
    );
    let gateway = start_gateway_with(config).await;

    let response = post_messages(&gateway, "mapped-model", None).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("x-shunt-token"));
}

#[tokio::test]
async fn mapped_route_with_wrong_token_is_401() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_M4_KEY_B", "upstream-key");
    std::env::set_var("SHUNT_TEST_M4_TOKENS_B", "alice:tok-a");
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(0)
        .mount(&upstream)
        .await;
    let config = with_inbound_auth(
        test_config(&upstream.uri(), "SHUNT_TEST_M4_KEY_B"),
        "SHUNT_TEST_M4_TOKENS_B",
    );
    let gateway = start_gateway_with(config).await;

    let response = post_messages(&gateway, "mapped-model", Some("not-the-token")).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mapped_route_with_valid_token_forwards_and_strips_the_token_header() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_M4_KEY_C", "upstream-key");
    std::env::set_var("SHUNT_TEST_M4_TOKENS_C", "alice:tok-a,bob:tok-b");
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(NoHeader("x-shunt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let config = with_inbound_auth(
        test_config(&upstream.uri(), "SHUNT_TEST_M4_KEY_C"),
        "SHUNT_TEST_M4_TOKENS_C",
    );
    let gateway = start_gateway_with(config).await;

    let response = post_messages(&gateway, "mapped-model", Some("tok-b")).await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn passthrough_route_needs_no_token_and_still_strips_the_header() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_M4_KEY_D", "upstream-key");
    std::env::set_var("SHUNT_TEST_M4_TOKENS_D", "alice:tok-a");
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(NoHeader("x-shunt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(2)
        .mount(&upstream)
        .await;
    let config = with_inbound_auth(
        test_config(&upstream.uri(), "SHUNT_TEST_M4_KEY_D"),
        "SHUNT_TEST_M4_TOKENS_D",
    );
    let gateway = start_gateway_with(config).await;

    // No token at all: passthrough is open.
    let response = post_messages(&gateway, "claude-sonnet-4-6", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    // A stale/wrong token on a passthrough request is stripped, not rejected.
    let response = post_messages(&gateway, "claude-sonnet-4-6", Some("whatever")).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_and_root_stay_open_when_inbound_auth_is_configured() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_M4_KEY_F", "upstream-key");
    std::env::set_var("SHUNT_TEST_M4_TOKENS_F", "alice:tok-a");
    let upstream = MockServer::start().await;
    let config = with_inbound_auth(
        test_config(&upstream.uri(), "SHUNT_TEST_M4_KEY_F"),
        "SHUNT_TEST_M4_TOKENS_F",
    );
    let gateway = start_gateway_with(config).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{}/health", gateway.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["status"], "ok");

    let response = client
        .get(format!("{}/", gateway.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn without_auth_config_mapped_route_stays_open() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_M4_KEY_E", "upstream-key");
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1)
        .mount(&upstream)
        .await;
    let config = test_config(&upstream.uri(), "SHUNT_TEST_M4_KEY_E");
    let gateway = start_gateway_with(config).await;

    let response = post_messages(&gateway, "mapped-model", None).await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn auth_config_without_tokens_env_fails_startup() {
    std::env::remove_var("SHUNT_TEST_M4_TOKENS_MISSING");
    let config = with_inbound_auth(Config::default(), "SHUNT_TEST_M4_TOKENS_MISSING");
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("SHUNT_TEST_M4_TOKENS_MISSING"));
    assert!(error.contains("refusing to run open"));
}
