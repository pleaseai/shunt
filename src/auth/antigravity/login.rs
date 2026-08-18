//! `shunt login antigravity` — Google authorization-code flow for an
//! Antigravity subscription.
//!
//! Unlike shunt's other loopback logins this pins the callback port: the
//! redirect URI registered for Antigravity's OAuth client is
//! `http://localhost:51121/oauth-callback`, so an ephemeral port would be
//! rejected as a redirect_uri mismatch.
//!
//! The project id is resolved here rather than on the first request, because
//! provisioning a first-time account can poll a long-running operation for a
//! while — `onboard_user` POSTs `onboardUser` once (up to two
//! `ONBOARD_REQUEST_TIMEOUT` windows: the send and, separately, the body
//! read), then, if the account isn't onboarded yet, polls the operation the
//! POST returned (`ONBOARD_POLL_INTERVAL` apart) for up to
//! `ONBOARD_POLL_DEADLINE`: 2 * 30s + 300s = 360s, roughly 6 minutes for
//! `onboard_user` alone. Chained after `refresh_call` and
//! `discover_project`'s own `loadCodeAssist` round trip — each of which can
//! already spend up to two `CREDENTIAL_REQUEST_TIMEOUT` windows — that is
//! roughly 2 * 60s + 360s = 480s, about 8 minutes.
//!
//! That full 8 minutes is reachable only from this module's `run()`, which
//! calls `discover_project` directly with no outer bound. On the request path
//! (`get_valid` via `resolve_credential`) the whole chain is wrapped in
//! `ANTIGRAVITY_CREDENTIAL_TIMEOUT` (120s, `src/auth/mod.rs`), so onboarding
//! there gets only whatever is left of that budget and never approaches
//! `ONBOARD_POLL_DEADLINE`. The asymmetry is deliberate, and is why the
//! project id is resolved here: 8 minutes is acceptable in an interactive
//! login, not in front of a proxied turn.

use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::auth::callback::{CallbackConfig, CallbackServer};
use crate::auth::shared::{generate_pkce, PkceChallenge};

use super::auth::{
    diagnostic_body, write_stored, StoredAuth, TokenResponse, AUTH_URL, CALLBACK_PATH,
    CALLBACK_PORT, CLIENT_ID, CLIENT_SECRET, SCOPES, TOKEN_URL, USERINFO_URL,
};
use super::default_antigravity_auth_path;

/// Bound on draining a rejected userinfo response body for the error message
/// below. Separate from [`USERINFO_REQUEST_TIMEOUT`] because it bounds only
/// the diagnostic read of an already-received rejection, not the exchange
/// that produced it.
const USERINFO_BODY_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the userinfo request and on reading its successful body.
///
/// The email only labels the stored credential, and `run` already treats a
/// failed lookup as non-fatal — but "failed" and "never answers" are not the
/// same thing. Unbounded, a userinfo endpoint that accepts the request and
/// then stalls hangs `shunt login antigravity` forever *after* the tokens
/// have already been exchanged, so the operator interrupts a completed OAuth
/// flow and is left with no credential on disk. Timing out turns that into
/// the failure `run` already degrades gracefully: the credential is written
/// with `email: None`.
const USERINFO_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the token exchange request and on reading its response.
///
/// Unlike [`USERINFO_REQUEST_TIMEOUT`], a stall here must not fail open: the
/// authorization code is single-use and already spent by the time this call
/// is made, and `run` writes no credential until it returns. An unbounded
/// exchange that accepts the request and then never answers hangs forever
/// with the code burned and nothing on disk, forcing the operator to redo
/// the whole browser flow. Timing out turns that into an ordinary,
/// immediately visible error instead.
///
/// 30s matches `CREDENTIAL_REQUEST_TIMEOUT` in `auth.rs`, which bounds
/// `refresh_call` — the same Google token endpoint, just a different
/// `grant_type`.
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Wall-clock budget for the one-shot client-version refresh at login.
///
/// `version::refresh_now` is bounded internally, but by the background
/// refresher's `FETCH_TIMEOUT` applied to each of its two stages — up to ~20s.
/// That is a reasonable bound for a task nobody is waiting on, and a poor one
/// directly after a browser flow, where the operator is watching a terminal
/// and the thing being fetched only labels the User-Agent. Five seconds buys
/// the fresh fingerprint on any healthy network and gives up quickly
/// otherwise; the refresh fails open, so giving up costs nothing but
/// freshness.
const LOGIN_VERSION_REFRESH_BUDGET: Duration = Duration::from_secs(5);

const CALLBACK_CONFIG: CallbackConfig = CallbackConfig {
    label: "Antigravity",
    port: CALLBACK_PORT,
    path: CALLBACK_PATH,
    // Matches the registered redirect URI. `CallbackServer` binds both loopback
    // families on a fixed port, and refuses to start rather than silently fall
    // back to v4-only if another process already holds [::1] on this port —
    // closing that one squatted-port case. Any other bind failure on [::1]
    // (permission denied, resource exhaustion, ...) still falls back to
    // v4-only, best-effort, so the `localhost` spelling can still hang if the
    // browser resolves it to ::1 first in one of those cases.
    host: "localhost",
};

#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    email: Option<String>,
}

/// `base_url` is the Code Assist host to discover the project against — the
/// same value `resolve_credential` passes `AntigravityAuthStore::new` on the
/// request path (see `src/auth/mod.rs`). Callers resolve their own default
/// (`main.rs`'s `antigravity` login arm mirrors the `cursor` one) since login
/// must not require a fully valid gateway config.
pub async fn run(base_url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let PkceChallenge {
        verifier,
        challenge,
        state,
    } = generate_pkce();

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
    let auth_url = build_auth_url(&challenge, &state, &redirect_uri);

    println!("Open this URL to authenticate with Antigravity:\n\n    {auth_url}\n");
    if let Err(error) = crate::auth::shared::open_url(&auth_url) {
        eprintln!("Could not open browser automatically: {error}");
    }

    let code = callback.wait_for_code(CALLBACK_TIMEOUT).await?;
    let tokens = exchange_code(
        &client,
        TOKEN_URL,
        &code,
        &redirect_uri,
        &verifier,
        TOKEN_EXCHANGE_TIMEOUT,
    )
    .await?;
    let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Antigravity returned no refresh token; the login would expire within the hour. \
             Retry the login — the authorization URL requests offline access."
        )
    })?;

    let email = fetch_email(
        &client,
        USERINFO_URL,
        &tokens.access_token,
        USERINFO_REQUEST_TIMEOUT,
    )
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

    // Login never starts `spawn_refresher` (that only runs from the
    // serve/reload paths), so without this one-shot refresh the discovery call
    // below would always send the compiled-in fallback User-Agent instead of a
    // current one.
    //
    // Bounded again here, on top of the bound inside `refresh_now`: that one is
    // sized for the background refresher, where nobody is waiting. Here someone
    // just finished a browser flow and is watching a terminal, so the refresh
    // gets a short budget and no more. Discarding the result is deliberate —
    // `refresh_now` fails open to the compiled-in version, so cutting it short
    // costs only the freshness of a User-Agent string, never the login.
    let _ = tokio::time::timeout(
        LOGIN_VERSION_REFRESH_BUDGET,
        super::version::refresh_now(&client),
    )
    .await;

    let path = default_antigravity_auth_path();
    let store = super::auth::AntigravityAuthStore::new(path.clone(), client.clone(), base_url);
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

pub(crate) fn build_auth_url(challenge: &str, state: &str, redirect_uri: &str) -> String {
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
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

async fn exchange_code(
    // The injected client follows redirects freely; this POST carries the
    // PKCE verifier and receives the refresh_token, so it goes through the
    // redirect-hardened `token_refresh_client()` instead — a permitted token
    // endpoint must not be able to 3xx the exchange to a plaintext/off-loopback
    // host. `fetch_email`/`discover_project` keep using the caller's client.
    _client: &reqwest::Client,
    token_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    timeout: Duration,
) -> anyhow::Result<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    let response = tokio::time::timeout(
        timeout,
        crate::auth::shared::token_refresh_client()
            .post(token_url)
            .form(&params)
            .send(),
    )
    .await
    .context("Antigravity token exchange timed out")?
    .context("Antigravity token exchange failed")?;
    let status = response.status();
    let body = tokio::time::timeout(timeout, response.text())
        .await
        .context("Antigravity token response read timed out")?
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
    timeout: Duration,
) -> anyhow::Result<Option<String>> {
    let response = tokio::time::timeout(
        timeout,
        client
            .get(userinfo_url)
            .bearer_auth(access_token)
            .header("User-Agent", super::version::user_agent())
            .send(),
    )
    .await
    .context("Google userinfo request timed out")?
    .context("Google userinfo request failed")?;
    if !response.status().is_success() {
        // Drain the body (bounded by USERINFO_BODY_DRAIN_TIMEOUT) rather than
        // dropping the response un-drained, which would strand the reqwest
        // connection instead of returning it to the pool — and fold it into
        // the error, since a bare status code does not say whether Google
        // rejected the scope, the token, or something else entirely.
        let status = response.status();
        let body = diagnostic_body(USERINFO_BODY_DRAIN_TIMEOUT, response).await;
        bail!("Google userinfo request failed (HTTP {status}): {body}");
    }
    let info = tokio::time::timeout(timeout, response.json::<UserInfo>())
        .await
        .context("Google userinfo response read timed out")?
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn auth_url_requests_offline_access_and_every_scope() {
        let url = build_auth_url(
            "challenge-1",
            "state-1",
            "http://localhost:51121/oauth-callback",
        );
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

    #[test]
    fn auth_url_carries_the_pkce_challenge() {
        // Without `code_challenge`/`code_challenge_method` Google's authorization
        // server issues a code that the token exchange's `code_verifier` cannot
        // redeem, so both must survive on the URL — this fails if either is
        // dropped.
        let url = build_auth_url(
            "challenge-value",
            "state-1",
            "http://localhost:51121/oauth-callback",
        );
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        let challenge = pairs
            .get("code_challenge")
            .expect("code_challenge must be present");
        assert!(!challenge.is_empty());
        assert_eq!(challenge, "challenge-value");
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
            "verifier-1",
            TOKEN_EXCHANGE_TIMEOUT,
        )
        .await
        .unwrap();

        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.expires_in, Some(3599));
    }

    #[tokio::test]
    async fn code_exchange_posts_the_pkce_verifier() {
        // The verifier must be the exact one that produced the authorization
        // URL's challenge — if it were dropped or a different value sent,
        // Google's token endpoint would reject the exchange.
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

        exchange_code(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "code-1",
            "http://localhost:51121/oauth-callback",
            "the-verifier-value",
            TOKEN_EXCHANGE_TIMEOUT,
        )
        .await
        .unwrap();

        let requests = server
            .received_requests()
            .await
            .expect("mock records requests");
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(
            body.contains("code_verifier=the-verifier-value"),
            "token exchange body missing code_verifier: {body}"
        );
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
            "verifier-1",
            TOKEN_EXCHANGE_TIMEOUT,
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
    async fn code_exchange_refuses_a_redirect_to_an_offhost_plaintext_target() {
        // The redirect-hardening guard lives in `auth::shared::token_refresh_client`
        // and is exercised directly in `codex/auth.rs`; this proves `exchange_code`
        // itself is actually wired through it, rather than the passed-in `client`
        // (which follows redirects freely).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", "http://evil.example/token"),
            )
            .mount(&server)
            .await;

        let error = exchange_code(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "code-1",
            "http://localhost:51121/oauth-callback",
            "verifier-1",
            TOKEN_EXCHANGE_TIMEOUT,
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Antigravity token exchange failed"),
            "expected the refused redirect to surface as an exchange failure, got: {error}"
        );
    }

    #[tokio::test]
    async fn a_stalled_token_exchange_times_out_rather_than_hanging_the_login() {
        // The authorization code is single-use and already spent by the time
        // this call is made, and `run` writes no credential until it
        // returns — so an endpoint that accepts the request and then never
        // answers would hang forever with the code burned and nothing on
        // disk. Unlike the userinfo stall, this must surface as `Err`
        // (fail-closed): a stalled exchange is fatal to the login, only the
        // hang is being fixed.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "access_token": "access-1",
                        "refresh_token": "refresh-1",
                        "expires_in": 3599
                    }))
                    .set_delay(Duration::from_secs(30)),
            )
            .mount(&server)
            .await;

        // Deliberately not the production constant: this asserts the bound
        // exists and fires, without making the suite wait for it.
        let error = exchange_code(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "code-1",
            "http://localhost:51121/oauth-callback",
            "verifier-1",
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("timed out"),
            "expected a timeout error, got: {error}"
        );
    }

    #[tokio::test]
    async fn userinfo_failure_is_reported_rather_than_returning_an_empty_email() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let error = fetch_email(
            &reqwest::Client::new(),
            &server.uri(),
            "token",
            USERINFO_REQUEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("403"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn userinfo_failure_folds_the_upstream_body_into_the_error() {
        // A bare status code does not say whether Google rejected the scope,
        // the token, or something else entirely; the drained body must be
        // folded into the error rather than discarded.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficient_scope"))
            .mount(&server)
            .await;

        let error = fetch_email(
            &reqwest::Client::new(),
            &server.uri(),
            "token",
            USERINFO_REQUEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("insufficient_scope"),
            "expected the upstream body in the error, got: {error}"
        );
    }

    #[tokio::test]
    async fn a_stalled_userinfo_endpoint_times_out_rather_than_hanging_the_login() {
        // The tokens are already exchanged by the time `run` calls this, so an
        // endpoint that accepts the request and then never answers would hang a
        // completed OAuth flow forever and leave no credential on disk. The
        // bound turns that into an ordinary error, which `run` already degrades
        // to `email: None`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"email":"someone@example.com"}"#)
                    .set_delay(Duration::from_secs(30)),
            )
            .mount(&server)
            .await;

        // Deliberately not the production constant: this asserts the bound
        // exists and fires, without making the suite wait for it.
        let error = fetch_email(
            &reqwest::Client::new(),
            &server.uri(),
            "token",
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("timed out"),
            "expected a timeout error, got: {error}"
        );
    }

    #[tokio::test]
    async fn userinfo_without_an_email_field_yields_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let email = fetch_email(
            &reqwest::Client::new(),
            &server.uri(),
            "token",
            USERINFO_REQUEST_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(email, None);
    }
}
