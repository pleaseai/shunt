use std::{path::PathBuf, time::Duration};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};

use crate::{
    accounts::{self, FailoverAction},
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{
        self, claude::auth::ClaudeAuthStore, resolve_claude_account, resolve_credential, Credential,
    },
    config::{ApiKeyHeader, AuthMode},
    error::UpstreamError,
    headers, keepalive,
    routing::Route,
    server::AppState,
};

pub struct AnthropicAdapter;

impl Adapter for AnthropicAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .expect("route provider was validated");
    if provider.auth == AuthMode::ClaudeOauth {
        return forward_claude_oauth(state, route, uri, headers, body).await;
    }

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let request_headers = outbound_headers(headers, &credential);
    let oauth_client = bearer_is_subscription_oauth(&request_headers);
    let body = normalize_upstream_model(body, &route.upstream_model);
    // Bounded transient retry (issue #48) for this single-credential path. Kept
    // off `count_tokens`, which passes through here for Anthropic-kind providers
    // — a token count is cheap for the client to re-issue and never worth a
    // gateway-side backoff.
    let policy = if crate::proxy::is_count_tokens(uri) {
        crate::retry::RetryPolicy::DISABLED
    } else {
        provider.retry.policy()
    };
    let url = upstream_url(&state, &route, uri);
    // `Bytes` clones the body as a cheap refcount bump, so an idempotent re-send
    // on retry never re-copies it.
    let body = bytes::Bytes::from(body);
    let client = state.http_client.clone();
    let upstream = crate::retry::send_with_retry(policy, &route.provider, || {
        client
            .post(url.as_str())
            .headers(request_headers.clone())
            .body(body.clone())
            .send()
    })
    .await
    .map_err(upstream_error)?;
    let status = upstream.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        tracing::warn!(
            provider = %route.provider,
            model = %route.model,
            upstream_model = %route.upstream_model,
            rate_limit_kind = rate_limit_kind(upstream.headers(), oauth_client),
            "upstream returned 429"
        );
    }
    // The non-pooled path builds its response exactly like the pooled path's
    // relay_response (header filtering, SSE keepalive, status passthrough), so
    // reuse it with no account attribution instead of duplicating that logic.
    relay_response(&state, upstream, None)
}

async fn forward_claude_oauth(
    state: AppState,
    route: Route,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .expect("route provider was validated");
    let accounts = auth::shared::resolve_pool_accounts(
        "Claude",
        &provider.accounts,
        auth::claude::store::default_accounts_dir(),
        auth::claude::store::scan_accounts,
    )
    .await
    .map_err(auth::auth_error)?;
    if accounts.is_empty() {
        return Err(auth::auth_error(format!(
            "provider '{}' uses claude_oauth but has no accounts; run `shunt login claude --name <name>` or configure [[providers.{}.accounts]]",
            route.provider, route.provider
        )));
    }

    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok());
    let order = state.accounts.select_order(
        &route.provider,
        &accounts,
        session_id,
        Some(route.upstream_model.as_str()),
    );
    let url = upstream_url(&state, &route, uri);
    let base_body = normalize_upstream_model(body, &route.upstream_model);
    let mut last_response = None;

    for index in order {
        let account = &accounts[index];
        // The per-account refresh_lock serializes only credential refreshes for
        // one account: resolve_claude_account can refresh-on-read and write the
        // token back, and the explicit force_refresh in the RefreshRetry branch
        // below. Hold it only around those two points — never across the upstream
        // POSTs or the PauseSame back-off sleep — so concurrent same-account
        // requests are not serialized behind an unrelated 429 retry-after wait.
        let refresh_lock = state.accounts.refresh_lock(&route.provider, &account.name);

        let credential = {
            let _guard = refresh_lock.lock().await;
            match resolve_claude_account(account, &state.http_client).await {
                Ok(credential) => credential,
                Err(error) => {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "failed to resolve Claude OAuth account"
                    );
                    continue;
                }
            }
        };
        let account_uuid = match &credential {
            Credential::ClaudeOauth { account_uuid, .. } => account_uuid.as_deref(),
            _ => None,
        };
        let request_body = rewrite_account_uuid(base_body.clone(), account_uuid);
        let request_headers = outbound_headers(headers, &credential);

        let upstream = match post_upstream(
            &state.http_client,
            &url,
            request_headers.clone(),
            request_body.clone(),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                state
                    .accounts
                    .cooldown(&route.provider, &account.name, Duration::from_secs(30));
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    error = %error,
                    "Claude OAuth upstream request failed"
                );
                continue;
            }
        };

        state
            .accounts
            .note_quota(&route.provider, &account.name, upstream.headers());
        let status = upstream.status();
        match accounts::classify(status, upstream.headers()) {
            FailoverAction::Relay => {
                state.accounts.mark_healthy(&route.provider, &account.name);
                return relay_response(&state, upstream, Some(&account.name));
            }
            FailoverAction::Rotate => {
                let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
                    accounts::retry_after(upstream.headers())
                        .unwrap_or(Duration::from_secs(60))
                        .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                } else {
                    Duration::from_secs(30)
                };
                state
                    .accounts
                    .cooldown(&route.provider, &account.name, cooldown);
                // Log on the way out like every other failover arm in this loop
                // (resolve/post/refresh errors all warn) — this is the most common
                // failover trigger (quota-rejected 429 or any 5xx), so an operator
                // watching logs during an incident sees why traffic shifted.
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    status = %status,
                    "Claude OAuth account failed over; cooling down and rotating to the next account"
                );
                last_response = Some(upstream);
            }
            FailoverAction::PauseSame => {
                let delay = accounts::retry_after(upstream.headers())
                    .unwrap_or(Duration::from_secs(1))
                    .min(Duration::from_secs(300));
                tokio::time::sleep(delay).await;
                let Some(retry) = retry_upstream(
                    &state,
                    &route,
                    account,
                    &url,
                    request_headers,
                    request_body,
                    "Claude OAuth throttle retry failed",
                )
                .await
                else {
                    last_response = Some(upstream);
                    continue;
                };
                let retry_status = retry.status();
                if retry_status.is_success() {
                    state.accounts.mark_healthy(&route.provider, &account.name);
                } else {
                    let cooldown = accounts::retry_after(retry.headers())
                        .unwrap_or(delay)
                        .clamp(Duration::from_secs(1), Duration::from_secs(300));
                    state
                        .accounts
                        .cooldown(&route.provider, &account.name, cooldown);
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        status = %retry_status,
                        "Claude OAuth throttle retry did not succeed; cooling down account"
                    );
                }
                return relay_response(&state, retry, Some(&account.name));
            }
            FailoverAction::RefreshRetry => {
                // account_is_static_store_token() reads the account file from
                // disk; run it on the blocking pool. A join failure defaults to
                // false (treat as refreshable), which is the safe fallback.
                let is_static = {
                    let account = account.clone();
                    tokio::task::spawn_blocking(move || account_is_static_store_token(&account))
                        .await
                        .unwrap_or(false)
                };
                if account.token_env.is_some() || is_static {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    // A static credential (token_env or a long-lived setup token)
                    // cannot be refreshed, so a 401 here means it is expired or
                    // revoked. Log it — otherwise the account cycles in and out of
                    // this cooldown indefinitely with no operator-visible signal.
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "Claude OAuth account returned 401 but its credential is not refreshable (token_env or long-lived setup token); cooling down"
                    );
                    last_response = Some(upstream);
                    continue;
                }

                let failed_access_token = match &credential {
                    Credential::ClaudeOauth { access_token, .. } => access_token.as_str(),
                    // resolve_claude_account only ever yields ClaudeOauth, so this
                    // is unreachable today — but this is a request-handling path in
                    // a failover proxy, so degrade gracefully (log loudly + fail
                    // over to the next account) instead of panicking if a future
                    // refactor ever breaks that invariant.
                    _ => {
                        tracing::error!(
                            provider = %route.provider,
                            account = %account.name,
                            "claude_oauth account resolved a non-OAuth credential"
                        );
                        last_response = Some(upstream);
                        continue;
                    }
                };
                let credentials = account
                    .credentials
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| auth::claude::store::account_path(&account.name));
                let store = ClaudeAuthStore::new(credentials, state.http_client.clone());
                // Serialize the refresh + credential writeback for this account
                // (see the refresh_lock note at the top of the loop); release the
                // lock again before the retry POST below.
                let access_token = {
                    let _guard = refresh_lock.lock().await;
                    match store
                        .force_refresh_if_access_token(failed_access_token)
                        .await
                    {
                        Ok(token) => token,
                        Err(error) => {
                            state.accounts.cooldown(
                                &route.provider,
                                &account.name,
                                Duration::from_secs(5 * 60),
                            );
                            tracing::warn!(
                                provider = %route.provider,
                                account = %account.name,
                                error = %error,
                                "failed to force-refresh Claude OAuth account"
                            );
                            last_response = Some(upstream);
                            continue;
                        }
                    }
                };
                let refreshed = Credential::ClaudeOauth {
                    access_token,
                    account_uuid: account.uuid.clone(),
                };
                let retry_headers = outbound_headers(headers, &refreshed);
                let Some(retry) = retry_upstream(
                    &state,
                    &route,
                    account,
                    &url,
                    retry_headers,
                    request_body,
                    "Claude OAuth refresh retry failed",
                )
                .await
                else {
                    last_response = Some(upstream);
                    continue;
                };
                let retry_status = retry.status();
                if retry_status == StatusCode::UNAUTHORIZED {
                    // Refresh succeeded but the credential is still rejected — the
                    // account is genuinely broken. Cool it down longer and rotate
                    // rather than relaying the 401 to the client.
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    last_response = Some(retry);
                    continue;
                }
                // Classify the refreshed retry the same way the initial response is
                // classified, so a non-success outcome fails over to the remaining
                // accounts instead of short-circuiting the pool. A non-429 4xx maps
                // to Relay (a client error, not the account's fault) and goes
                // straight back without a wrongful cooldown.
                match accounts::classify(retry_status, retry.headers()) {
                    FailoverAction::Relay => {
                        if retry_status.is_success() {
                            state.accounts.mark_healthy(&route.provider, &account.name);
                        }
                        return relay_response(&state, retry, Some(&account.name));
                    }
                    // Exhaustive rather than `_` so a new FailoverAction variant
                    // forces a decision here. RefreshRetry cannot recur (a 401 is
                    // special-cased just above), but listing it keeps this
                    // compiler-checked without a panic-on-invariant-break arm.
                    FailoverAction::Rotate
                    | FailoverAction::PauseSame
                    | FailoverAction::RefreshRetry => {
                        let cooldown = if retry_status == StatusCode::TOO_MANY_REQUESTS {
                            accounts::retry_after(retry.headers())
                                .unwrap_or(Duration::from_secs(60))
                                .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                        } else {
                            Duration::from_secs(30)
                        };
                        state
                            .accounts
                            .cooldown(&route.provider, &account.name, cooldown);
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            status = %retry_status,
                            "Claude OAuth refresh retry did not succeed; rotating to the next account"
                        );
                        last_response = Some(retry);
                        continue;
                    }
                }
            }
        }
    }

    if let Some(response) = last_response {
        return relay_response(&state, response, None);
    }

    Err(AdapterError {
        message: "all Claude OAuth accounts failed before receiving an upstream response"
            .to_string(),
        response: Box::new(
            UpstreamError::from_message(
                "all Claude OAuth accounts failed before receiving an upstream response",
            )
            .into_response(),
        ),
    })
}

fn account_is_static_store_token(account: &crate::config::AccountConfig) -> bool {
    if account.credentials.is_some() || account.token_env.is_some() {
        return false;
    }
    let path = auth::claude::store::account_path(&account.name);
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .pointer("/claudeAiOauth/shuntCredentialKind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some(auth::claude::store::SETUP_TOKEN_KIND)
}

async fn post_upstream(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<reqwest::Response, reqwest::Error> {
    client.post(url).headers(headers).body(body).send().await
}

/// Send a per-account retry POST, noting quota headers on success. On a
/// transport error it cools the account down for 30s, logs `fail_msg`, and
/// returns `None` so the caller fails over to the next account. Shared by the
/// throttle-retry and refresh-retry arms, whose transport-error handling is
/// otherwise identical.
async fn retry_upstream(
    state: &AppState,
    route: &Route,
    account: &crate::config::AccountConfig,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    fail_msg: &str,
) -> Option<reqwest::Response> {
    match post_upstream(&state.http_client, url, headers, body).await {
        Ok(response) => {
            state
                .accounts
                .note_quota(&route.provider, &account.name, response.headers());
            Some(response)
        }
        Err(error) => {
            state
                .accounts
                .cooldown(&route.provider, &account.name, Duration::from_secs(30));
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                error = %error,
                "{}",
                fail_msg
            );
            None
        }
    }
}

fn relay_response(
    state: &AppState,
    upstream: reqwest::Response,
    account_name: Option<&str>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let status = upstream.status();
    let response_headers = headers::filtered(upstream.headers());
    let is_sse = upstream
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("text/event-stream"))
        .unwrap_or(false);
    let stream = upstream.bytes_stream();

    let mut builder = Response::builder().status(status);
    for (name, value) in response_headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }
    if let Some(account_name) = account_name {
        if let Ok(value) = HeaderValue::from_str(account_name) {
            builder = builder.header("x-shunt-account", value);
        }
    }

    // Keepalive pings apply only to SSE relays; JSON bodies pass untouched.
    let body = if is_sse {
        Body::from_stream(keepalive::with_pings(
            stream,
            Duration::from_secs(state.config.server.sse_keepalive_seconds),
        ))
    } else {
        Body::from_stream(stream)
    };
    let response = builder
        .body(body)
        .expect("response builder uses valid upstream status and headers")
        .into_response();
    Ok((status, response))
}

/// Classify an upstream 429 for the request log. A genuine quota rate limit
/// carries `retry-after` and/or `anthropic-ratelimit-*` response headers.
/// api.anthropic.com additionally rejects a subscription-OAuth request that
/// does not look like Claude Code as a bare `rate_limit_error` carrying none
/// of those headers — but that gate only exists for OAuth bearers, so a
/// headerless 429 on any other credential (an api-key Anthropic-compatible
/// provider such as Kimi or DeepSeek, or key-based passthrough) is labeled
/// `no-ratelimit-headers` instead of being blamed on client shape. Triage
/// guidance lives in the site troubleshooting page.
fn rate_limit_kind(headers: &HeaderMap, oauth_client: bool) -> &'static str {
    let has_quota_signal = headers.contains_key("retry-after")
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("anthropic-ratelimit-"));
    if has_quota_signal {
        "quota"
    } else if oauth_client {
        "client-shape-rejection"
    } else {
        "no-ratelimit-headers"
    }
}

/// Rewrite the outbound request body's `model` to the routed `upstream_model`
/// when they differ. The passthrough adapter forwards the client body verbatim,
/// so without this two things leak to the provider: a `[1m]` context-window hint
/// (which `routing::strip_context_window_hint` removes from the routing key but
/// not from the body — and api.anthropic.com does not recognize a `[1m]`-suffixed
/// model id), and an explicit `[[routes]]` `upstream_model` remap (otherwise
/// ignored for an Anthropic-provider route). The common case — body model already
/// equal to `upstream_model` — re-serializes nothing and forwards the original
/// bytes untouched, preserving byte-for-byte passthrough.
fn normalize_upstream_model(body: Vec<u8>, upstream_model: &str) -> Vec<u8> {
    #[derive(serde::Deserialize)]
    struct ModelView {
        model: String,
    }

    // Cheap guard: peek only the `model` field. A body that isn't JSON, has no
    // `model`, or whose model already matches is forwarded unchanged.
    match serde_json::from_slice::<ModelView>(&body) {
        Ok(view) if view.model != upstream_model => {
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
                return body;
            };
            let Some(object) = value.as_object_mut() else {
                return body;
            };
            object.insert(
                "model".to_string(),
                serde_json::Value::String(upstream_model.to_string()),
            );
            serde_json::to_vec(&value).unwrap_or(body)
        }
        _ => body,
    }
}

fn rewrite_account_uuid(body: Vec<u8>, account_uuid: Option<&str>) -> Vec<u8> {
    let Some(account_uuid) = account_uuid else {
        return body;
    };
    let Ok(mut outer) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let Some(user_id) = outer
        .get_mut("metadata")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|metadata| metadata.get_mut("user_id"))
    else {
        return body;
    };
    let Some(user_id_string) = user_id.as_str() else {
        return body;
    };
    let Ok(mut inner) = serde_json::from_str::<serde_json::Value>(user_id_string) else {
        return body;
    };
    let Some(inner_object) = inner.as_object_mut() else {
        return body;
    };
    let Some(inner_account_uuid) = inner_object.get_mut("account_uuid") else {
        return body;
    };
    *inner_account_uuid = serde_json::Value::String(account_uuid.to_string());
    let Ok(serialized_inner) = serde_json::to_string(&inner) else {
        return body;
    };
    *user_id = serde_json::Value::String(serialized_inner);
    serde_json::to_vec(&outer).unwrap_or(body)
}

/// Build the headers sent upstream. For a passthrough provider (api.anthropic.com)
/// the client's own credential is forwarded unchanged. For an api-key provider
/// (Kimi, DeepSeek, Z.ai, OpenRouter, Vercel, …) the client's auth headers are
/// stripped and replaced with the provider's key in its configured header.
fn outbound_headers(headers: &HeaderMap, credential: &Credential) -> HeaderMap {
    let mut out = headers::filtered(headers);
    match credential {
        Credential::ApiKey { value, header } => {
            out.remove("authorization");
            out.remove("x-api-key");
            match header {
                ApiKeyHeader::Bearer => {
                    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {value}")) {
                        out.insert("authorization", value);
                    }
                }
                ApiKeyHeader::XApiKey => {
                    if let Ok(value) = HeaderValue::from_str(value) {
                        out.insert("x-api-key", value);
                    }
                }
            }
        }
        Credential::ClaudeOauth { access_token, .. } => {
            out.remove("authorization");
            out.remove("x-api-key");
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {access_token}")) {
                out.insert("authorization", value);
            }

            const OAUTH_BETA: &str = "oauth-2025-04-20";
            let beta = out
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let has_oauth_beta = beta.split(',').any(|token| token.trim() == OAUTH_BETA);
            if !has_oauth_beta {
                let value = if beta.is_empty() {
                    OAUTH_BETA.to_string()
                } else {
                    format!("{beta},{OAUTH_BETA}")
                };
                if let Ok(value) = HeaderValue::from_str(&value) {
                    out.insert("anthropic-beta", value);
                }
            }
        }
        // Passthrough forwards the client's own credential unchanged — with one
        // fix-up. Claude Code's `apiKeyHelper` is an API-key mechanism: it sends
        // its output in *both* `x-api-key` and `Authorization: Bearer`. When that
        // output is a Claude *subscription OAuth* token (`sk-ant-oat…`, e.g. from
        // `shunt token`), the copy in `x-api-key` makes api.anthropic.com reject
        // the request — an OAuth token authenticates only as a bearer. Drop the
        // duplicated `x-api-key` so the bearer stands alone. A real API key in
        // `x-api-key` (the `ANTHROPIC_API_KEY` path, which sends no bearer) is
        // left untouched.
        Credential::Passthrough => strip_duplicate_oauth_api_key(&mut out),
        _ => {}
    }
    out
}

/// api.anthropic.com authenticates a subscription OAuth token only via the
/// `Authorization: Bearer` header; the same token echoed in `x-api-key` is
/// rejected as an invalid API key. When the forwarded bearer is an OAuth token
/// (`sk-ant-oat…`), remove any `x-api-key` so a client that sends both — Claude
/// Code's `apiKeyHelper` — still authenticates on passthrough.
fn strip_duplicate_oauth_api_key(headers: &mut HeaderMap) {
    if bearer_is_subscription_oauth(headers) {
        headers.remove("x-api-key");
    }
}

/// True when the outbound `Authorization` header carries a Claude subscription
/// OAuth token (`sk-ant-oat…`). The `Bearer` scheme is case-insensitive
/// (RFC 6750): match it without regard to case, and tolerate surrounding
/// whitespace, so an OAuth token is recognized regardless of how the client
/// spells the scheme.
fn bearer_is_subscription_oauth(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().split_once(' '))
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token))
        .map(|token| token.trim().starts_with("sk-ant-oat"))
        .unwrap_or(false)
}

fn upstream_url(state: &AppState, route: &Route, uri: &Uri) -> String {
    let base = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.base_url.as_str())
        .unwrap_or("https://api.anthropic.com")
        .trim_end_matches('/');
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(uri.path());
    format!("{base}{path_and_query}")
}

fn upstream_error(error: reqwest::Error) -> AdapterError {
    let message = error.to_string();
    AdapterError {
        message,
        response: Box::new(UpstreamError::from_reqwest(error).into_response()),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use crate::config::ApiKeyHeader;

    use super::{
        normalize_upstream_model, outbound_headers, rate_limit_kind, rewrite_account_uuid,
        Credential,
    };

    fn client_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer client-token".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers
    }

    // Build an `Authorization` value from parts so no contiguous
    // `Bearer <token>` string literal appears in the test fixtures — secret
    // scanners (e.g. Sonar S8217) flag such literals as hardcoded credentials,
    // and these are throwaway fakes.
    fn auth(scheme: &str, token: &str) -> String {
        format!("{scheme} {token}")
    }

    fn claude_route() -> super::Route {
        super::Route {
            provider: "claude".to_string(),
            adapter: crate::routing::AdapterKind::Anthropic,
            model: "claude-test".to_string(),
            upstream_model: "claude-test".to_string(),
            effort: None,
        }
    }

    fn claude_account() -> crate::config::AccountConfig {
        crate::config::AccountConfig {
            name: "acct".to_string(),
            credentials: None,
            token_env: None,
            uuid: None,
        }
    }

    #[tokio::test]
    async fn retry_upstream_returns_response_on_success() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let state =
            super::AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();

        let response = super::retry_upstream(
            &state,
            &claude_route(),
            &claude_account(),
            &server.uri(),
            HeaderMap::new(),
            Vec::new(),
            "retry failed",
        )
        .await
        .expect("a 200 upstream should be handed back to the caller");
        assert!(response.status().is_success());
    }

    #[tokio::test]
    async fn retry_upstream_signals_failover_on_transport_error() {
        let state =
            super::AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();

        // Port 1 refuses immediately, so post_upstream returns a transport error
        // and the helper must cool the account down and return None (fail over).
        let outcome = super::retry_upstream(
            &state,
            &claude_route(),
            &claude_account(),
            "http://127.0.0.1:1/v1/messages",
            HeaderMap::new(),
            Vec::new(),
            "retry failed",
        )
        .await;
        assert!(
            outcome.is_none(),
            "a transport error should signal fail-over"
        );
    }

    #[test]
    fn passthrough_forwards_client_credential_unchanged() {
        let out = outbound_headers(&client_headers(), &Credential::Passthrough);
        assert_eq!(out.get("authorization").unwrap(), "Bearer client-token");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn passthrough_drops_duplicate_x_api_key_for_oauth_bearer() {
        // Claude Code's `apiKeyHelper` sends its OAuth token in BOTH headers;
        // the copy in `x-api-key` would make api.anthropic.com reject the token.
        let oauth = auth("Bearer", "sk-ant-oat01-abc");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", oauth.parse().unwrap());
        headers.insert("x-api-key", "sk-ant-oat01-abc".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());

        let out = outbound_headers(&headers, &Credential::Passthrough);
        // Bearer OAuth token survives; the poisoned x-api-key is removed.
        assert_eq!(out.get("authorization").unwrap(), oauth.as_str());
        assert!(out.get("x-api-key").is_none());
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn passthrough_keeps_real_api_key_in_x_api_key() {
        // The `ANTHROPIC_API_KEY` path sends a real key in x-api-key and no
        // bearer — it must be forwarded untouched.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "sk-ant-api03-realkey".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());

        let out = outbound_headers(&headers, &Credential::Passthrough);
        assert_eq!(out.get("x-api-key").unwrap(), "sk-ant-api03-realkey");
        assert!(out.get("authorization").is_none());
    }

    #[test]
    fn passthrough_keeps_x_api_key_when_bearer_is_not_oauth() {
        // A non-OAuth bearer (e.g. a real API key returned by apiKeyHelper, which
        // Anthropic reads from x-api-key) leaves x-api-key in place.
        let api_bearer = auth("Bearer", "sk-ant-api03-key");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", api_bearer.parse().unwrap());
        headers.insert("x-api-key", "sk-ant-api03-key".parse().unwrap());

        let out = outbound_headers(&headers, &Credential::Passthrough);
        assert_eq!(out.get("x-api-key").unwrap(), "sk-ant-api03-key");
        assert_eq!(out.get("authorization").unwrap(), api_bearer.as_str());
    }

    #[test]
    fn passthrough_drops_duplicate_x_api_key_for_lowercase_bearer_oauth() {
        // The scheme is matched case-insensitively (`Bearer ` / `bearer `); a
        // lowercase-prefixed OAuth token must still get its duplicate stripped.
        let oauth = auth("bearer", "sk-ant-oat01-abc");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", oauth.parse().unwrap());
        headers.insert("x-api-key", "sk-ant-oat01-abc".parse().unwrap());

        let out = outbound_headers(&headers, &Credential::Passthrough);
        assert_eq!(out.get("authorization").unwrap(), oauth.as_str());
        assert!(out.get("x-api-key").is_none());
    }

    #[test]
    fn passthrough_drops_duplicate_x_api_key_for_uppercase_bearer_oauth() {
        // The `Bearer` scheme is case-insensitive (RFC 6750/7235); an
        // upper-cased scheme must still strip the duplicate.
        let oauth = auth("BEARER", "sk-ant-oat01-abc");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", oauth.parse().unwrap());
        headers.insert("x-api-key", "sk-ant-oat01-abc".parse().unwrap());

        let out = outbound_headers(&headers, &Credential::Passthrough);
        assert_eq!(out.get("authorization").unwrap(), oauth.as_str());
        assert!(out.get("x-api-key").is_none());
    }

    #[test]
    fn api_key_bearer_replaces_client_credential() {
        let out = outbound_headers(
            &client_headers(),
            &Credential::ApiKey {
                value: "provider-key".to_string(),
                header: ApiKeyHeader::Bearer,
            },
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer provider-key");
        assert!(out.get("x-api-key").is_none());
        // Non-auth client headers still pass through.
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn api_key_x_api_key_replaces_client_credential() {
        let out = outbound_headers(
            &client_headers(),
            &Credential::ApiKey {
                value: "provider-key".to_string(),
                header: ApiKeyHeader::XApiKey,
            },
        );
        assert_eq!(out.get("x-api-key").unwrap(), "provider-key");
        assert!(out.get("authorization").is_none());
    }

    #[test]
    fn claude_oauth_sets_bearer_strips_client_auth_and_adds_beta() {
        let mut headers = client_headers();
        headers.insert("x-api-key", "client-key".parse().unwrap());
        let out = outbound_headers(
            &headers,
            &Credential::ClaudeOauth {
                access_token: "oauth-token".to_string(),
                account_uuid: None,
            },
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer oauth-token");
        assert!(out.get("x-api-key").is_none());
        assert_eq!(out.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
    }

    #[test]
    fn claude_oauth_appends_beta_without_duplication() {
        let credential = Credential::ClaudeOauth {
            access_token: "oauth-token".to_string(),
            account_uuid: None,
        };
        let mut headers = client_headers();
        headers.insert("anthropic-beta", "feature-a".parse().unwrap());
        let appended = outbound_headers(&headers, &credential);
        assert_eq!(
            appended.get("anthropic-beta").unwrap(),
            "feature-a,oauth-2025-04-20"
        );

        headers.insert(
            "anthropic-beta",
            "feature-a, oauth-2025-04-20".parse().unwrap(),
        );
        let unchanged = outbound_headers(&headers, &credential);
        assert_eq!(
            unchanged.get("anthropic-beta").unwrap(),
            "feature-a, oauth-2025-04-20"
        );
    }

    #[test]
    fn rewrite_account_uuid_replaces_stringified_inner_field() {
        let inner = serde_json::json!({"account_uuid":"old","device":"cli"}).to_string();
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-sonnet-4-6",
            "metadata": {"user_id": inner}
        }))
        .unwrap();
        let out = rewrite_account_uuid(body, Some("selected"));
        let outer: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let inner: serde_json::Value =
            serde_json::from_str(outer["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner["account_uuid"], "selected");
        assert_eq!(inner["device"], "cli");
    }

    #[test]
    fn rewrite_account_uuid_leaves_unusable_bodies_untouched() {
        for (body, uuid) in [
            (br#"{"model":"claude-sonnet-4-6"}"#.to_vec(), Some("new")),
            (b"not json".to_vec(), Some("new")),
            (
                br#"{"metadata":{"user_id":"{\"account_uuid\":\"old\"}"}}"#.to_vec(),
                None,
            ),
        ] {
            let original = body.clone();
            assert_eq!(rewrite_account_uuid(body, uuid), original);
        }
    }

    #[test]
    fn rate_limit_with_retry_after_is_quota() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(rate_limit_kind(&headers, true), "quota");
        assert_eq!(rate_limit_kind(&headers, false), "quota");
    }

    #[test]
    fn rate_limit_with_anthropic_ratelimit_headers_is_quota() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            "allowed_warning".parse().unwrap(),
        );
        assert_eq!(rate_limit_kind(&headers, true), "quota");
    }

    #[test]
    fn headerless_rate_limit_on_oauth_is_client_shape_rejection() {
        // The OAuth "must look like Claude Code" gate returns a bare 429 with
        // neither retry-after nor any anthropic-ratelimit-* header.
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("request-id", "req_123".parse().unwrap());
        assert_eq!(rate_limit_kind(&headers, true), "client-shape-rejection");
    }

    #[test]
    fn headerless_rate_limit_on_non_oauth_is_not_blamed_on_client_shape() {
        // The gate only exists for subscription OAuth bearers; an api-key
        // Anthropic-compatible provider (Kimi, DeepSeek, …) answering 429
        // without rate-limit headers is a real rate limit, not a shape issue.
        let headers = HeaderMap::new();
        assert_eq!(rate_limit_kind(&headers, false), "no-ratelimit-headers");
    }

    #[test]
    fn normalize_rewrites_model_when_upstream_differs() {
        // A `[1m]` context-window hint must not reach the provider verbatim.
        let body = br#"{"model":"claude-sonnet-4-6[1m]","max_tokens":1}"#.to_vec();
        let out = normalize_upstream_model(body, "claude-sonnet-4-6");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "claude-sonnet-4-6");
        // The rest of the body survives the rewrite.
        assert_eq!(value["max_tokens"], 1);
    }

    #[test]
    fn normalize_leaves_body_untouched_when_model_matches() {
        // Common case: byte-for-byte passthrough, no re-serialization.
        let body = br#"{"model":"claude-sonnet-4-6","max_tokens":1}"#.to_vec();
        let original = body.clone();
        let out = normalize_upstream_model(body, "claude-sonnet-4-6");
        assert_eq!(out, original);
    }

    #[test]
    fn normalize_leaves_non_json_body_untouched() {
        let body = b"not json".to_vec();
        let original = body.clone();
        let out = normalize_upstream_model(body, "claude-sonnet-4-6");
        assert_eq!(out, original);
    }
}
