//! Header and cookie sign-in (`[server.auth] jwt_header` / `jwt_cookie`) on the
//! inbound Codex endpoint, end to end: a real router, a loopback JWKS issuer
//! (wiremock) and mock upstreams.
//!
//! The Codex CLI's `gateway_oauth` delivers an identity-provider JWT in a named
//! header or cookie because Codex reserves `Authorization`. These tests pin what
//! a client sees (served turn, `401`, `503`, `usage_limit_reached`) and what an
//! upstream must never see (the JWT header, any `cookie`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        AccountConfig, AdminConfig, CodexEndpointConfig, CodexRouteConfig, Config,
        InboundAuthConfig, InboundJwtConfig, SpendConfig, StringList,
    },
    gateway::spend::{
        meter::{window, FEMTO_USD_PER_CENT},
        store::Period,
    },
    server::{self, AppState},
};
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tungstenite::client::IntoClientRequest;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;
use common::jwt;

const JWT_HEADER: &str = "x-gateway-token";
const JWT_COOKIE: &str = "auth-openid";
const ACCOUNT_ENV: &str = "SHUNT_TEST_JWT_SLOTS_ACCOUNT";
const ROUTE_KEY_ENV: &str = "SHUNT_TEST_JWT_SLOTS_ROUTE_KEY";
const ADMIN_ENV: &str = "SHUNT_TEST_JWT_SLOTS_ADMIN";
const ADMIN_TOKEN: &str = "admin-secret";
const MODEL: &str = "gpt-5.6-sol";
const BODY: &str = r#"{"model":"gpt-5.6-sol","input":[],"stream":false,"store":false}"#;

const MESSAGES_BODY: &str =
    r#"{"model":"claude-sonnet-4-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Gateway {
    base_url: String,
    addr: SocketAddr,
    state: AppState,
    task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn chatgpt_token() -> String {
    let payload = json!({
        "exp": now() + 3_600,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-jwt-slots"}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

async fn env() -> common::EnvVars {
    common::set_env(&[
        (ACCOUNT_ENV, &chatgpt_token()),
        (ROUTE_KEY_ENV, "route-key"),
        (ADMIN_ENV, &format!("admin:{ADMIN_TOKEN}")),
    ])
    .await
}

/// A token for `identity` signed by the fixture key, valid for `lifetime` seconds.
fn jwt_for(issuer: &str, identity: &str, lifetime: u64) -> String {
    jwt::token(&jwt::claims(issuer, identity, lifetime))
}

/// `[server.auth]` with one Entra-shaped issuer entry: no `email`, the caller
/// named by `preferred_username`, access limited to tenant `t1`.
fn auth(
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

fn config(upstream: &str, auth: InboundAuthConfig) -> Config {
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

async fn start(mut config: Config) -> Gateway {
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
async fn upstream(upstream_path: &str) -> MockServer {
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

async fn post(
    gateway: &Gateway,
    route: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{route}", gateway.base_url))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(body.to_string()).send().await.unwrap()
}

/// Asserts no request `server` received carried the JWT header or a `cookie`.
async fn assert_no_credential_forwarded(server: &MockServer) {
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty(), "the upstream must have been called");
    for request in requests {
        assert!(
            !request.headers.contains_key(JWT_HEADER),
            "{:?}",
            request.headers
        );
        assert!(
            !request.headers.contains_key("cookie"),
            "{:?}",
            request.headers
        );
    }
}

async fn connect(
    gateway: &Gateway,
    headers: &[(&str, &str)],
) -> Result<Socket, tungstenite::Error> {
    let mut request = format!("ws://{}/backend-api/codex/responses", gateway.addr)
        .into_client_request()
        .unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    connect_async(request).await.map(|(socket, _)| socket)
}

async fn send_create(socket: &mut Socket) {
    let create = json!({"type": "response.create", "generate": true, "model": MODEL, "input": []});
    socket
        .send(Message::Text(create.to_string().into()))
        .await
        .unwrap();
}

async fn next_json(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for a websocket frame")
        .expect("socket ended")
        .expect("websocket read failed");
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

/// Reads frames until `response.completed`, failing on an `error` frame.
async fn expect_completed(socket: &mut Socket) {
    loop {
        let frame = next_json(socket).await;
        assert_ne!(frame["type"], "error", "the turn failed: {frame}");
        if frame["type"] == "response.completed" {
            return;
        }
    }
}

#[tokio::test]
async fn header_only_request_is_served() {
    // AC-001
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        None,
    );
    let gateway = start(config(&upstream.uri(), auth)).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);

    let response = post(&gateway, "/v1/responses", BODY, &[(JWT_HEADER, &token)]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn header_only_request_to_the_anthropic_endpoint_is_unauthorized() {
    // AC-007: the header is a Codex-gate slot; the Anthropic gate ignores it.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        None,
    );
    let gateway = start(config(&upstream.uri(), auth)).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);

    let response = post(
        &gateway,
        "/v1/messages",
        MESSAGES_BODY,
        &[(JWT_HEADER, &token)],
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cookie_websocket_serves_a_turn_then_refuses_one_after_expiry() {
    // AC-003, plus the expiry Review Focus: the cookie is re-verified per turn.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        None,
        Some(JWT_COOKIE),
    );
    let gateway = start(config(&upstream.uri(), auth)).await;
    // `exp` has one-second granularity and the verifier allows no skew, so the
    // token is valid through its `exp` second and dead the second after. The
    // lifetime leaves the handshake and the first turn at least that long on a
    // loaded runner, and the deadline below comes from the signed `iat`.
    const LIFETIME: u64 = 5;
    let claims = jwt::claims(&issuer.uri(), "alice", LIFETIME);
    let issued = claims["iat"].as_u64().expect("iat");
    let token = jwt::token(&claims);
    let cookie = format!("theme=dark; {JWT_COOKIE}={token}; other=1");

    let mut socket = connect(&gateway, &[("cookie", &cookie)])
        .await
        .expect("101");
    send_create(&mut socket).await;
    expect_completed(&mut socket).await;
    assert_no_credential_forwarded(&upstream).await;

    let dead_at = Duration::from_secs(issued + LIFETIME + 1) + Duration::from_millis(100);
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    tokio::time::sleep(dead_at.saturating_sub(elapsed)).await;
    send_create(&mut socket).await;

    let frame = next_json(&mut socket).await;
    assert_eq!(frame["type"], "error", "{frame}");
    assert_eq!(frame["status"], 401, "{frame}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn header_websocket_serves_two_turns_from_the_upgrade_header() {
    // The jwt_header slot authenticates the upgrade and every later turn on the socket.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        None,
    );
    let gateway = start(config(&upstream.uri(), auth)).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);

    let mut socket = connect(&gateway, &[(JWT_HEADER, &token)])
        .await
        .expect("101");
    send_create(&mut socket).await;
    expect_completed(&mut socket).await;
    send_create(&mut socket).await;
    expect_completed(&mut socket).await;
    assert_no_credential_forwarded(&upstream).await;
}

#[tokio::test]
async fn cross_origin_websocket_with_only_the_cookie_is_unauthorized() {
    // AC-017: the gate runs before the 101.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        None,
        Some(JWT_COOKIE),
    );
    let gateway = start(config(&upstream.uri(), auth)).await;
    let cookie = format!("{JWT_COOKIE}={}", jwt_for(&issuer.uri(), "alice", 300));

    let rejected = connect(
        &gateway,
        &[("cookie", &cookie), ("origin", "https://evil.example")],
    )
    .await;
    match rejected {
        Err(tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        other => panic!("expected an HTTP 401 upgrade refusal, got {other:?}"),
    }

    // Same cookie with an Origin naming this host is accepted.
    let same_origin = format!("http://{}", gateway.addr);
    let mut socket = connect(&gateway, &[("cookie", &cookie), ("origin", &same_origin)])
        .await
        .expect("a same-origin upgrade is accepted");
    send_create(&mut socket).await;
    expect_completed(&mut socket).await;
}

#[tokio::test]
async fn pool_upstream_never_sees_the_jwt_header_or_cookie() {
    // AC-009
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        Some(JWT_COOKIE),
    );
    let gateway = start(config(&upstream.uri(), auth)).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);
    let cookie = format!("theme=dark; {JWT_COOKIE}={token}");

    let response = post(
        &gateway,
        "/v1/responses",
        BODY,
        &[(JWT_HEADER, &token), ("cookie", &cookie)],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_no_credential_forwarded(&upstream).await;
}

#[tokio::test]
async fn routed_non_chatgpt_upstream_never_sees_the_jwt_header_or_cookie() {
    // AC-010
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let pool = upstream("/codex/responses").await;
    let routed = upstream("/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        Some(JWT_COOKIE),
    );
    let mut config = config(&pool.uri(), auth);
    let third_party = config.providers.get_mut("openai").unwrap();
    third_party.base_url = routed.uri();
    third_party.api_key_env = Some(ROUTE_KEY_ENV.to_string());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: "glm-5.3".to_string(),
            provider: "openai".to_string(),
            upstream_model: None,
        }],
    });
    let gateway = start(config).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);
    let cookie = format!("theme=dark; {JWT_COOKIE}={token}");
    let body = r#"{"model":"glm-5.3","input":[],"stream":false}"#;

    let response = post(
        &gateway,
        "/v1/responses",
        body,
        &[(JWT_HEADER, &token), ("cookie", &cookie)],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_no_credential_forwarded(&routed).await;
    assert!(pool.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unreachable_issuer_is_an_openai_shaped_503() {
    // AC-013: bind then drop a listener so the JWKS port refuses connections.
    let _env = env().await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let upstream = upstream("/codex/responses").await;
    let auth = auth(&dead, &format!("{dead}/jwks"), Some(JWT_HEADER), None);
    let gateway = start(config(&upstream.uri(), auth)).await;
    let token = jwt_for(&dead, "alice", 300);

    let response = post(&gateway, "/v1/responses", BODY, &[(JWT_HEADER, &token)]).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "api_error", "{body}");
    assert!(
        body.get("type").is_none(),
        "not the Anthropic envelope: {body}"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// Gateway with spend caps enabled and an admin surface to create them.
async fn spend_gateway(upstream: &str, issuer: &MockServer) -> Gateway {
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        None,
        Some(JWT_COOKIE),
    );
    let mut config = config(upstream, auth);
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: ADMIN_ENV.to_string(),
        tokens_file: None,
        write_keys: Vec::new(),
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        hide_observed: false,
        oidc: None,
    });
    config.server.spend = Some(SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        ..SpendConfig::default()
    });
    start(config).await
}

#[tokio::test]
async fn over_cap_cookie_principal_is_refused_under_its_own_identity() {
    // AC-008: the cap names "alice" (the preferred_username claim). Alice's
    // cookie turn is refused; bob's, on the same gateway, is served.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let gateway = spend_gateway(&upstream.uri(), &issuer).await;
    let created = reqwest::Client::new()
        .post(format!(
            "{}/v1/organizations/spend_limits",
            gateway.base_url
        ))
        .header("x-shunt-admin-token", ADMIN_TOKEN)
        .json(&json!({"amount": "100",
                      "scope": {"type": "user", "user_id": "alice"},
                      "period": "daily"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK, "cap must be created");
    // Seed after waiting out a UTC day edge so the seed and the turn share a window.
    let remaining = window(Period::Daily, now()).end.saturating_sub(now());
    if remaining < 5 {
        tokio::time::sleep(Duration::from_secs(remaining) + Duration::from_millis(50)).await;
    }
    gateway.state.gateway_stores.spend.meter().record(
        "alice",
        now(),
        100 * FEMTO_USD_PER_CENT as u64,
    );
    let cookie_for =
        |identity: &str| format!("{JWT_COOKIE}={}", jwt_for(&issuer.uri(), identity, 300));

    let mut alice = connect(&gateway, &[("cookie", &cookie_for("alice"))])
        .await
        .unwrap();
    send_create(&mut alice).await;
    let frame = next_json(&mut alice).await;
    assert_eq!(frame["type"], "error", "{frame}");
    assert_eq!(frame["status"], 429, "{frame}");
    assert_eq!(frame["error"]["type"], "usage_limit_reached", "{frame}");
    assert!(upstream.received_requests().await.unwrap().is_empty());

    let mut bob = connect(&gateway, &[("cookie", &cookie_for("bob"))])
        .await
        .unwrap();
    send_create(&mut bob).await;
    expect_completed(&mut bob).await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}
