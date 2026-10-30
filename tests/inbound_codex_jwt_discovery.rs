//! Codex-shaped model discovery reading `[server.auth] jwt_header` /
//! `jwt_cookie`, end to end: a real router, a loopback JWKS issuer (wiremock)
//! and the Codex endpoint enabled. Each test maps to one acceptance criterion
//! of the `codex-discovery-jwt-slots` track spec.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{Config, GatewayConfig},
    gateway::{approval::Identity, jwt as gateway_jwt},
};

mod common;
use common::{
    codex_jwt::{auth, chatgpt_token, config, env, jwt_for, start, upstream, Gateway},
    codex_jwt::{JWT_COOKIE, JWT_HEADER},
    jwt,
};

const STATIC_ENV: &str = "SHUNT_TEST_JWT_DISCOVERY_TOKENS";
const GATEWAY_SECRET_ENV: &str = "SHUNT_TEST_JWT_DISCOVERY_GW_SECRET";
const GATEWAY_USERS_ENV: &str = "SHUNT_TEST_JWT_DISCOVERY_GW_USERS";
const GATEWAY_SECRET: &str = "0123456789abcdef0123456789abcdef";
const GATEWAY_URL: &str = "https://gateway.example";

async fn get(gateway: &Gateway, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(format!("{}{path}", gateway.base_url));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

async fn assert_catalog(response: reqwest::Response) {
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, json!({"models": []}));
}

/// A gateway with one issuer (`header` / `cookie` slots) and the Codex endpoint.
async fn gateway_with(
    issuer: &wiremock::MockServer,
    header: Option<&str>,
    cookie: Option<&str>,
) -> Gateway {
    let upstream = upstream("/codex/responses").await;
    let auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        header,
        cookie,
    );
    start(config(&upstream.uri(), auth)).await
}

#[tokio::test]
async fn cookie_jwt_with_a_chatgpt_bearer_is_served_on_v1_models_with_client_version() {
    // AC-001, plus the Review Focus: an empty `client_version` value and a
    // later position in the query both select the Codex posture.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let gateway = gateway_with(&issuer, None, Some(JWT_COOKIE)).await;
    let bearer = format!("Bearer {}", chatgpt_token());
    let cookie = format!("{JWT_COOKIE}={}", jwt_for(&issuer.uri(), "alice", 300));

    for path in [
        "/v1/models?client_version=0.1.0",
        "/v1/models?client_version",
        "/v1/models?foo=1&client_version=x",
    ] {
        let response = get(
            &gateway,
            path,
            &[("authorization", &bearer), ("cookie", &cookie)],
        )
        .await;
        assert_catalog(response).await;
    }
}

#[tokio::test]
async fn header_jwt_alone_is_served_on_the_codex_models_path() {
    // AC-002
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let gateway = gateway_with(&issuer, Some(JWT_HEADER), None).await;
    let value = format!("Bearer {}", jwt_for(&issuer.uri(), "alice", 300));

    // Every Codex-shaped discovery path reads the slot, not just this one.
    for path in [
        "/backend-api/codex/models",
        "/models",
        "/v1/models?client_version=0.1.0",
    ] {
        let response = get(&gateway, path, &[(JWT_HEADER, &value)]).await;
        assert_catalog(response).await;
    }
}

#[tokio::test]
async fn cross_origin_cookie_is_unauthorized_on_models() {
    // AC-003
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let gateway = gateway_with(&issuer, None, Some(JWT_COOKIE)).await;
    let cookie = format!("{JWT_COOKIE}={}", jwt_for(&issuer.uri(), "alice", 300));

    let response = get(
        &gateway,
        "/models",
        &[("cookie", &cookie), ("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Non-vacuity: the same cookie with an Origin naming this host is accepted.
    let same_origin = format!("http://{}", gateway.addr);
    let response = get(
        &gateway,
        "/models",
        &[("cookie", &cookie), ("origin", &same_origin)],
    )
    .await;
    assert_catalog(response).await;
}

#[tokio::test]
async fn repeated_jwt_header_is_unauthorized_on_models() {
    // AC-004: a repeated slot ends the scan, even when each value is valid.
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let gateway = gateway_with(&issuer, Some(JWT_HEADER), None).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);

    let response = get(
        &gateway,
        "/models",
        &[(JWT_HEADER, &token), (JWT_HEADER, &token)],
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn header_jwt_for_another_audience_is_unauthorized() {
    // AC-005
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let gateway = gateway_with(&issuer, Some(JWT_HEADER), None).await;
    let mut claims = jwt::claims(&issuer.uri(), "alice", 300);
    claims["aud"] = json!("someone-else");
    let token = jwt::token(&claims);

    for path in ["/models", "/v1/models?client_version=1"] {
        let response = get(&gateway, path, &[(JWT_HEADER, &token)]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn static_x_api_key_still_wins_over_a_repeated_jwt_header() {
    // AC-006, plus the Review Focus: discovery keeps `x-api-key`, and a static
    // token authenticates before the repeated slot is reached.
    let mut vars = env().await;
    vars.set(STATIC_ENV, "alice:static-secret");
    let issuer = jwt::issuer().await;
    let upstream = upstream("/codex/responses").await;
    let mut auth = auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        None,
    );
    auth.tokens_env = STATIC_ENV.to_string();
    let gateway = start(config(&upstream.uri(), auth)).await;

    let response = get(&gateway, "/models", &[("x-api-key", "static-secret")]).await;
    assert_catalog(response).await;

    let response = get(
        &gateway,
        "/models",
        &[
            ("x-api-key", "static-secret"),
            (JWT_HEADER, "junk"),
            (JWT_HEADER, "junk"),
        ],
    )
    .await;
    assert_catalog(response).await;
}

#[tokio::test]
async fn anthropic_shaped_v1_models_ignores_the_jwt_header_and_cookie() {
    // AC-007
    let _env = env().await;
    let issuer = jwt::issuer().await;
    let gateway = gateway_with(&issuer, Some(JWT_HEADER), Some(JWT_COOKIE)).await;
    let token = jwt_for(&issuer.uri(), "alice", 300);
    let cookie = format!("{JWT_COOKIE}={token}");

    let by_header = get(&gateway, "/v1/models", &[(JWT_HEADER, &token)]).await;
    assert_eq!(by_header.status(), StatusCode::UNAUTHORIZED);
    let by_cookie = get(&gateway, "/v1/models", &[("cookie", &cookie)]).await;
    assert_eq!(by_cookie.status(), StatusCode::UNAUTHORIZED);
}

/// An issuer whose JWKS port refuses connections.
fn dead_issuer() -> String {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    dead
}

#[tokio::test]
async fn unreachable_issuer_on_the_cookie_is_an_openai_shaped_503() {
    // AC-008
    let _env = env().await;
    let dead = dead_issuer();
    let upstream = upstream("/codex/responses").await;
    let auth = auth(&dead, &format!("{dead}/jwks"), None, Some(JWT_COOKIE));
    let gateway = start(config(&upstream.uri(), auth)).await;
    let cookie = format!("{JWT_COOKIE}={}", jwt_for(&dead, "alice", 300));

    let response = get(&gateway, "/models", &[("cookie", &cookie)]).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "api_error", "{body}");
    assert!(
        body.get("type").is_none(),
        "not the Anthropic envelope: {body}"
    );
}

#[tokio::test]
async fn gateway_login_token_wins_over_an_unreachable_issuer_on_the_cookie() {
    // AC-009
    let mut vars = env().await;
    vars.set(GATEWAY_SECRET_ENV, GATEWAY_SECRET);
    vars.set(GATEWAY_USERS_ENV, "dev@example.com:approval-secret");
    let dead = dead_issuer();
    let upstream = upstream("/codex/responses").await;
    let auth = auth(&dead, &format!("{dead}/jwks"), None, Some(JWT_COOKIE));
    let mut config: Config = config(&upstream.uri(), auth);
    config.server.gateway = Some(GatewayConfig {
        desktop_bootstrap: None,
        public_url: GATEWAY_URL.to_string(),
        jwt_secret_env: Some(GATEWAY_SECRET_ENV.to_string()),
        users_env: GATEWAY_USERS_ENV.to_string(),
        token_ttl_seconds: Some(3600),
        trust_forwarded_for: false,
        policies: None,
        telemetry: None,
        state_path: None,
        oidc: None,
        session: None,
    });
    let gateway = start(config).await;
    let login = gateway_jwt::mint(
        &Identity {
            sub: "dev".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        },
        GATEWAY_URL,
        GATEWAY_SECRET.as_bytes(),
        3600,
    );
    let bearer = format!("Bearer {login}");
    let cookie = format!("{JWT_COOKIE}={}", jwt_for(&dead, "alice", 300));

    let response = get(
        &gateway,
        "/models",
        &[("authorization", &bearer), ("cookie", &cookie)],
    )
    .await;

    assert_catalog(response).await;
}
