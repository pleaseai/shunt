use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, Response, Url};
use serde::Deserialize;

use crate::{gateway::approval::Identity, server::AppState};

use super::ResolvedIdp;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_BODY_LIMIT: usize = 4096;
const ERROR_FIELD_LIMIT: usize = 200;

#[derive(Clone, Debug, Deserialize)]
pub struct DiscoveredEndpoints {
    pub issuer: String,
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

#[derive(Default, Deserialize)]
struct OidcErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
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
        .gateway_stores
        .oidc_client
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
        .context("token endpoint request failed")?;
    let response = require_success(response, "token endpoint").await?;
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
    let response = state
        .gateway_stores
        .oidc_client
        .get(endpoint)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(access_token)
        .send()
        .await
        .context("userinfo request failed")?;
    let info: OidcUserInfo = require_success(response, "userinfo endpoint")
        .await?
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
    let response = state
        .gateway_stores
        .oidc_client
        .get(discovery_url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("OIDC discovery request failed")?;
    let endpoints: DiscoveredEndpoints = require_success(response, "OIDC discovery")
        .await?
        .json()
        .await
        .context("OIDC discovery returned invalid JSON")?;
    if endpoints.issuer != idp.issuer {
        return Err(anyhow!(
            "OIDC discovery issuer does not match configured issuer"
        ));
    }
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

async fn require_success(response: Response, endpoint: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(anyhow!(
        "{endpoint} returned HTTP {status}{}",
        sanitized_error_suffix(response).await
    ))
}

async fn sanitized_error_suffix(response: Response) -> String {
    let bytes = read_limited_body(response).await;
    let Ok(error) = serde_json::from_slice::<OidcErrorResponse>(&bytes) else {
        return String::new();
    };
    let code = error.error.as_deref().and_then(sanitize_error_code);
    let description = error
        .error_description
        .as_deref()
        .and_then(sanitize_error_description);
    match (code, description) {
        (Some(code), Some(description)) => {
            format!(": error={code}, error_description={description}")
        }
        (Some(code), None) => format!(": error={code}"),
        (None, Some(description)) => format!(": error_description={description}"),
        (None, None) => String::new(),
    }
}

async fn read_limited_body(response: Response) -> Vec<u8> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_LIMIT {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_LIMIT - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    body
}

fn sanitize_error_code(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    valid.then(|| value.to_string())
}

fn sanitize_error_description(value: &str) -> Option<String> {
    let sanitized: String = value
        .split_ascii_whitespace()
        .filter(|word| {
            let lower = word.to_ascii_lowercase();
            !lower.contains("token")
                && !lower.contains("secret")
                && !lower.contains("code=")
                && !lower.contains("authorization:")
                && !lower.starts_with("bearer")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|character| !character.is_control())
        .take(ERROR_FIELD_LIMIT)
        .collect();
    (!sanitized.trim().is_empty()).then_some(sanitized)
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

fn local_part(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_fields_are_sanitized_and_bounded() {
        assert_eq!(
            sanitize_error_code("invalid_grant"),
            Some("invalid_grant".into())
        );
        assert_eq!(sanitize_error_code("bad code"), None);
        assert_eq!(
            sanitize_error_description("bad\nclient secret=do-not-log retry"),
            Some("bad client retry".into())
        );
        assert_eq!(sanitize_error_description("token secret"), None);
        assert_eq!(
            sanitize_error_description(&"x".repeat(300)).unwrap().len(),
            200
        );
    }
}
