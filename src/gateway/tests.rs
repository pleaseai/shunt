use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::{
    config::{
        Config, GatewayConfig, GatewayPolicyConfig, GatewayPolicyMatch, GatewayTelemetryConfig,
        GatewayTelemetryDestination, InboundAuthConfig, ModelConfig, RouteConfig,
    },
    server::{build_router, AppState},
};

use super::{approval::Identity, jwt};

struct GatewayEnv {
    secret_env: String,
    users_env: String,
}

impl GatewayEnv {
    fn config(label: &str) -> (Config, Self) {
        let suffix = format!("{}_{}", std::process::id(), label);
        let secret_env = format!("SHUNT_GATEWAY_TEST_SECRET_{suffix}");
        let users_env = format!("SHUNT_GATEWAY_TEST_USERS_{suffix}");
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&users_env, "dev@example.com:password");
        let mut config = Config::default();
        config.server.gateway = Some(GatewayConfig {
            public_url: "https://gateway.example".into(),
            jwt_secret_env: secret_env.clone(),
            users_env: users_env.clone(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
        });
        (
            config,
            Self {
                secret_env,
                users_env,
            },
        )
    }
}

impl Drop for GatewayEnv {
    fn drop(&mut self) {
        std::env::remove_var(&self.secret_env);
        std::env::remove_var(&self.users_env);
    }
}

async fn json_response(router: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&body).expect("JSON response");
    (status, value)
}

fn policy(emails: Option<Vec<&str>>, cli: impl Into<toml::Value>) -> GatewayPolicyConfig {
    GatewayPolicyConfig {
        matcher: emails.map(|emails| GatewayPolicyMatch {
            emails: Some(emails.into_iter().map(str::to_string).collect()),
        }),
        cli: cli.into(),
    }
}

fn gateway_bearer(email: &str) -> String {
    jwt::mint(
        &Identity {
            sub: email.to_string(),
            email: email.to_string(),
            name: email.split('@').next().unwrap_or(email).to_string(),
        },
        "https://gateway.example",
        b"0123456789abcdef0123456789abcdef",
        3600,
    )
}

fn managed_request(bearer: Option<&str>, if_none_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri("/managed/settings");
    if let Some(bearer) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    if let Some(value) = if_none_match {
        builder = builder.header(header::IF_NONE_MATCH, value);
    }
    builder.body(Body::empty()).unwrap()
}

fn form_request(path: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.into()))
        .unwrap()
}

async fn device_attempt(
    router: &Router,
    peer: std::net::SocketAddr,
    forwarded_for: &str,
) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/device")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::ORIGIN, "https://gateway.example")
                .header("x-forwarded-for", forwarded_for)
                .extension(ConnectInfo(peer))
                .body(Body::from(
                    "user_code=BCDF-GHJK&login=dev%40example.com&secret=wrong",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

fn inference_request(path: &str, authorization: (&str, &str), model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(authorization.0, authorization.1)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ))
        .unwrap()
}

async fn responses_upstream() -> wiremock::MockServer {
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    upstream
}

fn temp_config_path(label: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let suffix = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(format!("shunt-{label}-{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shunt.toml");
    (dir, path, suffix)
}

#[tokio::test]
async fn discovery_has_exact_reference_shape() {
    let (config, _env) = GatewayEnv::config("discovery");
    let (router, _, _) = build_router(config).unwrap();

    let (status, body) = json_response(
        router,
        Request::builder()
            .uri("/.well-known/oauth-authorization-server")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "issuer": "https://gateway.example",
            "device_authorization_endpoint": "https://gateway.example/oauth/device_authorization",
            "token_endpoint": "https://gateway.example/oauth/token",
            "grant_types_supported": [
                "urn:ietf:params:oauth:grant-type:device_code",
                "refresh_token"
            ],
            "response_types_supported": [],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["openid", "profile", "email"],
            "gateway_protocol_version": 1
        })
    );
}

#[tokio::test]
async fn full_device_and_refresh_flow_rotates_tokens() {
    let (config, _env) = GatewayEnv::config("happy");
    let (router, _, state) = build_router(config).unwrap();

    let (status, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let device_code = authorization["device_code"].as_str().unwrap();
    let user_code = authorization["user_code"].as_str().unwrap();

    let approval = format!("user_code={user_code}&login=dev%40example.com&secret=password");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/device")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::ORIGIN, "https://gateway.example")
                .body(Body::from(approval))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&html).contains("return to your device"));

    let (status, token) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id=claude-code"
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(token["token_type"], "Bearer");
    assert_eq!(token["expires_in"], 3600);
    let old_refresh = token["refresh_token"].as_str().unwrap();
    assert!(token["access_token"].as_str().unwrap().split('.').count() == 3);

    let (status, refreshed) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={old_refresh}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(refreshed["refresh_token"], old_refresh);

    let (status, error) = json_response(
        router,
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={old_refresh}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error, json!({"error": "invalid_grant"}));
    assert!(
        state.gateway_stores.device_grants.poll(device_code) == super::store::DevicePoll::Expired
    );
}

#[tokio::test]
async fn state_path_keeps_refresh_sessions_across_a_restart() {
    let (mut config, _env) = GatewayEnv::config("persist");
    let dir = std::env::temp_dir().join(format!(
        "shunt-gateway-restart-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    let state_file = dir.join("sessions.json");
    config.server.gateway.as_mut().unwrap().state_path = Some(state_file.clone());

    let (router, _, _state) = build_router(config.clone()).unwrap();
    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let device_code = authorization["device_code"].as_str().unwrap();
    let user_code = authorization["user_code"].as_str().unwrap();
    let approval = format!("user_code={user_code}&login=dev%40example.com&secret=password");
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/device")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::ORIGIN, "https://gateway.example")
                .body(Body::from(approval))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, token) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id=claude-code"
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let refresh_r1 = token["refresh_token"].as_str().unwrap().to_string();
    assert!(
        state_file.exists(),
        "the token grant writes the state file before responding"
    );

    // Rotate R1 -> R2 before the "restart" so the persisted file actually
    // contains a replay tombstone (R1) alongside the new active token (R2).
    let (status, rotated) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={refresh_r1}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let refresh_r2 = rotated["refresh_token"].as_str().unwrap().to_string();

    let on_disk = std::fs::read_to_string(&state_file).expect("read state file");
    assert!(
        !on_disk.contains(&refresh_r1) && !on_disk.contains(&refresh_r2),
        "the opaque refresh tokens must never be written to disk"
    );

    // "Restart": a fresh router owns fresh in-memory stores; restore from disk.
    let (restarted, _, restarted_state) = build_router(config).unwrap();
    crate::gateway::persist::restore(&restarted_state).await;
    let (status, refreshed) = json_response(
        restarted.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={refresh_r2}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "restored session refreshes: {refreshed}"
    );
    assert!(
        refreshed["access_token"]
            .as_str()
            .unwrap()
            .split('.')
            .count()
            == 3
    );
    assert_ne!(refreshed["refresh_token"], refresh_r2);

    // Replaying R1 — the tombstone created *before* the restart — is still
    // caught after the restore, which proves the tombstone itself (not just
    // the active token) survived the JSON round trip through the state file.
    // This also revokes the family, which is correct rotation semantics.
    let (status, error) = json_response(
        restarted,
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={refresh_r1}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error, json!({"error": "invalid_grant"}));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn device_grant_error_table_and_csrf_rejection_match_contract() {
    let (config, _env) = GatewayEnv::config("errors");
    let (router, _, state) = build_router(config).unwrap();

    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let device_code = authorization["device_code"].as_str().unwrap();
    let user_code = authorization["user_code"].as_str().unwrap();

    let (status, pending) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(pending, json!({"error": "authorization_pending"}));

    let (status, slow) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(slow, json!({"error": "slow_down"}));

    let response = router
        .clone()
        .oneshot(form_request(
            "/device",
            format!("user_code={user_code}&login=dev%40example.com&secret=password"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&html).contains("another site"));

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/device")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("sec-fetch-site", "cross-site")
                .body(Body::from(format!(
                    "user_code={user_code}&login=dev%40example.com&secret=password"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&html).contains("another site"));
    assert!(state.gateway_stores.device_grants.approve(
        user_code,
        Identity {
            sub: "dev@example.com".into(),
            email: "dev@example.com".into(),
            name: "dev".into(),
        }
    ));

    let (_, denied_authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let denied_device = denied_authorization["device_code"].as_str().unwrap();
    let denied_user = denied_authorization["user_code"].as_str().unwrap();
    assert!(state.gateway_stores.device_grants.deny(denied_user));
    let (status, denied) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={denied_device}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(denied, json!({"error": "access_denied"}));

    let (status, expired) = json_response(
        router,
        form_request(
            "/oauth/token",
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=unknown&client_id=claude-code",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(expired, json!({"error": "expired_token"}));
}

#[tokio::test]
async fn device_rate_limit_ignores_spoofed_forwarded_ips_by_default() {
    let (config, _env) = GatewayEnv::config("forwarded-default");
    let (router, _, _) = build_router(config).unwrap();
    let peer: std::net::SocketAddr = "203.0.113.4:43123".parse().unwrap();

    for attempt in 0..31 {
        let html = device_attempt(&router, peer, &format!("198.51.100.{attempt}")).await;
        if attempt < 30 {
            assert!(html.contains("login or secret"));
        } else {
            assert!(html.contains("Too many attempts"));
        }
    }
}

#[tokio::test]
async fn device_rate_limit_honors_forwarded_ips_when_enabled() {
    let (mut config, _env) = GatewayEnv::config("forwarded-opt-in");
    config.server.gateway.as_mut().unwrap().trust_forwarded_for = true;
    let (router, _, _) = build_router(config).unwrap();
    let peer: std::net::SocketAddr = "203.0.113.4:43123".parse().unwrap();

    for attempt in 0..31 {
        let html = device_attempt(&router, peer, &format!("198.51.100.{attempt}")).await;
        assert!(html.contains("login or secret"));
    }
}

#[tokio::test]
async fn malformed_oauth_forms_use_rfc6749_error_shape() {
    let (config, _env) = GatewayEnv::config("malformed-forms");
    let (router, _, _) = build_router(config).unwrap();

    for (path, body) in [
        ("/oauth/device_authorization", ""),
        ("/oauth/device_authorization", "client_id=other"),
        ("/oauth/token", ""),
        (
            "/oauth/token",
            "grant_type=refresh_token&client_id=claude-code",
        ),
        (
            "/oauth/token",
            "grant_type=refresh_token&refresh_token=value&client_id=other",
        ),
    ] {
        let (status, error) = json_response(router.clone(), form_request(path, body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error, json!({"error": "invalid_request"}));
    }

    for path in ["/oauth/device_authorization", "/oauth/token"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error": "invalid_request"})
        );
    }
}

#[tokio::test]
async fn routes_are_absent_without_gateway_config() {
    let (router, _, _) = build_router(Config::default()).unwrap();
    for path in ["/.well-known/oauth-authorization-server", "/device"] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn gateway_jwt_and_static_client_token_compose_on_models() {
    let (mut config, _env) = GatewayEnv::config("composition");
    let auth_env = format!("SHUNT_GATEWAY_TEST_CLIENT_{}", std::process::id());
    std::env::set_var(&auth_env, "static:static-token");
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".into(),
        tokens_env: auth_env.clone(),
    });
    config.models = vec![ModelConfig {
        id: "claude-via-gateway".into(),
        display_name: None,
    }];
    let (router, _, _) = build_router(config).unwrap();

    let identity = Identity {
        sub: "dev@example.com".into(),
        email: "dev@example.com".into(),
        name: "dev".into(),
    };
    let bearer = jwt::mint(
        &identity,
        "https://gateway.example",
        b"0123456789abcdef0123456789abcdef",
        3600,
    );
    let (status, body) = json_response(
        router.clone(),
        Request::builder()
            .uri("/v1/models")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["id"], "claude-via-gateway");

    let (status, _) = json_response(
        router.clone(),
        Request::builder()
            .uri("/v1/models")
            .header("x-shunt-token", "static-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = json_response(
        router,
        Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    std::env::remove_var(auth_env);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn gateway_jwt_is_accepted_on_mapped_messages() {
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
            ),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;

    let (mut config, _env) = GatewayEnv::config("messages");
    let upstream_key_env = format!("SHUNT_GATEWAY_TEST_UPSTREAM_KEY_{}", std::process::id());
    std::env::set_var(&upstream_key_env, "upstream-key");
    let provider = config.providers.get_mut("openai").unwrap();
    provider.base_url = upstream.uri();
    provider.api_key_env = Some(upstream_key_env.clone());
    config.routes = vec![RouteConfig {
        model: "gateway-model".into(),
        provider: "openai".into(),
        upstream_model: None,
        effort: None,
    }];
    let (router, _, _) = build_router(config).unwrap();
    let identity = Identity {
        sub: "dev@example.com".into(),
        email: "dev@example.com".into(),
        name: "dev".into(),
    };
    let bearer = jwt::mint(
        &identity,
        "https://gateway.example",
        b"0123456789abcdef0123456789abcdef",
        3600,
    );
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "gateway-model",
                        "max_tokens": 16,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    std::env::remove_var(upstream_key_env);

    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].headers.get("x-shunt-inbound-client").is_none(),
        "gateway identity must remain local and not be forwarded upstream"
    );
}

#[tokio::test]
async fn managed_policy_matching_merges_catch_alls_and_uses_first_user_match() {
    let (mut config, _env) = GatewayEnv::config("managed-matching");
    config.server.gateway.as_mut().unwrap().policies = Some(vec![
        policy(None, toml::toml! { env = { BASE = "1", SHARED = "first" } }),
        policy(
            None,
            toml::toml! { env = { SECOND = "1", SHARED = "second" } },
        ),
        policy(
            Some(vec!["alice@example.com"]),
            toml::toml! { marker = "first-match" },
        ),
        policy(
            Some(vec!["alice@example.com"]),
            toml::toml! { marker = "must-not-win" },
        ),
    ]);
    let (router, _, _) = build_router(config).unwrap();

    let alice = gateway_bearer("alice@example.com");
    let (status, body) = json_response(router.clone(), managed_request(Some(&alice), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["settings"]["marker"], "first-match");
    assert_eq!(
        body["settings"]["env"],
        json!({"BASE": "1", "SECOND": "1", "SHARED": "second"})
    );

    let bob = gateway_bearer("bob@example.com");
    let (_, body) = json_response(router, managed_request(Some(&bob), None)).await;
    assert!(body["settings"].get("marker").is_none());
    assert_eq!(body["settings"]["env"]["BASE"], "1");
}

#[tokio::test]
async fn managed_policy_email_matching_is_exact_and_case_sensitive() {
    let (mut config, _env) = GatewayEnv::config("managed-case-sensitive-email");
    config.server.gateway.as_mut().unwrap().policies = Some(vec![policy(
        Some(vec!["alice@example.com"]),
        toml::toml! { marker = "lowercase-match-only" },
    )]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("Alice@example.com");

    let (status, body) = json_response(router, managed_request(Some(&bearer), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["settings"], json!({}));
}

#[tokio::test]
async fn managed_policy_without_catch_all_serves_match_or_empty_document() {
    let (mut config, _env) = GatewayEnv::config("managed-no-catch-all");
    config.server.gateway.as_mut().unwrap().policies = Some(vec![policy(
        Some(vec!["alice@example.com"]),
        toml::toml! { availableModels = ["allowed"] },
    )]);
    let (router, _, _) = build_router(config).unwrap();

    let alice = gateway_bearer("alice@example.com");
    let (_, alice_body) = json_response(router.clone(), managed_request(Some(&alice), None)).await;
    assert_eq!(
        alice_body["settings"]["availableModels"],
        json!(["allowed"])
    );

    let bob = gateway_bearer("bob@example.com");
    let (status, bob_body) = json_response(router, managed_request(Some(&bob), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bob_body["settings"], json!({}));
}

#[tokio::test]
async fn managed_settings_injects_telemetry_env_and_policy_wins() {
    let (mut config, _env) = GatewayEnv::config("managed-telemetry");
    let gateway = config.server.gateway.as_mut().unwrap();
    gateway.policies = Some(vec![policy(
        None,
        toml::toml! { env = { OTEL_METRICS_EXPORTER = "policy", CUSTOM = "yes" } },
    )]);
    gateway.telemetry = Some(GatewayTelemetryConfig {
        forward_to: vec![GatewayTelemetryDestination {
            url: "https://collector.example".to_string(),
            headers: None,
        }],
    });
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let (_, body) = json_response(router, managed_request(Some(&bearer), None)).await;
    assert_eq!(
        body["settings"]["env"],
        json!({
            "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
            "OTEL_METRICS_EXPORTER": "policy",
            "OTEL_LOGS_EXPORTER": "otlp",
            "OTEL_TRACES_EXPORTER": "otlp",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "https://gateway.example",
            "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
            "CUSTOM": "yes"
        })
    );
}

#[tokio::test]
async fn managed_settings_wire_has_hashes_etag_and_lenient_304() {
    let (mut config, _env) = GatewayEnv::config("managed-wire");
    config.server.gateway.as_mut().unwrap().policies =
        Some(vec![policy(None, toml::Value::Table(toml::Table::new()))]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let response = router
        .clone()
        .oneshot(managed_request(Some(&bearer), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["uuid"].as_str().unwrap().starts_with("sha256:"));
    assert!(body["checksum"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(body["settings"], json!({}));
    let settings_bytes = serde_json::to_vec(&body["settings"]).unwrap();
    assert_eq!(
        body["checksum"],
        format!("sha256:{:x}", Sha256::digest(settings_bytes))
    );
    assert_eq!(etag, format!("\"{}\"", body["checksum"].as_str().unwrap()));

    let legacy_etag = body["checksum"].as_str().unwrap().to_string();
    for candidate in [
        etag.clone(),
        legacy_etag,
        format!("W/{etag}"),
        format!("\"other\", {etag}"),
        "*".to_string(),
    ] {
        let response = router
            .clone()
            .oneshot(managed_request(Some(&bearer), Some(&candidate)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers().get(header::ETAG).unwrap(), &etag);
        assert!(to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty());
    }
    let response = router
        .oneshot(managed_request(Some(&bearer), Some("sha256:different")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn managed_settings_rejects_missing_bearer_and_reports_no_policy() {
    let (config, _env) = GatewayEnv::config("managed-errors");
    let (router, _, _) = build_router(config).unwrap();

    let (status, body) = json_response(router.clone(), managed_request(None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");

    let bearer = gateway_bearer("dev@example.com");
    let (status, body) = json_response(router, managed_request(Some(&bearer), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found_error");
    assert_eq!(body["error"]["message"], "no managed policy");
}

#[tokio::test]
async fn managed_settings_uuid_is_derived_from_subject_not_email() {
    let (mut config, _env) = GatewayEnv::config("managed-uuid");
    config.server.gateway.as_mut().unwrap().policies =
        Some(vec![policy(None, toml::Value::Table(toml::Table::new()))]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = |sub: &str, email: &str| {
        jwt::mint(
            &Identity {
                sub: sub.to_string(),
                email: email.to_string(),
                name: "managed user".to_string(),
            },
            "https://gateway.example",
            b"0123456789abcdef0123456789abcdef",
            3600,
        )
    };
    let first = bearer("stable-subject", "alice@example.com");
    let same_subject = bearer("stable-subject", "renamed@example.com");
    let other_subject = bearer("different-subject", "alice@example.com");

    let (_, first) = json_response(router.clone(), managed_request(Some(&first), None)).await;
    let (_, repeated) =
        json_response(router.clone(), managed_request(Some(&same_subject), None)).await;
    let (_, other) = json_response(router, managed_request(Some(&other_subject), None)).await;

    assert_eq!(first["uuid"], repeated["uuid"]);
    assert_ne!(first["uuid"], other["uuid"]);
}

#[tokio::test]
async fn managed_settings_rejects_malformed_bearer_headers() {
    let (mut config, _env) = GatewayEnv::config("managed-auth-errors");
    config.server.gateway.as_mut().unwrap().policies =
        Some(vec![policy(None, toml::Value::Table(toml::Table::new()))]);
    let (router, _, _) = build_router(config).unwrap();
    let wrong_signature = jwt::mint(
        &Identity {
            sub: "dev@example.com".into(),
            email: "dev@example.com".into(),
            name: "dev".into(),
        },
        "https://gateway.example",
        b"abcdef0123456789abcdef0123456789",
        3600,
    );

    for authorization in [
        "Basic abc".to_string(),
        "Bearer ".to_string(),
        "Bearer garbage.jwt".to_string(),
        format!("Bearer {wrong_signature}"),
    ] {
        let (status, body) = json_response(
            router.clone(),
            Request::builder()
                .uri("/managed/settings")
                .header(header::AUTHORIZATION, authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "authentication_error");
    }
}

#[tokio::test]
async fn managed_settings_empty_telemetry_targets_do_not_inject_env() {
    let (mut config, _env) = GatewayEnv::config("managed-empty-telemetry");
    let gateway = config.server.gateway.as_mut().unwrap();
    gateway.policies = Some(vec![policy(None, toml::toml! { env = { POLICY = "yes" } })]);
    gateway.telemetry = Some(GatewayTelemetryConfig {
        forward_to: Vec::new(),
    });
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let (_, body) = json_response(router, managed_request(Some(&bearer), None)).await;
    assert_eq!(body["settings"]["env"], json!({"POLICY": "yes"}));
}

#[test]
fn managed_policy_deserializes_from_real_toml() {
    let (dir, path, suffix) = temp_config_path("managed-toml");
    let secret_env = format!("SHUNT_REAL_TOML_SECRET_{suffix}");
    let users_env = format!("SHUNT_REAL_TOML_USERS_{suffix}");
    std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
    std::env::set_var(&users_env, "dev@example.com:password");
    std::fs::write(
        &path,
        format!(
            r#"
[server.gateway]
public_url = "https://gateway.example"
jwt_secret_env = "{secret_env}"
users_env = "{users_env}"

[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["dev@example.com"]
[server.gateway.policies.cli]
availableModels = ["allowed"]
[server.gateway.policies.cli.env]
CUSTOM = "yes"
[server.gateway.policies.cli.permissions]
allow = ["Read"]
"#
        ),
    )
    .unwrap();

    let config = Config::load(Some(&path)).unwrap();
    let gateway = config.server.gateway.as_ref().unwrap();
    let policy = &gateway.policies.as_ref().unwrap()[0];
    assert_eq!(
        policy.matcher.as_ref().unwrap().emails.as_deref(),
        Some(["dev@example.com".to_string()].as_slice())
    );
    assert_eq!(policy.cli["availableModels"].as_array().unwrap().len(), 1);
    assert_eq!(policy.cli["env"]["CUSTOM"].as_str(), Some("yes"));
    assert_eq!(policy.cli["permissions"]["allow"][0].as_str(), Some("Read"));
    assert!(config.resolve_gateway_auth().unwrap().is_some());

    std::env::remove_var(secret_env);
    std::env::remove_var(users_env);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn managed_settings_hot_reload_serves_new_policy_and_etag() {
    let (dir, path, suffix) = temp_config_path("managed-reload");
    let secret_env = format!("SHUNT_MANAGED_RELOAD_SECRET_{suffix}");
    let users_env = format!("SHUNT_MANAGED_RELOAD_USERS_{suffix}");
    std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
    std::env::set_var(&users_env, "dev@example.com:password");
    let document = |model: &str, telemetry: bool| {
        let telemetry = if telemetry {
            "\n[server.gateway.telemetry]\n[[server.gateway.telemetry.forward_to]]\nurl = \"https://collector.example\"\n"
        } else {
            ""
        };
        format!(
            "[server.gateway]\npublic_url = \"https://gateway.example\"\njwt_secret_env = \"{secret_env}\"\nusers_env = \"{users_env}\"\n\n[[server.gateway.policies]]\n[server.gateway.policies.cli]\navailableModels = [\"{model}\"]\n{telemetry}"
        )
    };
    std::fs::write(&path, document("first", false)).unwrap();
    let (router, shared, _) = build_router(Config::load(Some(&path)).unwrap()).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let response = router
        .clone()
        .oneshot(managed_request(Some(&bearer), None))
        .await
        .unwrap();
    let first_etag = response.headers()[header::ETAG].clone();
    let first: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(first["settings"]["availableModels"], json!(["first"]));
    assert!(first["settings"].get("env").is_none());

    std::fs::write(&path, document("second", true)).unwrap();
    crate::reload::reload(&shared, Some(&path)).unwrap();
    let response = router
        .oneshot(managed_request(Some(&bearer), None))
        .await
        .unwrap();
    assert_ne!(response.headers()[header::ETAG], first_etag);
    let second: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(second["settings"]["availableModels"], json!(["second"]));
    assert_eq!(second["settings"]["env"]["OTEL_METRICS_EXPORTER"], "otlp");

    std::env::remove_var(secret_env);
    std::env::remove_var(users_env);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn managed_settings_without_telemetry_does_not_inject_env() {
    let (mut config, _env) = GatewayEnv::config("managed-no-telemetry");
    config.server.gateway.as_mut().unwrap().policies =
        Some(vec![policy(None, toml::Value::Table(toml::Table::new()))]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let (_, body) = json_response(router, managed_request(Some(&bearer), None)).await;
    assert!(body["settings"].get("env").is_none());
}

#[tokio::test]
async fn empty_available_models_denies_all_gateway_inference_routes() {
    use wiremock::MockServer;

    let upstream = MockServer::start().await;
    let (mut config, _env) = GatewayEnv::config("managed-deny-all");
    let key_env = format!("SHUNT_GATEWAY_DENY_ALL_{}", std::process::id());
    std::env::set_var(&key_env, "upstream-key");
    let provider = config.providers.get_mut("openai").unwrap();
    provider.base_url = upstream.uri();
    provider.api_key_env = Some(key_env.clone());
    config.routes = vec![RouteConfig {
        model: "blocked".to_string(),
        provider: "openai".to_string(),
        upstream_model: None,
        effort: None,
    }];
    config.server.gateway.as_mut().unwrap().policies =
        Some(vec![policy(None, toml::toml! { availableModels = [] })]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");
    let body = json!({
        "model": "blocked",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();

    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let (status, response) = json_response(
            router.clone(),
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"]["type"], "invalid_request_error");
    }

    std::env::remove_var(key_env);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn available_models_policy_denies_or_allows_gateway_requests() {
    let upstream = responses_upstream().await;
    let (mut config, _env) = GatewayEnv::config("managed-enforcement");
    let key_env = format!("SHUNT_GATEWAY_POLICY_UPSTREAM_{}", std::process::id());
    let client_env = format!("SHUNT_GATEWAY_POLICY_CLIENT_{}", std::process::id());
    std::env::set_var(&key_env, "upstream-key");
    std::env::set_var(&client_env, "static-user:static-token");
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: client_env.clone(),
    });
    let provider = config.providers.get_mut("openai").unwrap();
    provider.base_url = upstream.uri();
    provider.api_key_env = Some(key_env.clone());
    config.routes = ["allowed", "denied"]
        .into_iter()
        .map(|model| RouteConfig {
            model: model.to_string(),
            provider: "openai".to_string(),
            upstream_model: Some("upstream-allowed".to_string()),
            effort: None,
        })
        .collect();
    config.server.gateway.as_mut().unwrap().policies = Some(vec![policy(
        None,
        toml::toml! { availableModels = ["allowed"] },
    )]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let (status, body) = json_response(
        router.clone(),
        inference_request(
            "/v1/messages",
            (header::AUTHORIZATION.as_str(), &format!("Bearer {bearer}")),
            "denied",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("denied"));

    let response = router
        .clone()
        .oneshot(inference_request(
            "/v1/messages",
            (header::AUTHORIZATION.as_str(), &format!("Bearer {bearer}")),
            "allowed[1m]",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (status, body) = json_response(
        router.clone(),
        inference_request(
            "/v1/messages/count_tokens",
            (header::AUTHORIZATION.as_str(), &format!("Bearer {bearer}")),
            "denied",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request_error");

    let response = router
        .oneshot(inference_request(
            "/v1/messages",
            ("x-shunt-token", "static-token"),
            "denied",
        ))
        .await
        .unwrap();
    std::env::remove_var(key_env);
    std::env::remove_var(client_env);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn gateway_policy_without_available_models_is_unrestricted() {
    let upstream = responses_upstream().await;
    let (mut config, _env) = GatewayEnv::config("managed-unrestricted");
    let key_env = format!("SHUNT_GATEWAY_UNRESTRICTED_{}", std::process::id());
    std::env::set_var(&key_env, "upstream-key");
    let provider = config.providers.get_mut("openai").unwrap();
    provider.base_url = upstream.uri();
    provider.api_key_env = Some(key_env.clone());
    config.routes = vec![RouteConfig {
        model: "any-model".to_string(),
        provider: "openai".to_string(),
        upstream_model: None,
        effort: None,
    }];
    config.server.gateway.as_mut().unwrap().policies =
        Some(vec![policy(None, toml::toml! { env = { TEST = "1" } })]);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");
    let response = router
        .oneshot(inference_request(
            "/v1/messages",
            (header::AUTHORIZATION.as_str(), &format!("Bearer {bearer}")),
            "any-model",
        ))
        .await
        .unwrap();
    std::env::remove_var(key_env);
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn app_state_can_resolve_gateway_snapshot() {
    let (config, _env) = GatewayEnv::config("state");
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    assert!(state.gateway_auth.is_some());
}
