use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{HeaderMap, StatusCode};
use tokio::io::AsyncReadExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

use crate::{
    adapters::AdapterFailure,
    config::{AccountConfig, AuthMode, Config},
    request::RequestBody,
    routing,
    server::AppState,
};

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(0);

struct Token(String);

impl Token {
    fn new(present: bool) -> Self {
        let index = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let name = format!("SHUNT_POOL_PROVENANCE_{}_{index}", std::process::id());
        assert!(std::env::var_os(&name).is_none());
        if present {
            std::env::set_var(&name, "synthetic-pool-provenance");
        }
        Self(name)
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

fn state(auth: AuthMode, base_url: String, accounts: Vec<AccountConfig>) -> AppState {
    let mut config = Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.auth = auth;
    provider.base_url = base_url;
    provider.accounts = accounts;
    AppState::new(config, reqwest::Client::new()).unwrap()
}

async fn forward(state: AppState) -> crate::adapters::AdapterResult {
    let route = routing::resolve_model(&state.config, "claude-fable-5");
    super::forward(
        state,
        route,
        &"/v1/messages".parse().unwrap(),
        &HeaderMap::new(),
        RequestBody::parse(br#"{"model":"claude-fable-5","max_tokens":8,"messages":[{"role":"user","content":"hello"}]}"#.to_vec()).unwrap(),
    ).await
}

#[tokio::test]
async fn pools_report_no_upstream_attempt_when_every_credential_fails() {
    for auth in [AuthMode::ClaudeOauth, AuthMode::KimiOauth] {
        let server = MockServer::start().await;
        let first = Token::new(false);
        let second = Token::new(false);
        let state = state(
            auth,
            server.uri(),
            vec![first.account(1), second.account(2)],
        );

        let error = forward(state).await.unwrap_err();

        assert_eq!(error.failure, Some(AdapterFailure::NoUpstreamAttempt));
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_later_credential_failure_preserves_an_earlier_transport_attempt() {
    for auth in [AuthMode::ClaudeOauth, AuthMode::KimiOauth] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
            })
            .await
            .expect("the transport attempt must reach the loopback listener");
        });
        let first = Token::new(true);
        let second = Token::new(false);
        let state = state(auth, base_url, vec![first.account(1), second.account(2)]);

        let error = forward(state).await.unwrap_err();

        assert_eq!(error.failure, Some(AdapterFailure::BeforeHeaders));
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn a_later_credential_failure_preserves_the_upstream_status_and_body() {
    for auth in [AuthMode::ClaudeOauth, AuthMode::KimiOauth] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("synthetic upstream failure"))
            .expect(1)
            .mount(&server)
            .await;
        let first = Token::new(true);
        let second = Token::new(false);
        let state = state(
            auth,
            server.uri(),
            vec![first.account(1), second.account(2)],
        );

        let (status, response) = forward(state).await.unwrap();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status(), status);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"synthetic upstream failure");
        server.verify().await;
    }
}
