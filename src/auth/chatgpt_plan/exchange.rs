//! The authorization-code exchange.

use anyhow::bail;
use serde::Deserialize;

use super::authorize::RESOURCE;

/// Deadline for the code exchange, request and body together.
pub(crate) const EXCHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The token endpoint's success body. No `Debug`: it holds live credentials.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<u64>,
    /// Space-separated.
    pub scope: Option<String>,
    pub earliest_refresh_at: Option<u64>,
}

/// POST the code with its PKCE verifier through the redirect-hardened client,
/// so a token endpoint cannot 3xx the code to another host. Errors name only
/// the status: nothing from the request or the body can reach a terminal.
pub async fn exchange_code(
    token_url: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<TokenResponse> {
    let response = crate::auth::shared::token_refresh_client()
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("resource", RESOURCE),
        ])
        .timeout(EXCHANGE_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("the ChatGPT token endpoint did not answer in time")
            } else {
                anyhow::anyhow!("failed to reach the ChatGPT token endpoint")
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        bail!("ChatGPT token exchange failed ({status})");
    }
    response.json().await.map_err(|error| {
        if error.is_timeout() {
            anyhow::anyhow!("the ChatGPT token endpoint did not answer in time")
        } else {
            anyhow::anyhow!("the ChatGPT token endpoint returned an unreadable response")
        }
    })
}
