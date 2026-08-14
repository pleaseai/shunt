//! `shunt login antigravity` — Google authorization-code flow for an
//! Antigravity subscription.
//!
//! Unlike shunt's other loopback logins this pins the callback port: the
//! redirect URI registered for Antigravity's OAuth client is
//! `http://localhost:51121/oauth-callback`, so an ephemeral port would be
//! rejected as a redirect_uri mismatch.
//!
//! The project id is resolved here rather than on the first request, because
//! provisioning a first-time account polls `onboardUser` for up to ten seconds
//! — acceptable in an interactive login, not in front of a proxied turn.

use std::time::Duration;

use anyhow::{bail, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::Deserialize;

use crate::auth::callback::{CallbackConfig, CallbackServer};

use super::auth::{
    write_stored, StoredAuth, TokenResponse, AUTH_URL, CALLBACK_PATH, CALLBACK_PORT, CLIENT_ID,
    CLIENT_SECRET, SCOPES, TOKEN_URL, USERINFO_URL,
};
use super::default_antigravity_auth_path;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const CALLBACK_CONFIG: CallbackConfig = CallbackConfig {
    label: "Antigravity",
    port: CALLBACK_PORT,
    path: CALLBACK_PATH,
    // Matches the registered redirect URI. `CallbackServer` binds both loopback
    // families on a fixed port, so the `localhost` spelling cannot hang on ::1.
    host: "localhost",
};

#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    email: Option<String>,
}

pub async fn run() -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let mut random = [0_u8; 32];
    rand::rng().fill_bytes(&mut random);
    let state = URL_SAFE_NO_PAD.encode(random);

    let callback = CallbackServer::bind(CALLBACK_CONFIG, state.clone())
        .await
        .with_context(|| {
            format!(
                "failed to start the Antigravity OAuth callback on port {CALLBACK_PORT}. \
                 That port is fixed by Antigravity's registered redirect URI, so it cannot \
                 be reassigned — free it and try again."
            )
        })?;
    let redirect_uri = callback.redirect_uri();
    let auth_url = build_auth_url(&state, &redirect_uri);

    println!("Open this URL to authenticate with Antigravity:\n\n    {auth_url}\n");
    if let Err(error) = open_url(&auth_url) {
        eprintln!("Could not open browser automatically: {error}");
    }

    let code = callback.wait_for_code(CALLBACK_TIMEOUT).await?;
    let tokens = exchange_code(&client, TOKEN_URL, &code, &redirect_uri).await?;
    let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Antigravity returned no refresh token; the login would expire within the hour. \
             Retry the login — the authorization URL requests offline access."
        )
    })?;

    let email = fetch_email(&client, USERINFO_URL, &tokens.access_token)
        .await
        .unwrap_or_else(|error| {
            // The email only labels the credential; failing to read it must not
            // discard a working token.
            eprintln!("Could not read the account email ({error}); continuing.");
            None
        });

    let expiry_date = expiry_millis(tokens.expires_in.unwrap_or(3600));
    let mut stored = StoredAuth {
        access_token: tokens.access_token,
        refresh_token,
        expiry_date,
        email: email.clone(),
        project_id: None,
    };

    let path = default_antigravity_auth_path();
    let store = super::auth::AntigravityAuthStore::new(path.clone(), client.clone());
    match store.discover_project(&stored.access_token).await {
        Ok(project_id) => stored.project_id = Some(project_id),
        Err(error) => {
            // Persist the credential regardless: discovery is retried on the
            // first request, and throwing away a valid token here would force
            // the whole browser flow again for a recoverable failure.
            eprintln!(
                "Signed in, but could not resolve the Code Assist project ({}). \
                 It will be retried on the first request.",
                error.message
            );
        }
    }

    let write_path = path.clone();
    let to_write = stored.clone();
    tokio::task::spawn_blocking(move || write_stored(&write_path, &to_write))
        .await
        .map_err(|error| anyhow::anyhow!("Antigravity auth write task failed: {error}"))?
        .with_context(|| {
            format!(
                "failed to write Antigravity credentials to {}",
                path.display()
            )
        })?;

    match email {
        Some(email) => println!(
            "Login successful for {email}. Credentials saved to {}",
            path.display()
        ),
        None => println!("Login successful. Credentials saved to {}", path.display()),
    }
    Ok(())
}

pub(crate) fn build_auth_url(state: &str, redirect_uri: &str) -> String {
    let mut url = reqwest::Url::parse(AUTH_URL).expect("Antigravity auth endpoint is a valid URL");
    url.query_pairs_mut()
        // `offline` + `consent` are what make Google return a refresh token; a
        // login without one expires in an hour and cannot be renewed.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("state", state);
    url.to_string()
}

async fn exchange_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("redirect_uri", redirect_uri),
    ];
    let response = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .context("Antigravity token exchange failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("could not read the Antigravity token response")?;
    if !status.is_success() {
        bail!("Antigravity token exchange failed (HTTP {status}): {body}");
    }
    serde_json::from_str::<TokenResponse>(&body)
        .context("invalid JSON in the Antigravity token response")
}

async fn fetch_email(
    client: &reqwest::Client,
    userinfo_url: &str,
    access_token: &str,
) -> anyhow::Result<Option<String>> {
    let response = client
        .get(userinfo_url)
        .bearer_auth(access_token)
        .header("User-Agent", super::version::user_agent())
        .send()
        .await
        .context("Google userinfo request failed")?;
    if !response.status().is_success() {
        bail!(
            "Google userinfo request failed (HTTP {})",
            response.status()
        );
    }
    let info = response
        .json::<UserInfo>()
        .await
        .context("invalid JSON in the Google userinfo response")?;
    Ok(info
        .email
        .map(|email| email.trim().to_string())
        .filter(|email| !email.is_empty()))
}

fn expiry_millis(expires_in: u64) -> Option<u64> {
    std::time::SystemTime::now()
        .checked_add(Duration::from_secs(expires_in))
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
}

fn open_url(url: &str) -> anyhow::Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()?
    } else if cfg!(target_os = "windows") {
        // Open via rundll32 FileProtocolHandler rather than `cmd /c start`: the
        // login URL contains `&` query separators, which cmd.exe would treat as
        // command separators and truncate the URL.
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()?
    } else {
        std::process::Command::new("xdg-open").arg(url).status()?
    };
    if !status.success() {
        bail!("browser open command exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn auth_url_requests_offline_access_and_every_scope() {
        let url = build_auth_url("state-1", "http://localhost:51121/oauth-callback");
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        // Without offline+consent Google withholds the refresh token, and the
        // login silently becomes a one-hour credential.
        assert_eq!(
            pairs.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(pairs.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(pairs.get("state").map(String::as_str), Some("state-1"));
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some("http://localhost:51121/oauth-callback")
        );

        let scope = pairs.get("scope").expect("scope must be requested");
        for expected in SCOPES {
            assert!(
                scope.contains(expected),
                "scope {expected} missing from {scope}"
            );
        }
        // The two scopes a Gemini CLI token never carries are the reason that
        // credential cannot be reused here; losing them would silently produce
        // a token the Antigravity backend rejects.
        assert!(scope.contains("cclog"));
        assert!(scope.contains("experimentsandconfigs"));
    }

    #[tokio::test]
    async fn code_exchange_returns_the_token_pair() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3599
            })))
            .mount(&server)
            .await;

        let tokens = exchange_code(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "code-1",
            "http://localhost:51121/oauth-callback",
        )
        .await
        .unwrap();

        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.expires_in, Some(3599));
    }

    #[tokio::test]
    async fn code_exchange_surfaces_the_upstream_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "redirect_uri_mismatch"
            })))
            .mount(&server)
            .await;

        let error = exchange_code(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "code-1",
            "http://127.0.0.1:1/oauth-callback",
        )
        .await
        .unwrap_err();

        // A redirect_uri mismatch is the most likely first-run failure; the
        // operator needs to see which one it was.
        assert!(
            error.to_string().contains("redirect_uri_mismatch"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn userinfo_failure_is_reported_rather_than_returning_an_empty_email() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let error = fetch_email(&reqwest::Client::new(), &server.uri(), "token")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("403"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn userinfo_without_an_email_field_yields_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let email = fetch_email(&reqwest::Client::new(), &server.uri(), "token")
            .await
            .unwrap();
        assert_eq!(email, None);
    }
}
