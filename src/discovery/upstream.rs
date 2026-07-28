//! Live `GET /v1/models` passthrough for the Anthropic upstream.
//!
//! Discovery answers from local config plus a builtin snapshot, but the real
//! catalog is **credential-scoped**: the same endpoint returns a different list
//! for an `x-api-key` caller, a Claude subscription OAuth bearer, and the
//! reference apps gateway. A shared cache would therefore serve one caller's
//! entitlement view to another, so this module deliberately holds **no state**
//! — every request asks upstream with that request's own credential and gets
//! that caller's own answer.
//!
//! Every failure path is a silent `None`: discovery then falls back to the
//! builtin snapshot rather than degrading the response.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::{
    auth::{resolve_claude_account, resolve_credential, Credential},
    config::{ApiKeyHeader, AuthMode, Config, ProviderConfig, ProviderKind},
    routing::{AdapterKind, Route},
    server::AppState,
};

use super::ModelEntry;

/// Discovery is documented to answer well under 3 s, and this call sits in that
/// budget, so it fails over to the builtin snapshot quickly.
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// The upstream page size maximum. The catalog is ~11 entries today, so this is
/// a single request in practice; the page loop exists for correctness.
const PAGE_LIMIT: u32 = 1000;
/// Backstop against a misbehaving upstream that always reports `has_more`.
const MAX_PAGES: usize = 5;

#[derive(Debug, Deserialize)]
struct UpstreamList {
    data: Vec<UpstreamModel>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

/// Upstream entry. Unknown fields are ignored, and every field past `id` is
/// optional so a partial or older upstream shape still yields usable entries.
#[derive(Debug, Deserialize)]
struct UpstreamModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    /// Relayed verbatim. shunt does not interpret it, but a client that reads
    /// effort levels or thinking support should see exactly what upstream said.
    #[serde(default)]
    capabilities: Option<serde_json::Value>,
}

impl From<UpstreamModel> for ModelEntry {
    fn from(model: UpstreamModel) -> Self {
        Self {
            entry_type: "model",
            id: model.id,
            display_name: model.display_name,
            created_at: model.created_at,
            max_input_tokens: model.max_input_tokens,
            max_tokens: model.max_tokens,
            capabilities: model.capabilities,
        }
    }
}

/// Fetch the caller's own model list from the Anthropic upstream.
///
/// Returns `None` — and the caller falls back to the builtin snapshot — when
/// there is no Anthropic-kind upstream configured, when no credential can be
/// resolved for it, or when the request fails, times out, or does not parse.
pub(super) async fn fetch(state: &AppState, inbound: &HeaderMap) -> Option<Vec<ModelEntry>> {
    let (name, provider) = anthropic_provider(&state.config)?;
    let headers = upstream_headers(state, name, provider, inbound).await?;
    let base = provider.base_url.trim_end_matches('/');

    let mut collected: Vec<ModelEntry> = Vec::new();
    let mut after_id: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut request = state
            .http_client
            .get(format!("{base}/v1/models"))
            .timeout(FETCH_TIMEOUT)
            .query(&[("limit", PAGE_LIMIT.to_string())]);
        if let Some(cursor) = after_id.as_deref() {
            request = request.query(&[("after_id", cursor)]);
        }
        for (key, value) in headers.iter() {
            request = request.header(key, value);
        }

        let response = match request.send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::debug!(
                    provider = %name,
                    status = %response.status(),
                    "upstream /v1/models rejected the discovery refresh; using builtin catalog"
                );
                return None;
            }
            Err(error) => {
                tracing::debug!(
                    provider = %name,
                    %error,
                    "upstream /v1/models unreachable; using builtin catalog"
                );
                return None;
            }
        };

        let page: UpstreamList = match response.json().await {
            Ok(page) => page,
            Err(error) => {
                tracing::debug!(
                    provider = %name,
                    %error,
                    "upstream /v1/models body did not parse; using builtin catalog"
                );
                return None;
            }
        };

        let next = page.last_id.clone();
        let has_more = page.has_more;
        collected.extend(page.data.into_iter().map(ModelEntry::from));

        // A `has_more` with no cursor cannot be followed; stop with what we have
        // rather than re-request the first page forever.
        match (has_more, next) {
            (true, Some(cursor)) => after_id = Some(cursor),
            _ => break,
        }
    }

    if collected.is_empty() {
        return None;
    }
    tracing::info!(
        provider = %name,
        models = collected.len(),
        "refreshed /v1/models discovery from upstream"
    );
    Some(collected)
}

/// The Anthropic-kind upstream to ask, preferring `server.default_provider`.
///
/// Gated on `kind` rather than an `anthropic.com` host check so an
/// Anthropic-compatible gateway is asked about *its own* catalog. That is
/// strictly more honest than advertising Claude ids it cannot serve, and a
/// backend without the endpoint simply fails the fetch and falls back.
fn anthropic_provider(config: &Config) -> Option<(&str, &ProviderConfig)> {
    let default = config.server.default_provider.as_str();
    if let Some(provider) = config
        .provider(default)
        .filter(|provider| provider.kind == ProviderKind::Anthropic)
    {
        return Some((default, provider));
    }
    config.upstream_order.iter().find_map(|name| {
        config
            .provider(name)
            .filter(|provider| provider.kind == ProviderKind::Anthropic)
            .map(|provider| (name.as_str(), provider))
    })
}

/// Build the outbound auth headers, mirroring what the Messages path sends for
/// the same `auth` mode. `None` means "no credential available", which is the
/// normal case for a passthrough upstream on an unauthenticated discovery call.
async fn upstream_headers(
    state: &AppState,
    name: &str,
    provider: &ProviderConfig,
    inbound: &HeaderMap,
) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

    match provider.auth {
        // Forward the caller's own credential, exactly as the Messages path
        // does. This is what keeps the answer scoped to the caller.
        AuthMode::Passthrough => {
            let bearer = inbound.get("authorization").cloned();
            let api_key = inbound.get("x-api-key").cloned();
            if bearer.is_none() && api_key.is_none() {
                return None;
            }
            // An `sk-ant-oat…` bearer authenticates only as a bearer; the copy
            // Claude Code's apiKeyHelper puts in x-api-key would be rejected.
            let oauth_bearer = bearer
                .as_ref()
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().split_once(' '))
                .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token))
                .is_some_and(|token| token.trim().starts_with("sk-ant-oat"));
            if let Some(value) = bearer {
                headers.insert("authorization", value);
            }
            if let Some(value) = api_key.filter(|_| !oauth_bearer) {
                headers.insert("x-api-key", value);
            }
        }
        AuthMode::ApiKey => {
            let route = probe_route(name);
            let credential = resolve_credential(&state.config, &route, &state.http_client)
                .await
                .ok()?;
            let Credential::ApiKey { value, header } = credential else {
                return None;
            };
            match header {
                ApiKeyHeader::Bearer => {
                    headers.insert("authorization", bearer_value(&value)?);
                }
                ApiKeyHeader::XApiKey => {
                    headers.insert("x-api-key", HeaderValue::from_str(&value).ok()?);
                }
            }
        }
        // Any configured account answers the catalog question, so take the first
        // that resolves. Discovery is not an inference turn, so this walks the
        // configured accounts directly rather than the pool: no selection,
        // cooldown, or quota bookkeeping is disturbed.
        AuthMode::ClaudeOauth => {
            let mut token = None;
            for account in &provider.accounts {
                if let Ok(Credential::ClaudeOauth { access_token, .. }) =
                    resolve_claude_account(account, &state.http_client).await
                {
                    token = Some(access_token);
                    break;
                }
            }
            headers.insert("authorization", bearer_value(&token?)?);
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("oauth-2025-04-20"),
            );
        }
        // No other mode authenticates against an Anthropic models endpoint.
        _ => return None,
    }

    Some(headers)
}

fn bearer_value(token: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// `resolve_credential` is route-shaped but only reads `route.provider`, so a
/// discovery probe supplies a minimal one.
fn probe_route(provider: &str) -> Route {
    Route {
        provider: provider.to_string(),
        adapter: AdapterKind::Anthropic,
        model: String::new(),
        upstream_model: String::new(),
        effort: None,
    }
}

#[cfg(test)]
mod tests;
