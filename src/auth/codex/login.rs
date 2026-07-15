//! `shunt login codex` still imports the current `codex login` credential into a
//! named shunt-owned account. The module also exposes the ChatGPT OAuth URL and
//! code-exchange internals used by the admin web surface; the CLI does not run
//! this flow directly.
//!
//! Codex has no setup-token concept. Imported and web-provisioned accounts both
//! use the refreshable `codex login` auth.json shape.

use std::path::PathBuf;

use anyhow::{bail, Context};
use serde_json::Value;

use super::{auth, store};

pub(crate) const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(crate) const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
// Mirror the current `codex login` authorize scope exactly (openai/codex
// `codex-rs/login/src/server.rs::build_authorize_url`) so a web-provisioned
// account is authorization-equivalent to a `codex login` one — including the
// connector scopes.
pub(crate) const SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

pub(crate) fn build_authorize_url(
    challenge: &str,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", auth::CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("state", state)
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "codex_cli_rs");
    Ok(url)
}

pub(crate) async fn exchange_code(
    // The injected proxy client follows redirects freely; the code-exchange POST
    // carries the one-time authorization code and PKCE verifier, so it goes through
    // the redirect-hardened `token_refresh_client()` instead — a permitted token
    // endpoint must not be able to 3xx the code to a plaintext/off-loopback host and
    // defeat the initial-URL-only `sanitize_token_url` guard.
    _client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    token_url: &str,
) -> anyhow::Result<auth::RefreshResponse> {
    let response = crate::auth::shared::token_refresh_client()
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", auth::CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("failed to exchange ChatGPT authorization code")?;
    let status = response.status();
    if !status.is_success() {
        bail!("ChatGPT token exchange failed ({status})");
    }
    let text = response
        .text()
        .await
        .context("invalid ChatGPT token exchange response")?;
    let value: Value =
        serde_json::from_str(&text).context("invalid ChatGPT token exchange response")?;
    auth::parse_refresh_response(&value)
        .ok_or_else(|| anyhow::anyhow!("invalid ChatGPT token exchange response"))
}

pub async fn run(name: &str) -> anyhow::Result<()> {
    store::validate_account_name(name)?;
    let path = import_current_login(name).await?;
    println!(
        "Codex account {name:?} saved to {}. Add a name-only account entry to use it.",
        path.display()
    );
    Ok(())
}

async fn import_current_login(name: &str) -> anyhow::Result<PathBuf> {
    let source = crate::auth::default_codex_auth_path();
    let name = name.to_string();
    let source_display = source.display().to_string();
    tokio::task::spawn_blocking(move || {
        store::import_auth(&name, &source)
            .with_context(|| format!("failed to import {source_display}; run `codex login` first"))
    })
    .await
    .context("Codex credential import task failed")?
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use wiremock::{
        matchers::{body_string, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    #[test]
    fn authorization_url_contains_codex_cli_parameters() {
        let url = build_authorize_url("challenge", "state", REDIRECT_URI).unwrap();
        let params = url.query_pairs().collect::<HashMap<_, _>>();

        for (key, expected) in [
            ("response_type", "code"),
            ("client_id", auth::CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", "challenge"),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("state", "state"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "codex_cli_rs"),
        ] {
            assert_eq!(
                params.get(key).map(|value| value.as_ref()),
                Some(expected),
                "missing or incorrect {key}"
            );
        }
    }

    #[tokio::test]
    async fn token_exchange_posts_form_and_parses_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header(
                "content-type",
                "application/x-www-form-urlencoded",
            ))
            .and(body_string(format!(
                "grant_type=authorization_code&code=oauth-code&redirect_uri={}&client_id={}&code_verifier=verifier",
                "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
                auth::CLIENT_ID
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "id_token": "id"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tokens = exchange_code(
            &reqwest::Client::new(),
            "oauth-code",
            "verifier",
            REDIRECT_URI,
            &format!("{}/token", server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(tokens.id_token.as_deref(), Some("id"));
    }
}
