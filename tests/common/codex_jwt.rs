//! The Codex-endpoint JWT sign-in harness shared by the integration tests that
//! drive `[server.auth] jwt_header` / `jwt_cookie` on the inbound Codex
//! endpoint: a real router on a loopback port, a `codex` provider backed by a
//! mock upstream, and an Entra-shaped issuer entry.

use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use shunt::{
    config::{
        AccountConfig, CodexEndpointConfig, Config, InboundAuthConfig, InboundJwtConfig, StringList,
    },
    server::{self, AppState},
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::jwt;

pub const JWT_HEADER: &str = "x-gateway-token";
pub const JWT_COOKIE: &str = "auth-openid";
pub const ACCOUNT_ENV: &str = "SHUNT_TEST_JWT_SLOTS_ACCOUNT";
pub const ROUTE_KEY_ENV: &str = "SHUNT_TEST_JWT_SLOTS_ROUTE_KEY";
pub const ADMIN_ENV: &str = "SHUNT_TEST_JWT_SLOTS_ADMIN";
pub const ADMIN_TOKEN: &str = "admin-secret";
pub struct Gateway {
    pub base_url: String,
    pub addr: SocketAddr,
    pub state: AppState,
    pub task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn chatgpt_token() -> String {
    let payload = json!({
        "exp": now() + 3_600,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-jwt-slots"}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

pub async fn env() -> super::EnvVars {
    super::set_env(&[
        (ACCOUNT_ENV, &chatgpt_token()),
        (ROUTE_KEY_ENV, "route-key"),
        (ADMIN_ENV, &format!("admin:{ADMIN_TOKEN}")),
    ])
    .await
}

/// A token for `identity` signed by the fixture key, valid for `lifetime` seconds.
pub fn jwt_for(issuer: &str, identity: &str, lifetime: u64) -> String {
    jwt::token(&jwt::claims(issuer, identity, lifetime))
}

/// `[server.auth]` with one Entra-shaped issuer entry: no `email`, the caller
/// named by `preferred_username`, access limited to tenant `t1`.
pub fn auth(
    issuer: &str,
    jwks_url: &str,
    header: Option<&str>,
    cookie: Option<&str>,
) -> InboundAuthConfig {
    InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_JWT_SLOTS_NO_TOKENS".to_string(),
        jwt_header: header.map(str::to_string),
        jwt_cookie: cookie.map(str::to_string),
        jwt: vec![InboundJwtConfig {
            issuer: issuer.to_string(),
            audience: StringList::One(jwt::AUDIENCE.to_string()),
            email_domains: Vec::new(),
            allowed_emails: Vec::new(),
            require: [("tid".to_string(), StringList::One("t1".to_string()))].into(),
            subject_prefix: StringList::default(),
            identity_claim: Some("preferred_username".to_string()),
            algorithms: vec!["RS256".to_string()],
            authorized_parties: Vec::new(),
            clock_skew_seconds: 0,
            max_token_age_seconds: 3600,
            jwks_url: Some(jwks_url.to_string()),
        }],
    }
}

pub fn config(upstream: &str, auth: InboundAuthConfig) -> Config {
    let mut config = Config::default();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = upstream.to_string();
    provider.accounts = vec![AccountConfig {
        name: "account".to_string(),
        token_env: Some(ACCOUNT_ENV.to_string()),
        ..Default::default()
    }];
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        ..Default::default()
    });
    config.server.auth = Some(auth);
    config
}

pub async fn start(mut config: Config) -> Gateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gateway {
        base_url: format!("http://{addr}"),
        addr,
        state,
        task,
    }
}

/// A mock upstream answering every turn with one completed SSE response.
pub async fn upstream(upstream_path: &str) -> MockServer {
    let server = MockServer::start().await;
    let body = format!(
        "event: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed",
               "response": {"id": "resp_1", "usage": {
                   "input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}})
    );
    Mock::given(method("POST"))
        .and(path(upstream_path))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    server
}
