use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::{
    adapters::AdapterFailure,
    config::{AccountConfig, Config},
    request::RequestBody,
    routing,
    server::AppState,
};

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(0);

fn access_token(account_id: &str) -> String {
    let payload = json!({
        "exp": 4_102_444_800_u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!("x.{}.y", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

struct Token(String);

impl Token {
    fn new(present: bool) -> Self {
        let index = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let name = format!("SHUNT_CODEX_PROVENANCE_{}_{index}", std::process::id());
        assert!(std::env::var_os(&name).is_none());
        if present {
            std::env::set_var(&name, access_token(&name));
        }
        Self(name)
    }

    fn local_failure(invalid_header: bool) -> Self {
        let token = Self::new(false);
        if invalid_header {
            std::env::set_var(&token.0, access_token("invalid\naccount"));
        }
        token
    }

    fn account(&self, priority: u32) -> AccountConfig {
        AccountConfig {
            name: self.0.to_ascii_lowercase().replace('_', "-"),
            token_env: Some(self.0.clone()),
            uuid: Some(self.0.clone()),
            priority,
            ..Default::default()
        }
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        std::env::remove_var(&self.0);
    }
}

fn state(base_url: String, accounts: Vec<AccountConfig>, websocket: bool) -> AppState {
    let mut config = Config::default();
    config.server.default_provider = "codex".into();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = base_url;
    provider.accounts = accounts;
    provider.websocket = websocket;
    provider.request_compression = false;
    AppState::new(config, reqwest::Client::new()).unwrap()
}

async fn forward(state: AppState) -> crate::adapters::AdapterResult {
    let route = routing::resolve_model(&state.config, "gpt-6-astra");
    let body = RequestBody::parse(
        json!({
            "model": "gpt-6-astra", "max_tokens": 8,
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string()
        .into_bytes(),
    )
    .unwrap();
    super::super::forward(state, route, None, None, body).await
}

#[tokio::test]
async fn local_credential_failures_never_claim_an_http_or_websocket_attempt() {
    for invalid_header in [false, true] {
        for websocket in [false, true] {
            let server = MockServer::start().await;
            let first = Token::local_failure(invalid_header);
            let second = Token::local_failure(invalid_header);
            let state = state(
                server.uri(),
                vec![first.account(1), second.account(2)],
                websocket,
            );

            let error = forward(state).await.unwrap_err();

            assert_eq!(error.failure, Some(AdapterFailure::NoUpstreamAttempt));
            assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(error.message, "responses adapter failed");
            let body = axum::body::to_bytes(error.response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["type"], "error");
            assert_eq!(
                body["error"]["message"],
                "all Codex OAuth accounts failed before receiving an upstream response"
            );
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn later_local_failure_preserves_an_earlier_transport_attempt() {
    for invalid_header in [false, true] {
        for websocket in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0; 4096];
                    assert!(socket.read(&mut request).await.unwrap() > 0);
                })
                .await
                .expect("the transport must reach the loopback listener");
            });
            let first = Token::new(true);
            let second = Token::local_failure(invalid_header);
            let state = state(
                base_url,
                vec![first.account(1), second.account(2)],
                websocket,
            );

            let error = forward(state).await.unwrap_err();

            assert_eq!(error.failure, Some(AdapterFailure::BeforeHeaders));
            assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn later_local_failure_preserves_an_upstream_status_and_error_body() {
    for invalid_header in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/codex/responses"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "error": {"message": "synthetic upstream failure"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let first = Token::new(true);
        let second = Token::local_failure(invalid_header);
        let state = state(
            server.uri(),
            vec![first.account(1), second.account(2)],
            false,
        );

        let error = forward(state).await.unwrap_err();

        assert_eq!(
            error.failure,
            Some(AdapterFailure::UpstreamStatus(
                StatusCode::SERVICE_UNAVAILABLE
            ))
        );
        assert_eq!(error.response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["message"], "synthetic upstream failure");
        server.verify().await;
    }
}

#[tokio::test]
async fn local_refresh_retry_preserves_the_original_response() {
    let identity = Token::new(false);
    let credentials = std::env::temp_dir().join(format!("{}.json", identity.0));
    assert!(!credentials.exists());
    let original = access_token("original-account");
    std::fs::write(
        &credentials,
        json!({"tokens": {"access_token": original}}).to_string(),
    )
    .unwrap();
    let mut account = identity.account(1);
    account.token_env = None;
    account.credentials = Some(credentials.to_string_lossy().into_owned());
    let upstream = MockServer::start().await;
    let replaced_credentials = credentials.clone();
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(move |_: &wiremock::Request| {
            // Another refresher installs a different token before the 401 retry reads it.
            std::fs::write(
                &replaced_credentials,
                json!({"tokens": {
                    "access_token": access_token("invalid\naccount")
                }})
                .to_string(),
            )
            .unwrap();
            ResponseTemplate::new(401)
                .insert_header("retry-after", "7")
                .set_body_json(json!({"error": {"message": "original unauthorized"}}))
        })
        .expect(1)
        .mount(&upstream)
        .await;

    let state = state(upstream.uri(), vec![account.clone()], false);
    let error = forward(state.clone()).await.unwrap_err();

    assert_eq!(
        error.failure,
        Some(AdapterFailure::UpstreamStatus(StatusCode::UNAUTHORIZED))
    );
    assert_eq!(error.response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error.response.headers()["retry-after"], "7");
    let health = state.accounts.snapshot("codex", &[account], None, None);
    assert!(
        health[0]
            .cooldown_secs_remaining
            .is_some_and(|seconds| seconds <= 30),
        "the retry must use the transport cooldown, not the auth cooldown"
    );
    let body = axum::body::to_bytes(error.response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["error"]["message"],
        "ChatGPT authentication failed; run codex login"
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
    assert_eq!(
        stored["tokens"]["access_token"],
        access_token("invalid\naccount")
    );
    upstream.verify().await;
    std::fs::remove_file(credentials).unwrap();
}
