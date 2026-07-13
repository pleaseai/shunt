//! M9 admin web surface — end-to-end gateway behavior.
//!
//! The admin routes exist only when `[server.admin]` is configured, authenticate
//! every request against a separate admin credential, and never return or log the
//! provisioned setup token. The full add → complete → list → pool → delete flow is
//! driven against a wiremock Claude token endpoint.

use std::{net::SocketAddr, path::PathBuf, time::SystemTime};

use reqwest::StatusCode;
use shunt::{
    config::{AdminConfig, AuthMode, Config},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

/// Serializes tests that mutate the shared `SHUNT_CLAUDE_*` process env. A tokio
/// mutex (held across `.await`) so it is safe over the async request calls.
static CLAUDE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let (app, _shared) = server::build_router(config).unwrap();
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
async fn provisioning_flow_stores_account_without_leaking_the_token() {
    if !can_bind_loopback() {
        return;
    }
    let _lock = CLAUDE_ENV_LOCK.lock().await;
    let dir = unique_dir();
    std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
    std::env::set_var("SHUNT_TEST_ADMIN_TOKENS_C", "ops:secret-c");

    // Mock the Claude token exchange endpoint.
    let token_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
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

#[test]
fn admin_config_without_tokens_env_fails_startup() {
    std::env::remove_var("SHUNT_TEST_ADMIN_TOKENS_MISSING");
    let config = admin_config("SHUNT_TEST_ADMIN_TOKENS_MISSING");
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("SHUNT_TEST_ADMIN_TOKENS_MISSING"));
    assert!(error.contains("refusing to run open"));
}
