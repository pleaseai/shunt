use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{
    config::{
        Config, GatewayConfig, GatewayOidcConfig, InboundAuthConfig, ModelConfig, RouteConfig,
    },
    server::{build_router, AppState},
};

use super::{approval::Identity, jwt};

struct GatewayEnv {
    secret_env: String,
    users_env: String,
    oidc_secret_env: Option<String>,
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
            state_path: None,
            oidc: None,
        });
        (
            config,
            Self {
                secret_env,
                users_env,
                oidc_secret_env: None,
            },
        )
    }
    fn oidc_config(label: &str, issuer: String, users: bool) -> (Config, Self) {
        let (mut config, mut env) = Self::config(label);
        let oidc_secret_env = format!(
            "SHUNT_GATEWAY_TEST_OIDC_SECRET_{}_{}",
            std::process::id(),
            label
        );
        std::env::set_var(&oidc_secret_env, "client-secret");
        if !users {
            std::env::remove_var(&env.users_env);
        }
        config.server.gateway.as_mut().unwrap().oidc = Some(GatewayOidcConfig {
            issuer,
            client_id: "client-id".into(),
            client_secret_env: oidc_secret_env.clone(),
            allowed_domains: vec!["example.com".into()],
            allowed_emails: vec![],
            scopes: vec![],
            authorization_endpoint: None,
            token_endpoint: None,
            userinfo_endpoint: None,
        });
        env.oidc_secret_env = Some(oidc_secret_env);
        (config, env)
    }
}

impl Drop for GatewayEnv {
    fn drop(&mut self) {
        std::env::remove_var(&self.secret_env);
        std::env::remove_var(&self.users_env);
        if let Some(env) = &self.oidc_secret_env {
            std::env::remove_var(env);
        }
    }
}

async fn json_response(router: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&body).expect("JSON response");
    (status, value)
}

async fn html_response(router: Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn get_request(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
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
async fn oidc_device_page_modes_and_disabled_password_post() {
    let (config, _env) = GatewayEnv::oidc_config(
        "oidc-page-only",
        "https://accounts.google.com".into(),
        false,
    );
    let (router, _, _) = build_router(config).unwrap();
    let (_, html) = html_response(router.clone(), get_request("/device?user_code=BCDF-GHJK")).await;
    assert!(html.contains("Sign in with Google"));
    assert!(html.contains("method=\"get\" action=\"/device/authorize\""));
    assert!(!html.contains("Approve device"));

    let (_, html) = html_response(
        router,
        Request::builder()
            .method("POST")
            .uri("/device")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, "https://gateway.example")
            .body(Body::from("user_code=BCDF-GHJK&login=x&secret=y"))
            .unwrap(),
    )
    .await;
    assert!(html.contains("Password sign-in is disabled"));

    let (config, _env) = GatewayEnv::oidc_config(
        "oidc-page-both",
        "https://accounts.example.com".into(),
        true,
    );
    let (router, _, _) = build_router(config).unwrap();
    let (_, html) = html_response(router, get_request("/device")).await;
    assert!(html.contains("Sign in with SSO"));
    assert!(html.contains("Approve device"));
}

#[tokio::test]
async fn oidc_authorize_rejects_unknown_code_and_builds_pkce_redirect() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};

    let idp = MockServer::start().await;
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{}/authorize", idp.uri()),
            "token_endpoint": format!("{}/token", idp.uri()),
            "userinfo_endpoint": format!("{}/userinfo", idp.uri())
        })))
        .mount(&idp)
        .await;
    let (config, _env) = GatewayEnv::oidc_config("oidc-authorize", idp.uri(), false);
    let (router, _, state) = build_router(config).unwrap();

    let (_, html) = html_response(
        router.clone(),
        get_request("/device/authorize?user_code=NOPE-CODE"),
    )
    .await;
    assert!(html.contains("invalid, expired, or already used"));

    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let user_code = authorization["user_code"].as_str().unwrap();
    let response = router
        .oneshot(get_request(&format!(
            "/device/authorize?user_code={user_code}"
        )))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let location =
        reqwest::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(location.path(), "/authorize");
    let params: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "client-id");
    assert_eq!(
        params["redirect_uri"],
        "https://gateway.example/device/callback"
    );
    assert_eq!(params["scope"], "openid email profile");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(!params["code_challenge"].is_empty());
    let pending = state
        .gateway_stores
        .oidc_states
        .take(&params["state"])
        .unwrap();
    assert_eq!(pending.user_code, user_code);
}

#[tokio::test]
async fn oidc_callback_completes_device_flow_and_enforces_allowlist() {
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    let idp = MockServer::start().await;
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{}/authorize", idp.uri()),
            "token_endpoint": format!("{}/token", idp.uri()),
            "userinfo_endpoint": format!("{}/userinfo", idp.uri())
        })))
        .mount(&idp)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sub":"subject-1", "email":"Dev@Example.com", "email_verified":true, "name":"Developer"
        })))
        .mount(&idp)
        .await;
    let (config, _env) = GatewayEnv::oidc_config("oidc-e2e", idp.uri(), false);
    let (router, _, _) = build_router(config).unwrap();
    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let user_code = authorization["user_code"].as_str().unwrap();
    let device_code = authorization["device_code"].as_str().unwrap();
    let authorize = router
        .clone()
        .oneshot(get_request(&format!(
            "/device/authorize?user_code={user_code}"
        )))
        .await
        .unwrap();
    let location =
        reqwest::Url::parse(authorize.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let params: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    let state = &params["state"];
    let (_, html) = html_response(
        router.clone(),
        get_request(&format!("/device/callback?code=auth-code&state={state}")),
    )
    .await;
    assert!(html.contains("return to your device"));
    let requests = idp.received_requests().await.unwrap();
    let token = requests
        .iter()
        .find(|request| request.url.path() == "/token")
        .unwrap();
    let body = String::from_utf8(token.body.clone()).unwrap();
    let form_url = reqwest::Url::parse(&format!("https://form.invalid/?{body}")).unwrap();
    let form: std::collections::HashMap<_, _> = form_url.query_pairs().into_owned().collect();
    assert_eq!(
        form["redirect_uri"],
        "https://gateway.example/device/callback"
    );
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
        params["code_challenge"]
    );

    let (status, token) = json_response(
        router,
        form_request(
            "/oauth/token",
            format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id=claude-code"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(token["token_type"], "Bearer");
}

#[tokio::test]
async fn oidc_callback_rejects_bad_state_idp_error_and_unverified_email() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};

    let idp = MockServer::start().await;
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{}/authorize", idp.uri()),
            "token_endpoint": format!("{}/token", idp.uri()),
            "userinfo_endpoint": format!("{}/userinfo", idp.uri())
        })))
        .mount(&idp)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .mount(&idp)
        .await;
    Mock::given(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sub":"subject-1", "email":"dev@example.com", "email_verified":false
        })))
        .mount(&idp)
        .await;
    let (config, _env) = GatewayEnv::oidc_config("oidc-reject", idp.uri(), false);
    let (router, _, _) = build_router(config).unwrap();
    let (_, html) = html_response(
        router.clone(),
        get_request("/device/callback?code=x&state=bad"),
    )
    .await;
    assert!(html.contains("invalid or has expired"));
    let (_, html) = html_response(
        router.clone(),
        get_request("/device/callback?error=denied&state=x"),
    )
    .await;
    assert!(html.contains("identity provider reported an error"));
    assert!(!html.contains("denied"));

    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let user_code = authorization["user_code"].as_str().unwrap();
    let authorize = router
        .clone()
        .oneshot(get_request(&format!(
            "/device/authorize?user_code={user_code}"
        )))
        .await
        .unwrap();
    let location =
        reqwest::Url::parse(authorize.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let state = location
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let (_, html) = html_response(
        router.clone(),
        get_request(&format!("/device/callback?code=x&state={state}")),
    )
    .await;
    assert!(html.contains("unavailable right now"));
    let (_, reused) = html_response(
        router,
        get_request(&format!("/device/callback?code=x&state={state}")),
    )
    .await;
    assert!(reused.contains("invalid or has expired"));
}

#[tokio::test]
async fn oidc_callback_uses_endpoint_overrides_and_exact_email_allowlist() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};

    let idp = MockServer::start().await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .mount(&idp)
        .await;
    Mock::given(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sub":"subject-override",
            "email":"exact@outside.test",
            "email_verified":true,
            "name":null
        })))
        .mount(&idp)
        .await;
    let (mut config, _env) = GatewayEnv::oidc_config("oidc-overrides", idp.uri(), false);
    let oidc = config
        .server
        .gateway
        .as_mut()
        .unwrap()
        .oidc
        .as_mut()
        .unwrap();
    oidc.allowed_domains.clear();
    oidc.allowed_emails = vec!["exact@outside.test".into()];
    oidc.authorization_endpoint = Some(format!("{}/authorize", idp.uri()));
    oidc.token_endpoint = Some(format!("{}/token", idp.uri()));
    oidc.userinfo_endpoint = Some(format!("{}/userinfo", idp.uri()));
    let (router, _, state) = build_router(config).unwrap();
    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let user_code = authorization["user_code"].as_str().unwrap();
    let device_code = authorization["device_code"].as_str().unwrap();
    let authorize = router
        .clone()
        .oneshot(get_request(&format!(
            "/device/authorize?user_code={user_code}"
        )))
        .await
        .unwrap();
    let location =
        reqwest::Url::parse(authorize.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(location.path(), "/authorize");
    let state_param = location
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let (_, html) = html_response(
        router,
        get_request(&format!("/device/callback?code=x&state={state_param}")),
    )
    .await;
    assert!(html.contains("return to your device"));
    match state.gateway_stores.device_grants.poll(device_code) {
        super::store::DevicePoll::Approved(identity) => {
            assert_eq!(identity.sub, "subject-override");
            assert_eq!(identity.email, "exact@outside.test");
            assert_eq!(identity.name, "exact");
        }
        other => panic!("expected approved identity, got {other:?}"),
    }
    assert!(idp
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.url.path() != "/.well-known/openid-configuration"));
}

#[tokio::test]
async fn oidc_callback_rejects_email_outside_allowlist() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};

    let idp = MockServer::start().await;
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{}/authorize", idp.uri()),
            "token_endpoint": format!("{}/token", idp.uri()),
            "userinfo_endpoint": format!("{}/userinfo", idp.uri())
        })))
        .mount(&idp)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .mount(&idp)
        .await;
    Mock::given(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sub":"subject-2", "email":"user@outside.test", "email_verified":true
        })))
        .mount(&idp)
        .await;
    let (config, _env) = GatewayEnv::oidc_config("oidc-allowlist", idp.uri(), false);
    let (router, _, _) = build_router(config).unwrap();
    let (_, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", "client_id=claude-code"),
    )
    .await;
    let user_code = authorization["user_code"].as_str().unwrap();
    let authorize = router
        .clone()
        .oneshot(get_request(&format!(
            "/device/authorize?user_code={user_code}"
        )))
        .await
        .unwrap();
    let location =
        reqwest::Url::parse(authorize.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let state = location
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let (_, html) = html_response(
        router,
        get_request(&format!("/device/callback?code=x&state={state}")),
    )
    .await;
    assert!(html.contains("not authorized for this gateway"));
    assert!(!html.contains("outside.test"));
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
    for path in [
        "/.well-known/oauth-authorization-server",
        "/device",
        "/device/authorize",
        "/device/callback",
    ] {
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

#[test]
fn app_state_can_resolve_gateway_snapshot() {
    let (config, _env) = GatewayEnv::config("state");
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    assert!(state.gateway_auth.is_some());
}
