use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{
    config::{Config, GatewayConfig, InboundAuthConfig, ModelConfig, RouteConfig},
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

fn form_request(path: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.into()))
        .unwrap()
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
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}"
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
            format!("grant_type=refresh_token&refresh_token={old_refresh}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(refreshed["refresh_token"], old_refresh);

    let (status, error) = json_response(
        router,
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={old_refresh}"),
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
async fn device_grant_error_table_and_csrf_rejection_match_contract() {
    let (config, _env) = GatewayEnv::config("errors");
    let (router, _, state) = build_router(config).unwrap();

    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", ""),
    )
    .await;
    let device_code = authorization["device_code"].as_str().unwrap();
    let user_code = authorization["user_code"].as_str().unwrap();

    let (status, pending) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(pending, json!({"error": "authorization_pending"}));

    let (status, slow) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}"),
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
                .header(header::CONTENT_TYPE, "application/json")
                .header("sec-fetch-site", "cross-site")
                .body(Body::from("{}"))
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
        form_request("/oauth/device_authorization", ""),
    )
    .await;
    let denied_device = denied_authorization["device_code"].as_str().unwrap();
    let denied_user = denied_authorization["user_code"].as_str().unwrap();
    assert!(state.gateway_stores.device_grants.deny(denied_user));
    let (status, denied) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={denied_device}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(denied, json!({"error": "access_denied"}));

    let (status, expired) = json_response(
        router,
        form_request(
            "/oauth/token",
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=unknown",
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
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/device")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::ORIGIN, "https://gateway.example")
                    .header("x-forwarded-for", format!("198.51.100.{attempt}"))
                    .extension(ConnectInfo(peer))
                    .body(Body::from(
                        "user_code=BCDF-GHJK&login=dev%40example.com&secret=wrong",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8_lossy(&body);
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
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/device")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::ORIGIN, "https://gateway.example")
                    .header("x-forwarded-for", format!("198.51.100.{attempt}"))
                    .extension(ConnectInfo(peer))
                    .body(Body::from(
                        "user_code=BCDF-GHJK&login=dev%40example.com&secret=wrong",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("login or secret"));
    }
}

#[tokio::test]
async fn malformed_oauth_forms_use_rfc6749_error_shape() {
    let (config, _env) = GatewayEnv::config("malformed-forms");
    let (router, _, _) = build_router(config).unwrap();

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
}

#[test]
fn app_state_can_resolve_gateway_snapshot() {
    let (config, _env) = GatewayEnv::config("state");
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    assert!(state.gateway_auth.is_some());
}
