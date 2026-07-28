//! Live `GET /v1/models` passthrough for the Anthropic upstream.
//!
//! Discovery answers from local config plus a builtin snapshot, but the real
//! catalog is **credential-scoped**: the same endpoint returns a different list
//! for an `x-api-key` caller, a Claude subscription OAuth bearer, and the
//! reference apps gateway. A shared cache would therefore serve one caller's
//! entitlement view to another, so this module deliberately holds **no state**
//! — every request asks upstream with that request's own upstream credential and
//! gets that caller's own answer. Credentials consumed by shunt's inbound auth
//! gate are never relayed to the upstream.
//!
//! Failure paths return `None` and emit an operator-visible warning; discovery
//! then falls back to the builtin snapshot rather than degrading the response.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::{
    adapters::anthropic::strip_duplicate_oauth_api_key,
    auth::{
        self, claude,
        inbound::{bearer_token, InboundAuth},
        resolve_claude_account, resolve_credential, Credential,
    },
    config::{ApiKeyHeader, AuthMode, Config, ProviderConfig, ProviderKind},
    routing::{AdapterKind, Route},
    server::AppState,
};

use super::ModelEntry;

/// Overall budget for credential resolution and every upstream page request.
/// Discovery is documented to answer well under 3 s, so the entire refresh
/// fails over to the builtin snapshot after this deadline.
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

/// Authentication context already established by the discovery endpoint. It
/// prevents a gateway credential consumed by shunt from being relayed upstream.
#[derive(Clone, Copy, Default)]
pub(super) struct InboundCredentialContext<'a> {
    pub(super) static_auth: Option<&'a InboundAuth>,
    pub(super) gateway_bearer_authenticated: bool,
}

/// Fetch the caller's own model list from the Anthropic upstream.
///
/// Returns `None` — and the caller falls back to the builtin snapshot — when
/// there is no Anthropic-kind upstream configured, when no credential can be
/// resolved for it, when the request fails, times out, or does not parse, or
/// when pagination cannot establish that the returned catalog is complete.
pub(super) async fn fetch(
    state: &AppState,
    inbound: &HeaderMap,
    inbound_context: InboundCredentialContext<'_>,
) -> Option<Vec<ModelEntry>> {
    let provider_name = anthropic_provider(&state.config).map(|(name, _)| name.to_string());
    match tokio::time::timeout(
        FETCH_TIMEOUT,
        fetch_within_deadline(state, inbound, inbound_context),
    )
    .await
    {
        Ok(models) => models,
        Err(_) => {
            tracing::warn!(
                provider = provider_name.as_deref().unwrap_or("unknown"),
                "upstream /v1/models discovery refresh exceeded overall deadline; using builtin catalog"
            );
            None
        }
    }
}

async fn fetch_within_deadline(
    state: &AppState,
    inbound: &HeaderMap,
    inbound_context: InboundCredentialContext<'_>,
) -> Option<Vec<ModelEntry>> {
    let (name, provider) = anthropic_provider(&state.config)?;
    let headers = upstream_headers(state, name, provider, inbound, inbound_context).await?;
    let base = provider.base_url.trim_end_matches('/');

    let url = format!("{base}/v1/models");
    let mut collected: Vec<ModelEntry> = Vec::new();
    let mut after_id: Option<String> = None;
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let mut request = state
            .http_client
            .get(&url)
            .timeout(FETCH_TIMEOUT)
            .query(&[("limit", PAGE_LIMIT)]);
        if let Some(cursor) = after_id.as_deref() {
            request = request.query(&[("after_id", cursor)]);
        }
        for (key, value) in headers.iter() {
            request = request.header(key, value);
        }

        let response = match request.send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::warn!(
                    provider = %name,
                    status = %response.status(),
                    "upstream /v1/models rejected the discovery refresh; using builtin catalog"
                );
                return None;
            }
            Err(error) => {
                tracing::warn!(
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
                tracing::warn!(
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

        if !has_more {
            complete = true;
            break;
        }

        // A `has_more` with no usable cursor cannot be followed; fail over to
        // the builtin snapshot rather than presenting a partial list as complete.
        let Some(cursor) = next.filter(|cursor| !cursor.trim().is_empty()) else {
            tracing::warn!(
                provider = %name,
                models = collected.len(),
                "upstream /v1/models reported more pages without a usable last_id; using builtin catalog"
            );
            return None;
        };
        after_id = Some(cursor);
    }

    if !complete {
        tracing::warn!(
            provider = %name,
            models = collected.len(),
            "upstream /v1/models exceeded pagination backstop; using builtin catalog"
        );
        return None;
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

/// The Anthropic-kind upstream to ask: `server.default_provider`, and only it.
///
/// Discovery may only advertise ids that inference can actually serve. An id
/// matching no `[[routes]]`/`[[route_prefixes]]` entry falls back to
/// `server.default_provider`, so asking some *other* Anthropic-kind upstream
/// would advertise a catalog the provider those ids route to cannot serve.
/// When the default provider is not Anthropic-kind, no upstream's live catalog
/// is guaranteed routable, so discovery answers from the builtin snapshot.
///
/// Gated on `kind` rather than an `anthropic.com` host check so an
/// Anthropic-compatible gateway is asked about *its own* catalog. That is
/// strictly more honest than advertising Claude ids it cannot serve, and a
/// backend without the endpoint simply fails the fetch and falls back.
fn anthropic_provider(config: &Config) -> Option<(&str, &ProviderConfig)> {
    let default = config.server.default_provider.as_str();
    config
        .provider(default)
        .filter(|provider| provider.kind == ProviderKind::Anthropic)
        .map(|provider| (default, provider))
}

/// Build the outbound auth headers, mirroring what the Messages path sends for
/// the same `auth` mode. `None` means "no credential available", which is the
/// normal case for a passthrough upstream on an unauthenticated discovery call.
async fn upstream_headers(
    state: &AppState,
    name: &str,
    provider: &ProviderConfig,
    inbound: &HeaderMap,
    inbound_context: InboundCredentialContext<'_>,
) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

    match provider.auth {
        // Forward only credentials meant for the upstream. Discovery requires
        // gateway auth whenever configured, so first remove any value shunt's own
        // auth gate consumed; inference passthrough routes do not have this step.
        AuthMode::Passthrough => {
            // A bearer shunt already consumed — gateway login, or a
            // `[server.auth]` client token sent as `Authorization` — authenticates
            // the caller against shunt, not the caller against the upstream.
            let bearer_is_consumed = inbound_context.gateway_bearer_authenticated
                || bearer_token(inbound).is_some_and(|token| {
                    inbound_context
                        .static_auth
                        .and_then(|auth| auth.authenticate_value(token))
                        .is_some()
                });
            let bearer = inbound
                .get("authorization")
                .cloned()
                .filter(|_| !bearer_is_consumed);
            let api_key = inbound.get("x-api-key").cloned().filter(|value| {
                inbound_context
                    .static_auth
                    .and_then(|auth| auth.authenticate_value(value.as_bytes()))
                    .is_none()
            });
            if bearer.is_none() && api_key.is_none() {
                tracing::warn!(
                    provider = %name,
                    "inbound gateway credential is not forwarded to passthrough upstream; using builtin catalog"
                );
                return None;
            }
            if let Some(value) = bearer {
                headers.insert("authorization", value);
            }
            if let Some(value) = api_key {
                headers.insert("x-api-key", value);
            }
            strip_duplicate_oauth_api_key(&mut headers);
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
        // Resolve the same effective account set as inference, but walk it
        // directly: discovery performs no pool selection, cooldown, or quota
        // accounting, so the inference pool's state is not disturbed.
        AuthMode::ClaudeOauth => {
            let accounts = match auth::shared::resolve_pool_accounts(
                "Claude",
                &provider.accounts,
                &provider.account_scope,
                crate::accounts::StoreFamily::Claude,
                claude::store::default_accounts_dir(),
                claude::store::scan_accounts,
            )
            .await
            {
                Ok(accounts) => accounts,
                Err(error) => {
                    tracing::warn!(
                        provider = %name,
                        %error,
                        "upstream /v1/models failed to resolve Claude OAuth accounts; using builtin catalog"
                    );
                    return None;
                }
            };
            let mut token = None;
            for account in accounts.iter().filter(|account| !account.disabled) {
                if let Ok(Credential::ClaudeOauth { access_token, .. }) =
                    resolve_claude_account(account, &state.http_client).await
                {
                    token = Some(access_token);
                    break;
                }
            }
            let Some(token) = token else {
                tracing::warn!(
                    provider = %name,
                    accounts = accounts.iter().filter(|account| !account.disabled).count(),
                    "no enabled Claude OAuth account resolved for /v1/models discovery; using builtin catalog"
                );
                return None;
            };
            headers.insert("authorization", bearer_value(&token)?);
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
