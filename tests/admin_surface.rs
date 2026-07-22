//! M9 admin web surface — end-to-end gateway behavior.
//!
//! The admin routes exist only when `[server.admin]` is configured, authenticate
//! every request against a separate admin credential, and never return or log the
//! provisioned OAuth credentials. Setup-token and full refreshable OAuth flows
//! are driven against a wiremock Claude token endpoint.

use std::{net::SocketAddr, path::PathBuf, time::SystemTime};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, AdminConfig, AdminOidcConfig, AuthMode, Config, OidcProviderConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_partial_json, method, path},
    Mock, MockServer, ResponseTemplate,
};

/// Serializes tests that mutate the shared `SHUNT_CLAUDE_*` process env. A tokio
/// mutex (held across `.await`) so it is safe over the async request calls.
static CLAUDE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Serializes tests that mutate the shared `SHUNT_CODEX_*` process env.
static CODEX_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Serializes admin OIDC tests because their config resolves process environment.
static ADMIN_OIDC_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Gateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

/// A config with `[server.admin]` enabled and the default `anthropic` provider
/// flipped to `claude_oauth` with an empty accounts list, so `/admin/pool`
/// enumerates the store and a completed add is "live now".
fn admin_config(tokens_env: &str) -> Config {
    let mut config = Config::default();
    let anthropic = config.providers.get_mut("anthropic").unwrap();
    anthropic.auth = AuthMode::ClaudeOauth;
    anthropic.accounts = Vec::new();
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: tokens_env.to_string(),
        tokens_file: None,
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        oidc: None,
    });
    config
}

async fn start_with_addr(mut config: Config) -> Gateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    if let Some(oidc) = config
        .server
        .admin
        .as_mut()
        .and_then(|admin| admin.oidc.as_mut())
    {
        oidc.public_url = format!("http://{addr}");
    }
    let (app, _shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

async fn start(config: Config) -> Gateway {
    start_with_addr(config).await
}

async fn start_with_state(mut config: Config) -> (Gateway, shunt::server::AppState) {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        Gateway {
            base_url: format!("http://{addr}"),
            task,
        },
        state,
    )
}

/// Monotonic counter appended to `unique_dir()` names: parallel test threads can
/// call `SystemTime::now()` within the same tick on some platforms, so the
/// nanosecond timestamp alone is not a reliable uniqueness guarantee under the
/// full test-suite's concurrency.
static UNIQUE_DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unique_dir() -> PathBuf {
    let counter = UNIQUE_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "shunt-admin-test-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        counter
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn chatgpt_token(exp: u64, account_id: &str) -> String {
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn chatgpt_token_without_account_id(exp: u64) -> String {
    let payload = serde_json::json!({"exp": exp});
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn authorize_state(body: &serde_json::Value) -> (reqwest::Url, String) {
    let url = reqwest::Url::parse(body["authorize_url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("authorize URL carries OAuth state");
    (url, state)
}

fn admin_oidc_config(tokens_env: &str, secret_env: &str, idp: &MockServer) -> Config {
    let mut config = admin_config(tokens_env);
    config.server.admin.as_mut().unwrap().oidc = Some(AdminOidcConfig {
        public_url: "http://127.0.0.1:1".into(),
        client_secret_env: secret_env.into(),
        provider: OidcProviderConfig {
            issuer: idp.uri(),
            client_id: "admin-client".into(),
            allowed_domains: vec!["example.com".into()],
            allowed_emails: vec![],
            scopes: vec![],
            authorization_endpoint: Some(format!("{}/authorize", idp.uri())),
            token_endpoint: Some(format!("{}/token", idp.uri())),
            userinfo_endpoint: Some(format!("{}/userinfo", idp.uri())),
        },
    });
    config
}

fn response_cookie(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("shunt_admin_session="))
        .map(|value| value.split(';').next().unwrap().to_string())
}

async fn oidc_state(client: &reqwest::Client, gateway: &Gateway) -> (reqwest::Url, String) {
    let response = client
        .post(format!("{}/admin/oidc/start", gateway.base_url))
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let location = reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let state = location
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("OIDC redirect carries state");
    (location, state)
}

#[tokio::test]
async fn admin_oidc_full_flow_mints_session_and_preserves_header_auth() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = ADMIN_OIDC_ENV_LOCK.lock().await;
    let idp = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token":"oidc-access"
        })))
        .expect(1)
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sub":"admin-subject", "email":"Admin@Example.com", "email_verified":true,
            "name":"Admin Operator"
        })))
        .expect(1)
        .mount(&idp)
        .await;
    std::env::set_var("SHUNT_TEST_ADMIN_OIDC_TOKENS", "ops:admin-secret");
    std::env::set_var("SHUNT_TEST_ADMIN_OIDC_SECRET", "client-secret");
    let gateway = start_with_addr(admin_oidc_config(
        "SHUNT_TEST_ADMIN_OIDC_TOKENS",
        "SHUNT_TEST_ADMIN_OIDC_SECRET",
        &idp,
    ))
    .await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = client
        .get(format!("{}/admin/login", gateway.base_url))
        .send()
        .await
        .unwrap();
    // The SSO form's POST redirects to the external IdP; Chrome/WebKit apply
    // `form-action` to that redirect, so the login-page CSP must allow it.
    let csp = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        csp.contains("form-action 'self' https: http://127.0.0.1:* http://localhost:*"),
        "login page with SSO must widen form-action for the IdP redirect: {csp}"
    );
    let login = response.text().await.unwrap();
    assert!(login.contains("Sign in with SSO"));
    assert!(login.contains("action=\"/admin/oidc/start\""));

    let (location, state) = oidc_state(&client, &gateway).await;
    let params: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "admin-client");
    assert_eq!(
        params["redirect_uri"],
        format!("{}/admin/oidc/callback", gateway.base_url)
    );
    assert_eq!(params["scope"], "openid email profile");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(!params["code_challenge"].is_empty());

    let response = client
        .get(format!(
            "{}/admin/oidc/callback?code=authorization-code&state={state}",
            gateway.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/admin");
    let cookie = response_cookie(&response).expect("OIDC callback sets admin session cookie");
    let dashboard = client
        .get(format!("{}/admin", gateway.base_url))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(dashboard.status(), StatusCode::OK);

    let requests = idp.received_requests().await.unwrap();
    let token = requests
        .iter()
        .find(|request| request.url.path() == "/token")
        .unwrap();
    let body = String::from_utf8(token.body.clone()).unwrap();
    let form_url = reqwest::Url::parse(&format!("https://form.invalid/?{body}")).unwrap();
    let form: std::collections::HashMap<_, _> = form_url.query_pairs().into_owned().collect();
    assert_eq!(form["code"], "authorization-code");
    assert_eq!(form["client_secret"], "client-secret");
    assert_eq!(form["redirect_uri"], params["redirect_uri"]);
    use sha2::{Digest, Sha256};
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
        params["code_challenge"]
    );

    let header_response = client
        .get(format!("{}/admin/pool", gateway.base_url))
        .header("x-shunt-admin-token", "admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(header_response.status(), StatusCode::OK);

    std::env::remove_var("SHUNT_TEST_ADMIN_OIDC_TOKENS");
    std::env::remove_var("SHUNT_TEST_ADMIN_OIDC_SECRET");
}

#[tokio::test]
async fn admin_oidc_rejects_replay_disallowed_email_provider_error_and_cross_origin() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = ADMIN_OIDC_ENV_LOCK.lock().await;
    let idp = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token":"oidc-access"
        })))
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sub":"outside-subject", "email":"user@outside.test", "email_verified":true
        })))
        .mount(&idp)
        .await;
    std::env::set_var("SHUNT_TEST_ADMIN_OIDC_REJECT_TOKENS", "ops:admin-secret");
    std::env::set_var("SHUNT_TEST_ADMIN_OIDC_REJECT_SECRET", "client-secret");
    let gateway = start_with_addr(admin_oidc_config(
        "SHUNT_TEST_ADMIN_OIDC_REJECT_TOKENS",
        "SHUNT_TEST_ADMIN_OIDC_REJECT_SECRET",
        &idp,
    ))
    .await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = client
        .post(format!("{}/admin/oidc/start", gateway.base_url))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response_cookie(&response).is_none());

    let (_, denied_state) = oidc_state(&client, &gateway).await;
    let denied = client
        .get(format!(
            "{}/admin/oidc/callback?code=x&state={denied_state}",
            gateway.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(response_cookie(&denied).is_none());
    let denied_html = denied.text().await.unwrap();
    assert!(denied_html.contains("not authorized"));
    assert!(!denied_html.contains("outside.test"));

    let replay = client
        .get(format!(
            "{}/admin/oidc/callback?code=x&state={denied_state}",
            gateway.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert!(response_cookie(&replay).is_none());
    assert!(replay
        .text()
        .await
        .unwrap()
        .contains("invalid or has expired"));

    let (_, error_state) = oidc_state(&client, &gateway).await;
    let provider_error = client
        .get(format!(
            "{}/admin/oidc/callback?error=access_denied&state={error_state}",
            gateway.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(provider_error.status(), StatusCode::BAD_REQUEST);
    assert!(response_cookie(&provider_error).is_none());
    let error_html = provider_error.text().await.unwrap();
    assert!(error_html.contains("identity provider reported an error"));
    assert!(!error_html.contains("access_denied"));

    std::env::remove_var("SHUNT_TEST_ADMIN_OIDC_REJECT_TOKENS");
    std::env::remove_var("SHUNT_TEST_ADMIN_OIDC_REJECT_SECRET");
}

#[tokio::test]
async fn admin_oidc_discovery_builds_authorization_redirect() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = ADMIN_OIDC_ENV_LOCK.lock().await;
    let idp = MockServer::start().await;
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": idp.uri(),
            "authorization_endpoint": format!("{}/discovered-authorize", idp.uri()),
            "token_endpoint": format!("{}/token", idp.uri()),
            "userinfo_endpoint": format!("{}/userinfo", idp.uri())
        })))
        .expect(1)
        .mount(&idp)
        .await;
    std::env::set_var("SHUNT_TEST_ADMIN_OIDC_DISCOVERY_TOKENS", "ops:admin-secret");
    std::env::set_var("SHUNT_TEST_ADMIN_OIDC_DISCOVERY_SECRET", "client-secret");
    let mut config = admin_oidc_config(
        "SHUNT_TEST_ADMIN_OIDC_DISCOVERY_TOKENS",
        "SHUNT_TEST_ADMIN_OIDC_DISCOVERY_SECRET",
        &idp,
    );
    let oidc = config.server.admin.as_mut().unwrap().oidc.as_mut().unwrap();
    oidc.provider.authorization_endpoint = None;
    oidc.provider.token_endpoint = None;
    oidc.provider.userinfo_endpoint = None;
    let gateway = start_with_addr(config).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let (location, _) = oidc_state(&client, &gateway).await;
    assert_eq!(location.path(), "/discovered-authorize");

    std::env::remove_var("SHUNT_TEST_ADMIN_OIDC_DISCOVERY_TOKENS");
    std::env::remove_var("SHUNT_TEST_ADMIN_OIDC_DISCOVERY_SECRET");
}

#[tokio::test]
async fn admin_routes_are_absent_without_the_block() {
    if !can_bind_loopback() {
        return;
    }
    // Default config has no [server.admin], so the routes must not be registered.
    let gateway = start(Config::default()).await;
    let client = reqwest::Client::new();
    for route in [
        "/admin",
        "/admin/login",
        "/admin/oidc/start",
        "/admin/oidc/callback",
        "/admin/pool",
    ] {
        let response = client
            .get(format!("{}{route}", gateway.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{route} must 404 when admin is disabled"
        );
    }
}

#[tokio::test]
async fn admin_pool_repeats_shared_physical_state_per_upstream() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_ADMIN_SHARED_POOL", "ops:shared-secret");
    let mut config = admin_config("SHUNT_TEST_ADMIN_SHARED_POOL");
    let mut account = AccountConfig {
        name: "shared-account".to_string(),
        uuid: Some("shared-uuid".to_string()),
        ..Default::default()
    };
    account.store_family = Some(shunt::accounts::StoreFamily::Claude);
    let template = config.providers["anthropic"].clone();
    for name in ["primary", "secondary"] {
        let mut provider = template.clone();
        provider.auth = AuthMode::ClaudeOauth;
        provider.accounts = vec![account.clone()];
        config.providers.insert(name.to_string(), provider);
    }
    config.providers.remove("anthropic");
    config.server.default_provider = "primary".to_string();
    let (gateway, state) = start_with_state(config).await;
    let mut quota = reqwest::header::HeaderMap::new();
    quota.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        reqwest::header::HeaderValue::from_static("0.73"),
    );
    state.accounts.note_quota("primary", &account, &quota);

    let response = reqwest::Client::new()
        .get(format!("{}/admin/pool", gateway.base_url))
        .header("x-shunt-admin-token", "shared-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    let providers = body["providers"].as_array().unwrap();
    for name in ["primary", "secondary"] {
        let section = providers
            .iter()
            .find(|provider| provider["provider"] == name)
            .unwrap();
        assert_eq!(section["accounts"][0]["name"], "shared-account");
        assert_eq!(section["accounts"][0]["utilization_5h"], 0.73);
    }
    std::env::remove_var("SHUNT_TEST_ADMIN_SHARED_POOL");
}

#[tokio::test]
async fn admin_api_requires_authentication() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_B", "ops:secret-b");
    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_B")).await;
    let client = reqwest::Client::new();

    // No credential at all.
    let response = client
        .get(format!("{}/admin/pool", gateway.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Wrong admin token.
    let response = client
        .post(format!("{}/admin/accounts/claude", gateway.base_url))
        .header("x-shunt-admin-token", "nope")
        .header("content-type", "application/json")
        .body(r#"{"name":"main"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_B");
}

#[tokio::test]
async fn provisioning_flow_stores_setup_token_without_leaking_it() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_C", "ops:secret-c");

    // Mock the setup-token exchange, including the one-year expires_in request.
    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_partial_json(serde_json::json!({
            "grant_type": "authorization_code",
            "code": "the-auth-code",
            "redirect_uri": "https://platform.claude.com/oauth/code/callback",
            "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            "expires_in": 31_536_000_u64,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "SECRET-SETUP-TOKEN",
            "account": {"uuid": "acct-uuid-123"}
        })))
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_C")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-c")
            .header("content-type", "application/json")
    };

    // Start: returns an inference-only authorize URL carrying the OAuth state.
    let response = auth(client.post(format!("{}/admin/accounts/claude", gateway.base_url)))
        .body(r#"{"name":"main"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let authorize_url = reqwest::Url::parse(body["authorize_url"].as_str().unwrap()).unwrap();
    let scope = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "scope")
        .map(|(_, value)| value.into_owned());
    assert_eq!(scope.as_deref(), Some("user:inference"));
    let state = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("authorize URL carries the OAuth state");

    // Complete: paste `<code>#<state>`; the account is stored and live immediately.
    let response = auth(client.post(format!(
        "{}/admin/accounts/claude/main/complete",
        gateway.base_url
    )))
    .body(format!(r#"{{"code":"the-auth-code#{state}"}}"#))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains("SECRET-SETUP-TOKEN"),
        "the setup token must never be returned to the browser"
    );
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["stored"], true);
    assert_eq!(body["live"], true);

    // The store file holds the token + UUID; the token lives only on disk (0600).
    let stored = std::fs::read_to_string(dir.join("main.json")).unwrap();
    assert!(stored.contains("SECRET-SETUP-TOKEN"));
    assert!(stored.contains("acct-uuid-123"));

    // List reports metadata only (kind, not the token).
    let response = auth(client.get(format!("{}/admin/accounts", gateway.base_url)))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["accounts"][0]["name"], "main");
    assert_eq!(body["accounts"][0]["kind"], "setup_token");
    assert!(!body.to_string().contains("SECRET-SETUP-TOKEN"));

    // Pool enumerates the scanned account.
    let response = auth(client.get(format!("{}/admin/pool", gateway.base_url)))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert!(body.to_string().contains("\"main\""));

    // Delete removes the store file.
    let response = auth(client.delete(format!("{}/admin/accounts/claude/main", gateway.base_url)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("main.json").exists());

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_C");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn provisioning_flow_stores_refreshable_oauth_account() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_OAUTH", "ops:secret-oauth");

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_partial_json(serde_json::json!({
            "grant_type": "authorization_code",
            "code": "oauth-code",
            "redirect_uri": "https://platform.claude.com/oauth/code/callback",
            "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "SECRET-OAUTH-ACCESS",
            "refresh_token": "SECRET-OAUTH-REFRESH",
            "expires_in": 7200,
            "account": {"uuid": "acct-oauth-123"}
        })))
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_OAUTH")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-oauth")
            .header("content-type", "application/json")
    };

    let response = auth(client.post(format!("{}/admin/accounts/claude", gateway.base_url)))
        .body(r#"{"name":"oauthy","mode":"oauth"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let authorize_url = reqwest::Url::parse(body["authorize_url"].as_str().unwrap()).unwrap();
    let scope = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "scope")
        .map(|(_, value)| value.into_owned());
    assert_eq!(
        scope.as_deref(),
        Some("user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload")
    );
    let state = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("authorize URL carries the OAuth state");

    let response = auth(client.post(format!(
        "{}/admin/accounts/claude/oauthy/complete",
        gateway.base_url
    )))
    .body(format!(r#"{{"code":"oauth-code#{state}"}}"#))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requests = token_server
        .received_requests()
        .await
        .expect("mock records token exchange requests");
    let exchange_body: serde_json::Value = requests
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path() == "/token")
        .expect("full OAuth completion exchanges its code")
        .body_json()
        .unwrap();
    assert!(
        exchange_body.get("expires_in").is_none(),
        "full OAuth must let the provider choose the access-token lifetime"
    );
    let text = response.text().await.unwrap();
    assert!(!text.contains("SECRET-OAUTH-ACCESS"));
    assert!(!text.contains("SECRET-OAUTH-REFRESH"));

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("oauthy.json")).unwrap()).unwrap();
    assert_eq!(
        stored["claudeAiOauth"]["accessToken"],
        "SECRET-OAUTH-ACCESS"
    );
    assert_eq!(
        stored["claudeAiOauth"]["refreshToken"],
        "SECRET-OAUTH-REFRESH"
    );
    assert!(stored["claudeAiOauth"]["expiresAt"].as_i64().unwrap() > 0);
    assert!(stored["claudeAiOauth"].get("shuntCredentialKind").is_none());
    assert_eq!(stored["shuntAccountUuid"], "acct-oauth-123");

    let response = auth(client.get(format!("{}/admin/accounts", gateway.base_url)))
        .send()
        .await
        .unwrap();
    let text = response.text().await.unwrap();
    assert!(!text.contains("SECRET-OAUTH-ACCESS"));
    assert!(!text.contains("SECRET-OAUTH-REFRESH"));
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["accounts"][0]["kind"], "imported");

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_OAUTH");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn claude_reprovision_clears_orphaned_identity_without_wiping_shared_alias_health() {
    // Regression test mirroring
    // `codex_reprovision_clears_orphaned_identity_without_wiping_shared_alias_health`:
    // reprovisioning "account-a" from an old upstream identity ("acct-old") to a
    // new one ("shared-id") must (a) drop the now-orphaned old identity's pool
    // health, and (b) never wipe pool health for the new identity when it is
    // still shared by another stored account alias.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CLAUDE_REPROV",
        "ops:secret-claude-reprov",
    );

    // "other-account" is a pre-existing store account sharing the identity
    // ("shared-id") that "account-a" will be reprovisioned onto below.
    std::fs::write(
        dir.join("other-account.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "other-access",
                "refreshToken": "other-refresh",
                "expiresAt": 4_102_444_800_000i64,
            },
            "shuntAccountUuid": "shared-id",
        })
        .to_string(),
    )
    .unwrap();

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ACCESS-1",
            "refresh_token": "REFRESH-1",
            "expires_in": 7200,
            "account": {"uuid": "acct-old"}
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&token_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ACCESS-2",
            "refresh_token": "REFRESH-2",
            "expires_in": 7200,
            "account": {"uuid": "shared-id"}
        })))
        .with_priority(2)
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_REPROV");
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    // Seed pool health: "other-account" (identity "shared-id") is cooling down.
    let other_account = AccountConfig {
        name: "other-account".to_string(),
        uuid: Some("shared-id".to_string()),
        ..Default::default()
    };
    state.accounts.cooldown(
        "anthropic",
        &other_account,
        std::time::Duration::from_secs(300),
        "transport",
    );

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-claude-reprov")
            .header("content-type", "application/json")
    };

    // First provisioning: account-a -> identity "acct-old".
    let response = auth(client.post(format!("{base_url}/admin/accounts/claude")))
        .body(r#"{"name":"account-a","mode":"oauth"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state1) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/claude/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-1#{state1}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Cool down "account-a" while it is still on "acct-old".
    let account_a_old = AccountConfig {
        name: "account-a".to_string(),
        uuid: Some("acct-old".to_string()),
        ..Default::default()
    };
    state.accounts.cooldown(
        "anthropic",
        &account_a_old,
        std::time::Duration::from_secs(300),
        "transport",
    );
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&account_a_old),
        None,
        None,
    );
    assert!(
        snapshot[0].has_state,
        "acct-old health should be observed before reprovisioning"
    );

    // Reprovision account-a onto "shared-id" -- the same identity as
    // "other-account".
    let response = auth(client.post(format!("{base_url}/admin/accounts/claude")))
        .body(r#"{"name":"account-a","mode":"oauth"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state2) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/claude/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-2#{state2}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // (a) The now-orphaned "acct-old" identity's health must be cleared.
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&account_a_old),
        None,
        None,
    );
    assert!(
        !snapshot[0].has_state,
        "orphaned old identity health should have been cleared on reprovision"
    );

    // (b) "other-account"'s health for the shared "shared-id" identity must
    // survive, since account-a's reprovision must not unjustly clear health
    // shared by another alias.
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&other_account),
        None,
        None,
    );
    assert!(
        snapshot[0].has_state,
        "shared identity health must survive a reprovision of another alias"
    );
    assert!(
        snapshot[0].cooldown_secs_remaining.is_some(),
        "shared identity's cooldown must not have been wiped"
    );

    task.abort();
    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_REPROV");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn claude_reprovision_clears_blank_uuid_old_identity_using_name_fallback() {
    // Regression test: the runtime identity of a stored account with no UUID at
    // all falls back to its own name (`accounts::account_identity`), not to
    // "no identity". Before the fix, capturing the pre-reprovision identity as
    // a bare `account_uuid(name)` conflated that legitimate blank-UUID case
    // with "no prior account existed", so a reprovision that moved a blank-UUID
    // account onto a real UUID silently left its old name-keyed health entry
    // stranded forever.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CLAUDE_BLANK_OLD",
        "ops:secret-claude-blank-old",
    );

    let token_server = MockServer::start().await;
    // First exchange returns no `account` at all -- the stored account carries
    // no UUID, so its runtime identity is its own name ("account-a").
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ACCESS-1",
            "refresh_token": "REFRESH-1",
            "expires_in": 7200,
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&token_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ACCESS-2",
            "refresh_token": "REFRESH-2",
            "expires_in": 7200,
            "account": {"uuid": "new-id"}
        })))
        .with_priority(2)
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_BLANK_OLD");
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-claude-blank-old")
            .header("content-type", "application/json")
    };

    // First provisioning: account-a stored with no UUID at all.
    let response = auth(client.post(format!("{base_url}/admin/accounts/claude")))
        .body(r#"{"name":"account-a","mode":"oauth"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state1) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/claude/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-1#{state1}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Cool down "account-a" under its name-fallback identity ("account-a").
    let account_a_blank = AccountConfig {
        name: "account-a".to_string(),
        uuid: None,
        store_entry: true,
        store_family: Some(shunt::accounts::StoreFamily::Claude),
        ..Default::default()
    };
    state.accounts.cooldown(
        "anthropic",
        &account_a_blank,
        std::time::Duration::from_secs(300),
        "transport",
    );
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&account_a_blank),
        None,
        None,
    );
    assert!(
        snapshot[0].has_state,
        "blank-UUID name-fallback identity health should be observed before reprovisioning"
    );

    // Reprovision account-a onto a real UUID ("new-id").
    let response = auth(client.post(format!("{base_url}/admin/accounts/claude")))
        .body(r#"{"name":"account-a","mode":"oauth"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state2) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/claude/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-2#{state2}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The now-orphaned blank-UUID ("account-a" name-fallback) identity's
    // health must be cleared, not stranded.
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&account_a_blank),
        None,
        None,
    );
    assert!(
        !snapshot[0].has_state,
        "orphaned blank-UUID old identity health should have been cleared on reprovision"
    );

    task.abort();
    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_BLANK_OLD");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn claude_remove_preserves_shared_identity_health_until_last_alias_is_removed() {
    // Regression test for the admin Claude remove-account identity-health
    // cleanup: removing one alias of a shared upstream identity must preserve
    // pool health while a sibling alias still resolves to that identity, and
    // only clear it once the last alias sharing the identity is gone.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CLAUDE_REMOVE",
        "ops:secret-claude-remove",
    );

    // "alias-a" and "alias-b" both resolve to the shared "shared-id" identity.
    for name in ["alias-a", "alias-b"] {
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": format!("{name}-access"),
                    "refreshToken": format!("{name}-refresh"),
                    "expiresAt": 4_102_444_800_000i64,
                },
                "shuntAccountUuid": "shared-id",
            })
            .to_string(),
        )
        .unwrap();
    }

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_REMOVE");
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    let shared_identity = AccountConfig {
        name: "shared-id".to_string(),
        uuid: Some("shared-id".to_string()),
        ..Default::default()
    };
    state.accounts.cooldown(
        "anthropic",
        &shared_identity,
        std::time::Duration::from_secs(300),
        "transport",
    );

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request.header("x-shunt-admin-token", "secret-claude-remove")
    };

    // Removing "alias-a" must not clear "shared-id" health: "alias-b" still
    // resolves to it.
    let response = auth(client.delete(format!("{base_url}/admin/accounts/claude/alias-a")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("alias-a.json").exists());
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&shared_identity),
        None,
        None,
    );
    assert!(
        snapshot[0].has_state,
        "shared identity health must survive removing one of two aliases"
    );

    // Removing "alias-b" (the last remaining alias) must now clear it.
    let response = auth(client.delete(format!("{base_url}/admin/accounts/claude/alias-b")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("alias-b.json").exists());
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&shared_identity),
        None,
        None,
    );
    assert!(
        !snapshot[0].has_state,
        "shared identity health should be cleared once no alias resolves to it any more"
    );

    task.abort();
    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_REMOVE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn claude_remove_preserves_a_configured_providers_health_the_store_scan_cannot_see() {
    // Regression test: an identity can be shared between a store-scanned
    // account (removed here) and an *explicitly configured*
    // `[[providers.<name>.accounts]]` alias on a different, non-empty-accounts
    // provider -- e.g. a `credentials`/`token_env` entry that never appears in
    // any store directory scan at all. Before the fix, the removal cleanup
    // decided whether an identity was "still in use" purely from the store
    // scan, so it would wipe every provider's health for that identity
    // (`forget_pool_health` looped every same-auth-mode provider
    // unconditionally) even though the configured provider's alias still
    // legitimately relies on it. The fix must check each provider against its
    // own effective account set: the store scan for a dynamic-discovery
    // provider, but the provider's own configured accounts for one that sets
    // `accounts` explicitly.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CLAUDE_CONFIGURED",
        "ops:secret-claude-configured",
    );

    // "account-a" is a store-scanned account sharing the identity ("shared-id")
    // that a *different*, explicitly configured provider's alias also uses.
    std::fs::write(
        dir.join("account-a.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "account-a-access",
                "refreshToken": "account-a-refresh",
                "expiresAt": 4_102_444_800_000i64,
            },
            "shuntAccountUuid": "shared-id",
        })
        .to_string(),
    )
    .unwrap();

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_CONFIGURED");
    // A second Claude provider with an explicitly configured (non-empty)
    // account list -- never scanned from the store directory -- whose one
    // alias resolves to the same "shared-id" identity as "account-a" above.
    let mut configured_provider = config.providers.get("anthropic").unwrap().clone();
    configured_provider.accounts = vec![AccountConfig {
        name: "configured-alias".to_string(),
        uuid: Some("shared-id".to_string()),
        credentials: Some("/tmp/shunt-test-does-not-need-to-exist.json".to_string()),
        ..Default::default()
    }];
    config
        .providers
        .insert("anthropic-configured".to_string(), configured_provider);
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    let shared_identity = AccountConfig {
        name: "shared-id".to_string(),
        uuid: Some("shared-id".to_string()),
        ..Default::default()
    };
    // Seed health on both providers for the shared identity: the
    // dynamic-discovery "anthropic" provider (which will legitimately lose
    // the identity once "account-a" is removed, since the store has no other
    // alias for it) and the explicitly configured "anthropic-configured"
    // provider (which must keep it, since its own "configured-alias" entry
    // still resolves to "shared-id" -- a fact the store scan cannot see).
    state.accounts.cooldown(
        "anthropic",
        &shared_identity,
        std::time::Duration::from_secs(300),
        "transport",
    );
    state.accounts.cooldown(
        "anthropic-configured",
        &shared_identity,
        std::time::Duration::from_secs(300),
        "transport",
    );

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request.header("x-shunt-admin-token", "secret-claude-configured")
    };

    let response = auth(client.delete(format!("{base_url}/admin/accounts/claude/account-a")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("account-a.json").exists());

    // The physical identity remains live because another configured upstream
    // still references it, so both upstream views retain the shared state.
    let snapshot = state.accounts.snapshot(
        "anthropic",
        std::slice::from_ref(&shared_identity),
        None,
        None,
    );
    assert!(
        snapshot[0].has_state,
        "shared physical health must survive while another upstream references it"
    );

    // The explicitly configured provider's health must survive: its own
    // "configured-alias" account still resolves to "shared-id", even though
    // the store scan (which drove the dynamic-discovery provider's decision
    // above) knows nothing about it.
    let snapshot = state.accounts.snapshot(
        "anthropic-configured",
        std::slice::from_ref(&shared_identity),
        None,
        None,
    );
    assert!(
        snapshot[0].has_state,
        "a configured provider's health for an identity its own account list still uses \
         must not be wiped by an unrelated provider's store-only removal"
    );
    assert!(
        snapshot[0].cooldown_secs_remaining.is_some(),
        "the configured provider's cooldown must not have been wiped"
    );

    task.abort();
    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CLAUDE_CONFIGURED");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn full_oauth_completion_rejects_missing_refresh_token() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_NO_REFRESH",
        "ops:secret-no-refresh",
    );

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ACCESS-WITHOUT-REFRESH",
            "expires_in": 3600,
            "account": {"uuid": "acct-no-refresh"}
        })))
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_NO_REFRESH")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-no-refresh")
            .header("content-type", "application/json")
    };
    let response = auth(client.post(format!("{}/admin/accounts/claude", gateway.base_url)))
        .body(r#"{"name":"missing-refresh","mode":"oauth"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let authorize_url = reqwest::Url::parse(body["authorize_url"].as_str().unwrap()).unwrap();
    let state = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap();

    let response = auth(client.post(format!(
        "{}/admin/accounts/claude/missing-refresh/complete",
        gateway.base_url
    )))
    .body(format!(r#"{{"code":"oauth-code#{state}"}}"#))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!dir.join("missing-refresh.json").exists());

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_NO_REFRESH");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cookie_session_mutations_require_a_csrf_token() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_D", "ops:secret-d");
    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_D")).await;
    // Do not auto-follow the post-login redirect; inspect the Set-Cookie directly.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // Sign in with the admin token → session cookie.
    let response = client
        .post(format!("{}/admin/login", gateway.base_url))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("token=secret-d")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("shunt_admin_session="))
        .map(|value| value.split(';').next().unwrap().to_string())
        .expect("login sets a session cookie");
    // Loopback host ⇒ the cookie is not marked Secure, so it works over plain HTTP.
    assert!(!cookie.contains("Secure"));

    // A cookie-authenticated mutation without the CSRF token is rejected.
    let response = client
        .post(format!("{}/admin/accounts/claude", gateway.base_url))
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(r#"{"name":"main"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_D");
}

#[tokio::test]
async fn browser_session_dashboard_csrf_accept_and_logout() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_E", "ops:secret-e");
    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_E")).await;
    // Do not auto-follow redirects; assert on the raw responses.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // Without OIDC the login page keeps the strict same-origin form-action.
    let response = client
        .get(format!("{}/admin/login", gateway.base_url))
        .send()
        .await
        .unwrap();
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("form-action 'self';"));

    // Sign in with the admin token → session cookie.
    let response = client
        .post(format!("{}/admin/login", gateway.base_url))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("token=secret-e")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("shunt_admin_session="))
        .map(|value| value.split(';').next().unwrap().to_string())
        .expect("login sets a session cookie");

    // The dashboard renders and embeds the session's CSRF token for its script.
    let response = client
        .get(format!("{}/admin", gateway.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = response.text().await.unwrap();
    let csrf = html
        .split_once("const CSRF = \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(token, _)| token.to_string())
        .expect("dashboard embeds the CSRF token");
    assert!(!csrf.is_empty());

    // A cookie mutation WITH the matching CSRF token + same-origin is accepted
    // (the accept branch of check_csrf, complementing the reject-path test).
    let response = client
        .post(format!("{}/admin/accounts/claude", gateway.base_url))
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .header("x-csrf-token", &csrf)
        .body(r#"{"name":"pool-b"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a valid session cookie + CSRF token is accepted"
    );

    // Cross-site logout is rejected by the same-origin guard.
    let response = client
        .post(format!("{}/admin/logout", gateway.base_url))
        .header("cookie", &cookie)
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "cross-origin logout is rejected"
    );

    // Same-origin logout clears the cookie and invalidates the session.
    let response = client
        .post(format!("{}/admin/logout", gateway.base_url))
        .header("cookie", &cookie)
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // After logout the old cookie no longer authenticates → redirect to login.
    let response = client
        .get(format!("{}/admin", gateway.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/admin/login",
        "a logged-out session is redirected to the login page"
    );

    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_E");
}

#[tokio::test]
async fn completion_reports_bad_gateway_when_token_exchange_fails() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_F", "ops:secret-f");

    // Upstream token endpoint fails; the completion must surface a generic 502
    // without echoing the upstream detail, and must not store an account.
    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_string("invalid_grant: bad code"))
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CLAUDE_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_F")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-f")
            .header("content-type", "application/json")
    };

    // Start to obtain a valid pending OAuth state.
    let response = auth(client.post(format!("{}/admin/accounts/claude", gateway.base_url)))
        .body(r#"{"name":"main"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let authorize_url = reqwest::Url::parse(body["authorize_url"].as_str().unwrap()).unwrap();
    let state = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("authorize URL carries the OAuth state");

    // Complete with a well-formed `<code>#<state>` but a failing upstream.
    let response = auth(client.post(format!(
        "{}/admin/accounts/claude/main/complete",
        gateway.base_url
    )))
    .body(format!(r#"{{"code":"the-auth-code#{state}"}}"#))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains("invalid_grant"),
        "the generic 502 must not echo upstream detail"
    );
    assert!(
        !dir.join("main.json").exists(),
        "a failed exchange must not store an account"
    );

    std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CLAUDE_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_F");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn admin_negative_paths_are_rejected() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_G", "ops:secret-g");
    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_G")).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let base = &gateway.base_url;
    let hdr = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-g")
            .header("content-type", "application/json")
    };

    // Wrong admin token → 401 with the re-rendered (escaped) login error.
    let response = client
        .post(format!("{base}/admin/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("token=wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("Invalid admin token."));

    // add_account with a malformed JSON body → 400.
    let response = hdr(client.post(format!("{base}/admin/accounts/claude")))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // add_account with an invalid account name → 400.
    let response = hdr(client.post(format!("{base}/admin/accounts/claude")))
        .body(r#"{"name":"BAD_NAME"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // add_account with an invalid mode → 400.
    let response = hdr(client.post(format!("{base}/admin/accounts/claude")))
        .body(r#"{"name":"valid-name","mode":"bogus"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // complete with no pending login for the name → 400.
    let response = hdr(client.post(format!("{base}/admin/accounts/claude/ghost/complete")))
        .body(r#"{"code":"the-code#the-state"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A cookie mutation from a cross-site context is rejected by the same-origin
    // guard even when it carries a CSRF header.
    let login = client
        .post(format!("{base}/admin/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("token=secret-g")
        .send()
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("shunt_admin_session="))
        .map(|value| value.split(';').next().unwrap().to_string())
        .expect("login sets a session cookie");
    let response = client
        .post(format!("{base}/admin/accounts/claude"))
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "cross-site")
        .header("x-csrf-token", "whatever")
        .body(r#"{"name":"x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_G");
}

#[tokio::test]
async fn codex_provisioning_supports_code_state_and_full_redirect() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_CODEX", "ops:secret-codex");

    let token_server = MockServer::start().await;
    let access = chatgpt_token(4_102_444_800, "acct-codex");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": access,
            "refresh_token": "SECRET-CODEX-REFRESH",
            "id_token": "SECRET-CODEX-ID"
        })))
        .expect(2)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex")
            .header("content-type", "application/json")
    };

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"codex-a"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (authorize_url, state) = authorize_state(&body);
    let params = authorize_url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    for (key, expected) in [
        ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
        ("redirect_uri", "http://localhost:1455/auth/callback"),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        ("codex_cli_simplified_flow", "true"),
        ("id_token_add_organizations", "true"),
        ("state", state.as_str()),
    ] {
        assert_eq!(params.get(key).map(|value| value.as_ref()), Some(expected));
    }

    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/codex-a/complete",
        gateway.base_url
    )))
    .body(serde_json::json!({"code": format!("oauth-code#{state}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(!text.contains(&access));
    assert!(!text.contains("SECRET-CODEX-REFRESH"));
    assert!(!text.contains("SECRET-CODEX-ID"));

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("codex-a.json")).unwrap()).unwrap();
    assert_eq!(stored["auth_mode"], "ChatGPT");
    assert_eq!(stored["tokens"]["access_token"], access);
    assert_eq!(stored["tokens"]["refresh_token"], "SECRET-CODEX-REFRESH");
    assert_eq!(stored["tokens"]["account_id"], "acct-codex");

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"codex-url"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, url_state) = authorize_state(&body);
    let callback = reqwest::Url::parse_with_params(
        "http://localhost:1455/auth/callback",
        &[("code", "url-code"), ("state", url_state.as_str())],
    )
    .unwrap();
    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/codex-url/complete",
        gateway.base_url
    )))
    .body(serde_json::json!({"code": callback.to_string()}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(dir.join("codex-url.json").exists());

    let requests = token_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let content_type = request
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert_eq!(content_type, "application/x-www-form-urlencoded");
        let body = String::from_utf8(request.body).unwrap();
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(body.contains("code_verifier="));
    }

    let response = auth(client.get(format!("{}/admin/accounts/codex", gateway.base_url)))
        .send()
        .await
        .unwrap();
    let text = response.text().await.unwrap();
    assert!(!text.contains(&access));
    assert!(!text.contains("SECRET-CODEX-REFRESH"));
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["accounts"].as_array().unwrap().len(), 2);
    assert!(body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|account| account["account_id"] == "acct-codex"));

    let response = auth(client.get(format!("{}/admin/pool", gateway.base_url)))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let codex = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["provider"] == "codex")
        .expect("pool includes built-in codex provider");
    assert!(codex["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|account| account["name"] == "codex-a"));

    let response =
        auth(client.delete(format!("{}/admin/accounts/codex/codex-a", gateway.base_url)))
            .send()
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("codex-a.json").exists());

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_reprovision_clears_orphaned_identity_without_wiping_shared_alias_health() {
    // Regression test for the admin Codex reprovisioning identity-health
    // cleanup: reprovisioning "account-a" from an old upstream identity to a
    // new one must (a) drop the now-orphaned old identity's pool health, and
    // (b) never wipe pool health for the new identity when it is still
    // shared by another stored account alias.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_REPROV",
        "ops:secret-codex-reprov",
    );

    // "other-account" is a pre-existing store account sharing the identity
    // ("shared-id") that "account-a" will be reprovisioned onto below.
    let other_access = chatgpt_token(4_102_444_800, "shared-id");
    std::fs::write(
        dir.join("other-account.json"),
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "access_token": other_access,
                "refresh_token": "other-refresh",
                "account_id": "shared-id",
            }
        })
        .to_string(),
    )
    .unwrap();

    let token_server = MockServer::start().await;
    let first_access = chatgpt_token(4_102_444_800, "acct-old");
    let second_access = chatgpt_token(4_102_444_800 + 1, "shared-id");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": first_access,
            "refresh_token": "refresh-1"
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&token_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": second_access,
            "refresh_token": "refresh-2"
        })))
        .with_priority(2)
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_REPROV");
    let codex = config.providers.get_mut("codex").unwrap();
    codex.auth = AuthMode::ChatgptOauth;
    codex.accounts = Vec::new();
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    // Seed pool health: "other-account" (identity "shared-id") is cooling down.
    let other_account = AccountConfig {
        name: "other-account".to_string(),
        uuid: Some("shared-id".to_string()),
        ..Default::default()
    };
    state.accounts.cooldown(
        "codex",
        &other_account,
        std::time::Duration::from_secs(300),
        "transport",
    );

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex-reprov")
            .header("content-type", "application/json")
    };

    // First provisioning: account-a -> identity "acct-old".
    let response = auth(client.post(format!("{base_url}/admin/accounts/codex")))
        .body(r#"{"name":"account-a"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state1) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/codex/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-1#{state1}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Cool down "account-a" while it is still on "acct-old".
    let account_a_old = AccountConfig {
        name: "account-a".to_string(),
        uuid: Some("acct-old".to_string()),
        ..Default::default()
    };
    state.accounts.cooldown(
        "codex",
        &account_a_old,
        std::time::Duration::from_secs(300),
        "transport",
    );
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&account_a_old), None, None);
    assert!(
        snapshot[0].has_state,
        "acct-old health should be observed before reprovisioning"
    );

    // Reprovision account-a onto "shared-id" -- the same identity as
    // "other-account".
    let response = auth(client.post(format!("{base_url}/admin/accounts/codex")))
        .body(r#"{"name":"account-a"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state2) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/codex/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-2#{state2}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // (a) The now-orphaned "acct-old" identity's health must be cleared.
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&account_a_old), None, None);
    assert!(
        !snapshot[0].has_state,
        "orphaned old identity health should have been cleared on reprovision"
    );

    // (b) "other-account"'s health for the shared "shared-id" identity must
    // survive, since account-a's reprovision must not unjustly clear health
    // shared by another alias.
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&other_account), None, None);
    assert!(
        snapshot[0].has_state,
        "shared identity health must survive a reprovision of another alias"
    );
    assert!(
        snapshot[0].cooldown_secs_remaining.is_some(),
        "shared identity's cooldown must not have been wiped"
    );

    task.abort();
    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_REPROV");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_reprovision_clears_blank_identity_old_account_using_name_fallback() {
    // Regression test mirroring
    // `claude_reprovision_clears_blank_uuid_old_identity_using_name_fallback`:
    // a stored account whose file carries no resolvable `account_id` (nor a
    // JWT `chatgpt_account_id` claim) still has a runtime identity -- its own
    // name (`accounts::account_identity`'s fallback) -- not "no identity" at
    // all. Capturing the pre-reprovision identity as a bare
    // `account_id(name)` would conflate that with "no prior account existed",
    // stranding the old name-keyed health entry forever once reprovisioned
    // onto a real identity.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_BLANK_OLD",
        "ops:secret-codex-blank-old",
    );

    // "account-a" already exists in the store, but its file carries no
    // resolvable identity at all (no `account_id`, and an access token that is
    // not a parseable JWT) -- so its runtime identity is its own name.
    std::fs::write(
        dir.join("account-a.json"),
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "access_token": "not-a-jwt",
                "refresh_token": "old-refresh",
            }
        })
        .to_string(),
    )
    .unwrap();

    let token_server = MockServer::start().await;
    let new_access = chatgpt_token(4_102_444_800, "new-id");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": new_access,
            "refresh_token": "new-refresh"
        })))
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_BLANK_OLD");
    let codex = config.providers.get_mut("codex").unwrap();
    codex.auth = AuthMode::ChatgptOauth;
    codex.accounts = Vec::new();
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    // Cool down "account-a" under its name-fallback identity ("account-a").
    let account_a_blank = AccountConfig {
        name: "account-a".to_string(),
        uuid: None,
        store_entry: true,
        store_family: Some(shunt::accounts::StoreFamily::Chatgpt),
        ..Default::default()
    };
    state.accounts.cooldown(
        "codex",
        &account_a_blank,
        std::time::Duration::from_secs(300),
        "transport",
    );
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&account_a_blank), None, None);
    assert!(
        snapshot[0].has_state,
        "blank-identity name-fallback health should be observed before reprovisioning"
    );

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex-blank-old")
            .header("content-type", "application/json")
    };

    // Reprovision account-a onto a real identity ("new-id").
    let response = auth(client.post(format!("{base_url}/admin/accounts/codex")))
        .body(r#"{"name":"account-a"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state1) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{base_url}/admin/accounts/codex/account-a/complete"
    )))
    .body(serde_json::json!({"code": format!("code-1#{state1}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The now-orphaned blank-identity ("account-a" name-fallback) health must
    // be cleared, not stranded.
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&account_a_blank), None, None);
    assert!(
        !snapshot[0].has_state,
        "orphaned blank-identity old account health should have been cleared on reprovision"
    );

    task.abort();
    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_BLANK_OLD");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_remove_preserves_shared_identity_health_until_last_alias_is_removed() {
    // Regression test for the admin Codex remove-account identity-health
    // cleanup: removing one alias of a shared upstream identity must preserve
    // pool health while a sibling alias still resolves to that identity, and
    // only clear it once the last alias sharing the identity is gone.
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_REMOVE",
        "ops:secret-codex-remove",
    );

    // "alias-a" and "alias-b" both resolve to the shared "shared-id" identity.
    for name in ["alias-a", "alias-b"] {
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::json!({
                "auth_mode": "ChatGPT",
                "tokens": {
                    "access_token": format!("{name}-access"),
                    "refresh_token": format!("{name}-refresh"),
                    "account_id": "shared-id",
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    let mut config = admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_REMOVE");
    let codex = config.providers.get_mut("codex").unwrap();
    codex.auth = AuthMode::ChatgptOauth;
    codex.accounts = Vec::new();
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}");

    let shared_identity = AccountConfig {
        name: "shared-id".to_string(),
        uuid: Some("shared-id".to_string()),
        ..Default::default()
    };
    state.accounts.cooldown(
        "codex",
        &shared_identity,
        std::time::Duration::from_secs(300),
        "transport",
    );

    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request.header("x-shunt-admin-token", "secret-codex-remove")
    };

    // Removing "alias-a" must not clear "shared-id" health: "alias-b" still
    // resolves to it.
    let response = auth(client.delete(format!("{base_url}/admin/accounts/codex/alias-a")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("alias-a.json").exists());
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&shared_identity), None, None);
    assert!(
        snapshot[0].has_state,
        "shared identity health must survive removing one of two aliases"
    );

    // Removing "alias-b" (the last remaining alias) must now clear it.
    let response = auth(client.delete(format!("{base_url}/admin/accounts/codex/alias-b")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join("alias-b.json").exists());
    let snapshot =
        state
            .accounts
            .snapshot("codex", std::slice::from_ref(&shared_identity), None, None);
    assert!(
        !snapshot[0].has_state,
        "shared identity health should be cleared once no alias resolves to it any more"
    );

    task.abort();
    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_REMOVE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_provisioning_rejects_missing_refresh_and_bad_inputs() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_NEGATIVE",
        "ops:secret-codex-negative",
    );

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": chatgpt_token(4_102_444_800, "acct-no-refresh")
        })))
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_NEGATIVE")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex-negative")
            .header("content-type", "application/json")
    };

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"BAD_NAME"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/ghost/complete",
        gateway.base_url
    )))
    .body(r#"{"code":"code#state"}"#)
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"no-refresh"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state) = authorize_state(&body);
    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/no-refresh/complete",
        gateway.base_url
    )))
    .body(serde_json::json!({"code": format!("code#{state}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!dir.join("no-refresh.json").exists());

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_NEGATIVE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_completion_rejects_oauth_state_mismatch_before_exchange() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_STATE",
        "ops:secret-codex-state",
    );

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": chatgpt_token(4_102_444_800, "acct-unexpected"),
            "refresh_token": "refresh-unexpected"
        })))
        .expect(0)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_STATE")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex-state")
            .header("content-type", "application/json")
    };

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"state-mismatch"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state) = authorize_state(&body);
    assert_ne!(state, "WRONG-state");

    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/state-mismatch/complete",
        gateway.base_url
    )))
    .body(r#"{"code":"the-code#WRONG-state"}"#)
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(token_server.received_requests().await.unwrap().is_empty());
    assert!(!dir.join("state-mismatch.json").exists());

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_STATE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_completion_rejects_access_token_without_account_id() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_NO_ACCOUNT_ID",
        "ops:secret-codex-no-account-id",
    );

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": chatgpt_token_without_account_id(4_102_444_800),
            "refresh_token": "refresh-without-account-id"
        })))
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_NO_ACCOUNT_ID")).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex-no-account-id")
            .header("content-type", "application/json")
    };

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"no-account-id"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state) = authorize_state(&body);

    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/no-account-id/complete",
        gateway.base_url
    )))
    .body(serde_json::json!({"code": format!("the-code#{state}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!dir.join("no-account-id.json").exists());
    token_server.verify().await;

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_NO_ACCOUNT_ID");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_completion_reports_generic_bad_gateway_when_token_exchange_fails() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_EXCHANGE_FAILURE",
        "ops:secret-codex-exchange-failure",
    );

    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_string("invalid_grant: bad code"))
        .expect(1)
        .mount(&token_server)
        .await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_EXCHANGE_FAILURE",
    ))
    .await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder| {
        request
            .header("x-shunt-admin-token", "secret-codex-exchange-failure")
            .header("content-type", "application/json")
    };

    let response = auth(client.post(format!("{}/admin/accounts/codex", gateway.base_url)))
        .body(r#"{"name":"exchange-failure"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let (_, state) = authorize_state(&body);

    let response = auth(client.post(format!(
        "{}/admin/accounts/codex/exchange-failure/complete",
        gateway.base_url
    )))
    .body(serde_json::json!({"code": format!("the-code#{state}")}).to_string())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains("invalid_grant"),
        "the generic 502 must not echo upstream detail"
    );
    assert!(!dir.join("exchange-failure.json").exists());
    token_server.verify().await;

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_EXCHANGE_FAILURE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn codex_cookie_session_mutations_require_a_csrf_token() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CODEX_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
    std::env::set_var(
        "SHUNT_TEST_ADMIN_TOKENS_CODEX_CSRF",
        "ops:secret-codex-csrf",
    );

    let token_server = MockServer::start().await;
    std::env::set_var(
        "SHUNT_CODEX_TOKEN_URL",
        format!("{}/token", token_server.uri()),
    );

    let gateway = start(admin_config("SHUNT_TEST_ADMIN_TOKENS_CODEX_CSRF")).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let base = &gateway.base_url;

    let response = client
        .post(format!("{base}/admin/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("token=secret-codex-csrf")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("shunt_admin_session="))
        .map(|value| value.split(';').next().unwrap().to_string())
        .expect("login sets a session cookie");

    let response = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = response.text().await.unwrap();
    let csrf = html
        .split_once("const CSRF = \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(token, _)| token.to_string())
        .expect("dashboard embeds the CSRF token");

    let response = client
        .post(format!("{base}/admin/accounts/codex"))
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(r#"{"name":"codex-csrf"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = client
        .post(format!("{base}/admin/accounts/codex/codex-csrf/complete"))
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(r#"{"code":"the-code#the-state"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = client
        .delete(format!("{base}/admin/accounts/codex/codex-csrf"))
        .header("cookie", &cookie)
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = client
        .post(format!("{base}/admin/accounts/codex"))
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .header("x-csrf-token", &csrf)
        .body(r#"{"name":"codex-csrf"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a valid session cookie + CSRF token is accepted on the Codex route"
    );

    std::env::remove_var("SHUNT_CODEX_ACCOUNTS_DIR");
    std::env::remove_var("SHUNT_CODEX_TOKEN_URL");
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_CODEX_CSRF");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn admin_config_without_tokens_env_fails_startup() {
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_MISSING");
    let config = admin_config("SHUNT_TEST_ADMIN_TOKENS_MISSING");
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("SHUNT_TEST_ADMIN_TOKENS_MISSING"));
    assert!(error.contains("refusing to run open"));
}
