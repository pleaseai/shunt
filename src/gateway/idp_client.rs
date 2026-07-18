use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::{header, Client, Url};
use serde::Deserialize;

use crate::{gateway::approval::Identity, server::AppState};

use super::ResolvedIdp;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Deserialize)]
pub struct DiscoveredEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct OidcUserInfo {
    sub: String,
    email: String,
    #[serde(default)]
    email_verified: bool,
    name: Option<String>,
}

pub async fn authorization_endpoint(state: &AppState, idp: &ResolvedIdp) -> Result<String> {
    if let Some(endpoint) = &idp.authorization_endpoint {
        return Ok(endpoint.clone());
    }
    Ok(discover(state, idp).await?.authorization_endpoint)
}

async fn token_endpoint(state: &AppState, idp: &ResolvedIdp) -> Result<String> {
    if let Some(endpoint) = &idp.token_endpoint {
        return Ok(endpoint.clone());
    }
    Ok(discover(state, idp).await?.token_endpoint)
}

async fn userinfo_endpoint(state: &AppState, idp: &ResolvedIdp) -> Result<String> {
    if let Some(endpoint) = &idp.userinfo_endpoint {
        return Ok(endpoint.clone());
    }
    Ok(discover(state, idp).await?.userinfo_endpoint)
}

pub async fn exchange_code(
    state: &AppState,
    idp: &ResolvedIdp,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<String> {
    let endpoint = token_endpoint(state, idp).await?;
    let response = state
        .http_client
        .post(endpoint)
        .timeout(REQUEST_TIMEOUT)
        .header(header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", idp.client_id.as_str()),
            ("client_secret", idp.client_secret.as_str()),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("token endpoint request failed")?
        .error_for_status()
        .context("token endpoint returned an error")?;
    let tokens: TokenResponse = response
        .json()
        .await
        .context("token endpoint returned invalid JSON")?;
    if tokens.access_token.trim().is_empty() {
        return Err(anyhow!("token endpoint returned an empty access token"));
    }
    Ok(tokens.access_token)
}

pub async fn fetch_identity(
    state: &AppState,
    idp: &ResolvedIdp,
    access_token: &str,
) -> Result<Identity> {
    let endpoint = userinfo_endpoint(state, idp).await?;
    let info: OidcUserInfo = get_bearer(&state.http_client, &endpoint, access_token)
        .send()
        .await
        .context("userinfo request failed")?
        .error_for_status()
        .context("userinfo endpoint returned an error")?
        .json()
        .await
        .context("userinfo endpoint returned invalid JSON")?;
    if info.sub.trim().is_empty() || info.email.trim().is_empty() || !info.email_verified {
        return Err(anyhow!("userinfo did not return a verified email identity"));
    }
    let name = info
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| local_part(&info.email).to_string());
    Ok(Identity {
        sub: info.sub,
        email: info.email,
        name,
    })
}

async fn discover(state: &AppState, idp: &ResolvedIdp) -> Result<DiscoveredEndpoints> {
    if let Some(cached) = state
        .gateway_stores
        .oidc_discovery
        .lock()
        .expect("gateway OIDC-discovery lock poisoned")
        .get(&idp.issuer)
        .cloned()
    {
        return Ok(cached);
    }
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        idp.issuer.trim_end_matches('/')
    );
    let endpoints: DiscoveredEndpoints = state
        .http_client
        .get(discovery_url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("OIDC discovery request failed")?
        .error_for_status()
        .context("OIDC discovery returned an error")?
        .json()
        .await
        .context("OIDC discovery returned invalid JSON")?;
    validate_endpoint(&endpoints.authorization_endpoint, "authorization_endpoint")?;
    validate_endpoint(&endpoints.token_endpoint, "token_endpoint")?;
    validate_endpoint(&endpoints.userinfo_endpoint, "userinfo_endpoint")?;
    state
        .gateway_stores
        .oidc_discovery
        .lock()
        .expect("gateway OIDC-discovery lock poisoned")
        .insert(idp.issuer.clone(), endpoints.clone());
    Ok(endpoints)
}

fn validate_endpoint(raw: &str, name: &str) -> Result<()> {
    let url = Url::parse(raw).with_context(|| format!("discovered {name} is not a valid URL"))?;
    let safe_transport = url.scheme() == "https"
        || url.scheme() == "http"
            && crate::config::host_is_loopback(url.host_str().unwrap_or_default());
    if !safe_transport
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(anyhow!("discovered {name} is unsafe"));
    }
    Ok(())
}

fn get_bearer(client: &Client, url: &str, token: &str) -> reqwest::RequestBuilder {
    client.get(url).timeout(REQUEST_TIMEOUT).bearer_auth(token)
}

fn local_part(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}
