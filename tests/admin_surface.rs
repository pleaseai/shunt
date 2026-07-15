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
    config::{AdminConfig, AuthMode, Config},
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
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
    });
    config
}

async fn start(mut config: Config) -> Gateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

fn unique_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-admin-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
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

#[tokio::test]
async fn admin_routes_are_absent_without_the_block() {
    if !can_bind_loopback() {
        return;
    }
    // Default config has no [server.admin], so the routes must not be registered.
    let gateway = start(Config::default()).await;
    let client = reqwest::Client::new();
    for route in ["/admin", "/admin/login", "/admin/pool"] {
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
