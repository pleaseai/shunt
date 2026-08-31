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
        self, claude::auth::ClaudeAuthStore, resolve_claude_account_classified, resolve_credential,
        resolve_kimi_account, Credential,
    },
    config::{ApiKeyHeader, AuthMode},
    error::UpstreamError,
    headers, keepalive,
    request::RequestBody,
    routing::Route,
    server::AppState,
};

mod auto_mode_classifier;
mod deferral;
mod model_rewrite;

pub struct AnthropicAdapter;

impl Adapter for AnthropicAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: RequestBody,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    uri: &Uri,
    headers: &HeaderMap,
    mut body: RequestBody,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .expect("route provider was validated");
    if provider.auth == AuthMode::ClaudeOauth {
        return forward_claude_oauth(state, route, uri, headers, body).await;
    }
    if provider.auth == AuthMode::KimiOauth {
        return forward_kimi_oauth(state, route, uri, headers, body).await;
    }

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let request_headers = outbound_headers(headers, &credential);
    let oauth_client = bearer_is_subscription_oauth(&request_headers);
    // Only a subscription-OAuth bearer faces the client-shape gate; an API-key
    // Anthropic-compatible provider keeps byte-for-byte passthrough except for
    // deferred-tool fields that the upstream model cannot accept (OpenRouter
    // stealth slugs, Kimi, …).
    if oauth_client {
        auto_mode_classifier::restore_claude_code_identity(&mut body);
    }
    normalize_upstream_model_request(&mut body, &route.upstream_model);
    deferral::strip_unsupported_deferral(&mut body, &route.upstream_model);
    let body = body.into_raw();
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
    // `Bytes` clones the body as a cheap refcount bump for the safe,
    // pre-acceptance transport retry.
    let body = bytes::Bytes::from(body);
    let client = state.http_client.clone();
    let upstream = crate::retry::send_with_retry_with_safety(
        policy,
        &route.provider,
        crate::retry::RetrySafety::NonIdempotentPost,
        || {
            crate::upstream_timeout::wait(
                state.config.server.timeouts.upstream_ttfb_ms,
                client
                    .post(url.as_str())
                    .headers(request_headers.clone())
                    .body(body.clone())
                    .send(),
            )
        },
    )
    .await
    .map_err(|error| error.into_adapter_error(upstream_error))?;
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
    relay_response(&state, &route, upstream, None).await
}

async fn forward_claude_oauth(
    state: AppState,
    route: Route,
    uri: &Uri,
    headers: &HeaderMap,
    mut body: RequestBody,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .expect("route provider was validated");
    let accounts = auth::shared::resolve_pool_accounts(
        "Claude",
        &provider.accounts,
        &provider.account_scope,
        crate::accounts::StoreFamily::Claude,
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
    // Distinguish an all-`disabled` pool from a genuine upstream outage: with
    // every account disabled, select_order returns an empty order and the loop
    // below would otherwise fall through to the generic "all accounts failed"
    // error, misdirecting an operator who disabled every account by mistake.
    if accounts.iter().all(|account| account.disabled) {
        tracing::warn!(
            provider = %route.provider,
            accounts = accounts.len(),
            "all accounts for provider are disabled; none are selectable"
        );
        return Err(auth::auth_error(format!(
            "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
            route.provider,
            accounts.len()
        )));
    }

    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok());
    let is_fable = accounts::is_fable_model(Some(route.upstream_model.as_str()));
    let order = state.accounts.select_order(
        &route.provider,
        &accounts,
        session_id,
        Some(route.upstream_model.as_str()),
        state.config.server.pool.as_ref(),
    );
    let url = upstream_url(&state, &route, uri);
    normalize_upstream_model_request(&mut body, &route.upstream_model);
    deferral::strip_unsupported_deferral(&mut body, &route.upstream_model);
    let base_body = body;
    let ramp_initial = state.config.storm_ramp_initial();
    let candidates = order.len();
    let mut last_response = None;

    for (position, index) in order.into_iter().enumerate() {
        let account = &accounts[index];

        // Storm control (issue #195, `AccountPool::admit_candidate`): a
        // saturated identity rotates to the next candidate. On a relayed
        // success the guard moves into the response body (`with_admission`),
        // so the slot stays held until the stream actually finishes; on
        // rotation it drops with the iteration.
        let admission = match state.accounts.admit_candidate(
            &route.provider,
            account,
            ramp_initial,
            position,
            candidates,
        ) {
            Some(admission) => admission,
            None => continue,
        };
        // The per-account refresh_lock serializes only credential refreshes for
        // one account: resolve_claude_account can refresh-on-read and write the
        // token back, and the explicit force_refresh in the RefreshRetry branch
        // below. Hold it only around those two points — never across the upstream
        // POSTs or the PauseSame back-off sleep — so concurrent same-account
        // requests are not serialized behind an unrelated 429 retry-after wait.
        let refresh_lock = state.accounts.refresh_lock(&route.provider, account);

        let credential = {
            let _guard = refresh_lock.lock().await;
            match resolve_claude_account_classified(account, &state.http_client).await {
                Ok(credential) => credential,
                Err(failure) => {
                    state.accounts.cooldown(
                        &route.provider,
                        account,
                        Duration::from_secs(5 * 60),
                        "auth",
                    );
                    // The dominant steady state for a dead account: once its
                    // access token expires, the refresh is rejected here rather
                    // than after a 401, so the mark has to be set on this path
                    // too or the account cycles through this cooldown forever
                    // with nothing durable for an operator to see.
                    if failure.terminal {
                        state.accounts.mark_needs_relogin(&route.provider, account);
                    }
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %failure.detail,
                        terminal = failure.terminal,
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
        let mut request_body = base_body.clone();
        rewrite_account_uuid_request(&mut request_body, account_uuid);
        let request_headers = outbound_headers(headers, &credential);
        // Gate on the bearer that actually goes out rather than on the pool's
        // shape. Every account here resolves to `Credential::ClaudeOauth`, but
        // the `token_env` branch of `resolve_claude_account` wraps whatever the
        // variable holds without checking it is a subscription token — so an
        // account pointed at an `sk-ant-api…` key would otherwise have its body
        // rewritten despite facing no client-shape gate.
        if bearer_is_subscription_oauth(&request_headers) {
            auto_mode_classifier::restore_claude_code_identity(&mut request_body);
        }
        let request_body = request_body.into_raw();

        let upstream = match post_upstream(
            &state,
            &url,
            request_headers.clone(),
            request_body.clone(),
        )
        .await
        {
            Ok(response) => response,
            Err(error @ crate::upstream_timeout::SendError::Timeout) => {
                return Err(error.into_adapter_error(upstream_error));
            }
            Err(crate::upstream_timeout::SendError::Transport(error)) => {
                state.accounts.cooldown(
                    &route.provider,
                    account,
                    Duration::from_secs(30),
                    "transport",
                );
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    error = %error.without_url(),
                    "Claude OAuth upstream request failed"
                );
                continue;
            }
        };

        state
            .accounts
            .note_quota(&route.provider, account, upstream.headers());
        let status = upstream.status();
        match accounts::classify(status, upstream.headers()) {
            FailoverAction::Relay => {
                // A relayed 4xx still clears the cooldown (the account answered)
                // but only a success grows the storm-control allowance.
                state.accounts.mark_healthy_scoped(
                    &route.provider,
                    account,
                    status.is_success(),
                    is_fable,
                );
                return relay_response(&state, &route, upstream, Some(&account.name))
                    .await
                    .map(|(status, response)| {
                        (
                            status,
                            hold_admission_on_success(status, response, admission),
                        )
                    });
            }
            FailoverAction::Rotate => {
                let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
                    accounts::retry_after(upstream.headers())
                        .unwrap_or(Duration::from_secs(60))
                        .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                } else {
                    Duration::from_secs(30)
                };
                let scope = quota_cooldown_scope(status, upstream.headers(), is_fable);
                state.accounts.cooldown_scoped(
                    &route.provider,
                    account,
                    cooldown,
                    accounts::rotation_reason(status, upstream.headers()),
                    scope,
                );
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
                .await?
                else {
                    last_response = Some(upstream);
                    continue;
                };
                let retry_status = retry.status();
                if retry_status.is_success() {
                    state
                        .accounts
                        .mark_healthy_scoped(&route.provider, account, true, is_fable);
                } else {
                    let cooldown = accounts::retry_after(retry.headers())
                        .unwrap_or(delay)
                        .clamp(Duration::from_secs(1), Duration::from_secs(300));
                    let scope = quota_cooldown_scope(retry_status, retry.headers(), is_fable);
                    state.accounts.cooldown_scoped(
                        &route.provider,
                        account,
                        cooldown,
                        accounts::rotation_reason(retry_status, retry.headers()),
                        scope,
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        status = %retry_status,
                        "Claude OAuth throttle retry did not succeed; cooling down account"
                    );
                }
                return relay_response(&state, &route, retry, Some(&account.name))
                    .await
                    .map(|(status, response)| {
                        (
                            status,
                            hold_admission_on_success(status, response, admission),
                        )
                    });
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
                        account,
                        Duration::from_secs(5 * 60),
                        "auth",
                    );
                    // A static credential (token_env or a long-lived setup token)
                    // cannot be refreshed, so a 401 here means it is expired or
                    // revoked — terminal by definition, with no grant left to
                    // retry. Mark it so the operator sees a dead account on the
                    // admin dashboard; without the mark it just cycles in and out
                    // of this cooldown indefinitely, indistinguishable from a
                    // quota cooldown that will clear on its own.
                    state.accounts.mark_needs_relogin(&route.provider, account);
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
                        // A successful refresh grant proves the *refresh token*
                        // is alive, which is not the same as the account being
                        // able to serve traffic: the retry below can still come
                        // back 401 (a revoked subscription, a withdrawn scope),
                        // and that branch cools down and rotates. Clearing the
                        // mark here would unflag an account in exactly that
                        // state, so the clear is deferred to the retry's
                        // `mark_healthy_scoped` on a successful response.
                        Ok(token) => token,
                        Err(error) => {
                            state.accounts.cooldown(
                                &route.provider,
                                account,
                                Duration::from_secs(5 * 60),
                                "auth",
                            );
                            // Only a *terminal* rejection means the account is
                            // dead: the provider will never accept this refresh
                            // token again, so the 5-minute retry loop can only
                            // repeat the same rejected grant forever. A transient
                            // failure (5xx, network, timeout) must not set the
                            // mark — that would report a healthy account as dead
                            // on a momentary provider blip.
                            let terminal = auth::claude::auth::is_terminal_refresh_failure(&error);
                            if terminal {
                                state.accounts.mark_needs_relogin(&route.provider, account);
                            }
                            tracing::warn!(
                                provider = %route.provider,
                                account = %account.name,
                                error = %error,
                                terminal,
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
                .await?
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
                        account,
                        Duration::from_secs(5 * 60),
                        "auth",
                    );
                    // A live refresh grant that yields a bearer the API still
                    // rejects is not something the next attempt can fix: the
                    // account is de-authorized upstream, not momentarily unlucky.
                    // Without the mark it cycles through this cooldown forever,
                    // reported as plain `cooling` — the exact indistinguishable
                    // state this change exists to remove. The mark is deliberately
                    // set *here* rather than kept from before the refresh, because
                    // the refresh itself succeeded and the success path clears on a
                    // served response, never on the grant alone.
                    state.accounts.mark_needs_relogin(&route.provider, account);
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
                            state.accounts.mark_healthy_scoped(
                                &route.provider,
                                account,
                                true,
                                is_fable,
                            );
                        }
                        return relay_response(&state, &route, retry, Some(&account.name))
                            .await
                            .map(|(status, response)| {
                                (
                                    status,
                                    hold_admission_on_success(status, response, admission),
                                )
                            });
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
                        let scope = quota_cooldown_scope(retry_status, retry.headers(), is_fable);
                        state.accounts.cooldown_scoped(
                            &route.provider,
                            account,
                            cooldown,
                            accounts::rotation_reason(retry_status, retry.headers()),
                            scope,
                        );
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

    crate::metrics::record_pool_rotation(&route.provider, "exhausted");
    if let Some(response) = last_response {
        return relay_response(&state, &route, response, None).await;
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
        failure: Some(crate::adapters::AdapterFailure::BeforeHeaders),
    })
}

/// Kimi Code OAuth pool: resolves the provider's Kimi accounts, then rotates
/// across them on auth/quota/server/membership failures using the same
/// account pool and storm control as [`forward_claude_oauth`]. Unlike the
/// Claude path, this uses [`accounts::classify_kimi`], which additionally
/// rotates on `402 Payment Required` — a Kimi account with an inactive
/// subscription membership returns 402 on every request, so it must be
/// treated as an account-specific, persistent failure rather than relayed.
///
/// Deliberately simpler than the Claude path in two ways:
/// - No account-UUID rewrite: that mechanism (`metadata.user_id`'s embedded
///   `account_uuid`) is specific to Anthropic's first-party client protocol,
///   not part of the Kimi Code wire protocol — and `outbound_headers` never
///   injects Claude Code's `anthropic-beta: oauth-2025-04-20` for a
///   `Credential::KimiOauth`, which would be equally out of place upstream.
/// - No `PauseSame`/`RefreshRetry` same-account retry: `KimiAuthStore` only
///   exposes `get_valid()` (refresh-on-read, before the request goes out),
///   with no forced-refresh-past-a-rejected-token entry point, unlike
///   `ClaudeAuthStore::force_refresh_if_access_token` and the ChatGPT store's
///   equivalent. So every non-`Relay` classification here — quota rotate,
///   throttle pause, or a 401 that would otherwise trigger a refresh retry —
///   collapses to the same treatment: cool the account down and rotate to the
///   next candidate.
///
/// Also unlike the Claude path, cooldown/health use the unscoped
/// `cooldown`/`mark_healthy` (not `_scoped`): Kimi models are never
/// Fable-scoped, so there is no separate quota window to track.
async fn forward_kimi_oauth(
    state: AppState,
    route: Route,
    uri: &Uri,
    headers: &HeaderMap,
    mut body: RequestBody,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .expect("route provider was validated");
    let accounts = auth::shared::resolve_pool_accounts(
        "Kimi",
        &provider.accounts,
        &provider.account_scope,
        crate::accounts::StoreFamily::Kimi,
        auth::kimi::store::default_accounts_dir(),
        auth::kimi::store::scan_accounts,
    )
    .await
    .map_err(auth::auth_error)?;
    if accounts.is_empty() {
        return Err(auth::auth_error(format!(
            "provider '{}' uses kimi_oauth but has no accounts; run `shunt login kimi --name <name>` or configure [[providers.{}.accounts]]",
            route.provider, route.provider
        )));
    }
    // Distinguish an all-`disabled` pool from a genuine upstream outage: with
    // every account disabled, select_order returns an empty order and the loop
    // below would otherwise fall through to the generic "all accounts failed"
    // error, misdirecting an operator who disabled every account by mistake.
    if accounts.iter().all(|account| account.disabled) {
        tracing::warn!(
            provider = %route.provider,
            accounts = accounts.len(),
            "all accounts for provider are disabled; none are selectable"
        );
        return Err(auth::auth_error(format!(
            "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
            route.provider,
            accounts.len()
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
        state.config.server.pool.as_ref(),
    );
    let url = upstream_url(&state, &route, uri);
    normalize_upstream_model_request(&mut body, &route.upstream_model);
    deferral::strip_unsupported_deferral(&mut body, &route.upstream_model);
    let base_body = body;
    let ramp_initial = state.config.storm_ramp_initial();
    let candidates = order.len();
    let mut last_response = None;

    for (position, index) in order.into_iter().enumerate() {
        let account = &accounts[index];

        // Storm control, same as forward_claude_oauth: a saturated identity
        // rotates to the next candidate rather than piling onto it.
        let admission = match state.accounts.admit_candidate(
            &route.provider,
            account,
            ramp_initial,
            position,
            candidates,
        ) {
            Some(admission) => admission,
            None => continue,
        };
        // Per-account refresh_lock, held only around the credential resolve
        // (which may refresh-on-read and write the token back) — never across
        // the upstream POST — so concurrent same-account requests are not
        // serialized behind an unrelated request.
        let refresh_lock = state.accounts.refresh_lock(&route.provider, account);

        let credential = {
            let _guard = refresh_lock.lock().await;
            match resolve_kimi_account(account, &state.http_client).await {
                Ok(credential) => credential,
                Err(error) => {
                    state.accounts.cooldown(
                        &route.provider,
                        account,
                        Duration::from_secs(5 * 60),
                        "auth",
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "failed to resolve Kimi OAuth account"
                    );
                    continue;
                }
            }
        };
        let request_headers = outbound_headers(headers, &credential);
        let request_body = base_body.clone().into_raw();

        let upstream = match post_upstream(&state, &url, request_headers, request_body).await {
            Ok(response) => response,
            Err(error @ crate::upstream_timeout::SendError::Timeout) => {
                return Err(error.into_adapter_error(upstream_error));
            }
            Err(crate::upstream_timeout::SendError::Transport(error)) => {
                state.accounts.cooldown(
                    &route.provider,
                    account,
                    Duration::from_secs(30),
                    "transport",
                );
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    error = %error.without_url(),
                    "Kimi OAuth upstream request failed"
                );
                continue;
            }
        };

        state
            .accounts
            .note_quota(&route.provider, account, upstream.headers());
        let status = upstream.status();
        match accounts::classify_kimi(status, upstream.headers()) {
            FailoverAction::Relay => {
                // A relayed 4xx still clears the cooldown (the account answered)
                // but only a success grows the storm-control allowance.
                state
                    .accounts
                    .mark_healthy(&route.provider, account, status.is_success());
                return relay_response(&state, &route, upstream, Some(&account.name))
                    .await
                    .map(|(status, response)| {
                        (
                            status,
                            hold_admission_on_success(status, response, admission),
                        )
                    });
            }
            // Rotate, PauseSame, and RefreshRetry all collapse to the same
            // cooldown-and-rotate treatment — see the function doc comment for
            // why: `KimiAuthStore` has no same-account forced-refresh path, so
            // there is nothing a same-account retry could accomplish that
            // failing over to the next candidate does not already do at least
            // as well.
            FailoverAction::Rotate | FailoverAction::PauseSame | FailoverAction::RefreshRetry => {
                let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
                    accounts::retry_after(upstream.headers())
                        .unwrap_or(Duration::from_secs(60))
                        .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                } else {
                    Duration::from_secs(30)
                };
                state.accounts.cooldown(
                    &route.provider,
                    account,
                    cooldown,
                    accounts::rotation_reason(status, upstream.headers()),
                );
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    status = %status,
                    "Kimi OAuth account failed over; cooling down and rotating to the next account"
                );
                last_response = Some(upstream);
            }
        }
    }

    crate::metrics::record_pool_rotation(&route.provider, "exhausted");
    if let Some(response) = last_response {
        return relay_response(&state, &route, response, None).await;
    }

    Err(AdapterError {
        message: "all Kimi OAuth accounts failed before receiving an upstream response".to_string(),
        response: Box::new(
            UpstreamError::from_message(
                "all Kimi OAuth accounts failed before receiving an upstream response",
            )
            .into_response(),
        ),
        failure: Some(crate::adapters::AdapterFailure::BeforeHeaders),
    })
}

fn quota_cooldown_scope(
    status: StatusCode,
    headers: &HeaderMap,
    is_fable: bool,
) -> accounts::CooldownScope {
    if status == StatusCode::TOO_MANY_REQUESTS
        && is_fable
        && accounts::is_fable_scoped_rejection(headers)
    {
        accounts::CooldownScope::Fable
    } else {
        accounts::CooldownScope::Account
    }
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
    state: &AppState,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<reqwest::Response, crate::upstream_timeout::SendError<reqwest::Error>> {
    crate::upstream_timeout::wait(
        state.config.server.timeouts.upstream_ttfb_ms,
        state
            .http_client
            .post(url)
            .headers(headers)
            .body(body)
            .send(),
    )
    .await
}

/// Send a per-account retry POST, noting quota headers on success. On a
/// transport error it cools the account down for 30s, logs `fail_msg`, and
/// returns `None` so the caller fails over to the next account. A TTFB timeout
/// is returned directly as a 504 instead of being classified as transport
/// failover. Shared by the throttle-retry and refresh-retry arms.
async fn retry_upstream(
    state: &AppState,
    route: &Route,
    account: &crate::config::AccountConfig,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    fail_msg: &str,
) -> Result<Option<reqwest::Response>, AdapterError> {
    match post_upstream(state, url, headers, body).await {
        Ok(response) => {
            state
                .accounts
                .note_quota(&route.provider, account, response.headers());
            Ok(Some(response))
        }
        Err(error @ crate::upstream_timeout::SendError::Timeout) => {
            Err(error.into_adapter_error(upstream_error))
        }
        Err(crate::upstream_timeout::SendError::Transport(error)) => {
            state.accounts.cooldown(
                &route.provider,
                account,
                Duration::from_secs(30),
                "transport",
            );
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                error = %error.without_url(),
                "{}",
                fail_msg
            );
            Ok(None)
        }
    }
}

fn hold_admission_on_success(
    status: StatusCode,
    response: axum::response::Response,
    admission: Option<crate::accounts::AdmissionGuard>,
) -> axum::response::Response {
    if status.is_success() {
        crate::adapters::with_admission(response, admission)
    } else {
        response
    }
}

async fn relay_response(
    state: &AppState,
    route: &Route,
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

    // Restore the client-facing model alias on the return path. A discovery
    // alias route (`route.model != route.upstream_model`) sends `upstream_model`
    // outbound, and the upstream — possibly several hops away — reports its own
    // model id back in `message_start` / the JSON body. Without this the raw id
    // leaks to Claude Code and breaks `--resume` model restoration (issue #172).
    // When alias == upstream_model there is nothing to restore: `None` keeps the
    // body byte-for-byte, preserving passthrough for api.anthropic.com routes.
    let alias = (route.model != route.upstream_model).then(|| route.model.clone());

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

    let body = if is_sse {
        // Keepalive pings apply only to SSE relays; the model rewrite scans just
        // the first frame and then passes through (a no-op when `alias` is None).
        let stream = model_rewrite::rewrite_first_model_stream(upstream.bytes_stream(), alias);
        Body::from_stream(keepalive::with_pings(
            stream,
            Duration::from_secs(state.config.server.sse_keepalive_seconds),
        ))
    } else if let Some(alias) = alias {
        // Non-streaming JSON: the client asked for a buffered response, so
        // reading it whole to rewrite the top-level `model` respects the
        // "don't buffer SSE unless non-streaming" rule.
        match upstream.bytes().await {
            Ok(bytes) => Body::from(model_rewrite::rewrite_response_model(bytes, &alias)),
            Err(error) => return Err(post_header_error(error)),
        }
    } else {
        Body::from_stream(upstream.bytes_stream())
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
/// equal to `upstream_model` — mutates nothing, so [`RequestBody`] refreshes no raw
/// bytes and preserves byte-for-byte passthrough. A changed model mutates the
/// parsed request in place and refreshes the raw bytes once.
fn normalize_upstream_model_request(body: &mut RequestBody, upstream_model: &str) {
    if body.json().get("model").and_then(serde_json::Value::as_str) == Some(upstream_model) {
        return;
    }
    body.mutate(|request| {
        let Some(model) = request.get_mut("model") else {
            return false;
        };
        let Some(current_model) = model.as_str() else {
            return false;
        };
        if current_model == upstream_model {
            return false;
        }
        *model = serde_json::Value::String(upstream_model.to_string());
        true
    });
}

fn rewrite_account_uuid_request(body: &mut RequestBody, account_uuid: Option<&str>) {
    let Some(account_uuid) = account_uuid else {
        return;
    };
    // Do the whole rewrite read-only first, and enter `mutate` only to install the
    // finished string. Two costs drive that split, both paid per pool candidate:
    // `RequestBody::mutate` runs `Arc::make_mut` before the closure can report "no
    // change", and the pool loop keeps `base_body` alive while cloning it per
    // candidate — so the tree is always shared here and entering `mutate` at all
    // costs a full deep clone of the request. Bailing out before it saves that clone
    // for a body with nothing to rewrite. Preparing the value here rather than inside
    // the closure also keeps `metadata.user_id` parsed exactly once: it is
    // client-controlled and bounded only by the inbound body limit, so parsing it in
    // both a pre-check and the closure would add a full parse of it per candidate.
    let Some(rewritten_user_id) = body
        .json()
        .pointer("/metadata/user_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|user_id| serde_json::from_str::<serde_json::Value>(user_id).ok())
        .and_then(|mut inner| {
            // Only an `account_uuid` that is already present is rewritten; this never
            // introduces one.
            let existing = inner.as_object_mut()?.get_mut("account_uuid")?;
            *existing = serde_json::Value::String(account_uuid.to_string());
            serde_json::to_string(&inner).ok()
        })
    else {
        return;
    };
    body.mutate(|outer| {
        // The pre-check above resolved this same path on the same tree, so the `else`
        // is unreachable; returning `false` there would simply leave the body as the
        // client sent it.
        let Some(user_id) = outer
            .get_mut("metadata")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|metadata| metadata.get_mut("user_id"))
        else {
            return false;
        };
        *user_id = serde_json::Value::String(rewritten_user_id);
        true
    });
}

/// Test wrapper for the raw-byte contract at the adapter boundary.
#[cfg(test)]
fn normalize_upstream_model(body: Vec<u8>, upstream_model: &str) -> Vec<u8> {
    let Ok(mut request) = RequestBody::parse(body.clone()) else {
        return body;
    };
    normalize_upstream_model_request(&mut request, upstream_model);
    request.into_raw()
}

/// Test wrapper for the raw-byte contract at the adapter boundary.
#[cfg(test)]
fn rewrite_account_uuid(body: Vec<u8>, account_uuid: Option<&str>) -> Vec<u8> {
    let Ok(mut request) = RequestBody::parse(body.clone()) else {
        return body;
    };
    rewrite_account_uuid_request(&mut request, account_uuid);
    request.into_raw()
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
        Credential::KimiOauth {
            access_token,
            device_id,
        } => {
            out.remove("authorization");
            out.remove("x-api-key");
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {access_token}")) {
                out.insert("authorization", value);
            }
            // `device_id` is normally the account's stable, persisted
            // `X-Msh-Device-Id` (see `kimi::auth::msh_headers`). A
            // `token_env`-backed credential has no account file to persist one
            // in, so fall back to this process's id rather than omitting a
            // header Kimi requires on every call — a per-request id would
            // present one account to Kimi as a new device on every request.
            let device_id: &str = match device_id.as_deref() {
                Some(device_id) => device_id,
                None => crate::auth::kimi::auth::process_device_id(),
            };
            for (name, value) in crate::auth::kimi::auth::msh_headers(device_id) {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    out.insert(name, value);
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
pub(crate) fn strip_duplicate_oauth_api_key(headers: &mut HeaderMap) {
    if bearer_is_subscription_oauth(headers) {
        headers.remove("x-api-key");
    }
}

/// True when the outbound `Authorization` header carries a Claude subscription
/// OAuth token (`sk-ant-oat…`). The `Bearer` scheme is case-insensitive
/// (RFC 6750): match it without regard to case, and tolerate surrounding
/// whitespace, so an OAuth token is recognized regardless of how the client
/// spells the scheme.
pub(crate) fn bearer_is_subscription_oauth(headers: &HeaderMap) -> bool {
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

fn post_header_error(error: reqwest::Error) -> AdapterError {
    let message = error.to_string();
    AdapterError {
        message,
        response: Box::new(UpstreamError::from_reqwest(error).into_response()),
        failure: None,
    }
}

fn upstream_error(error: reqwest::Error) -> AdapterError {
    let message = error.to_string();
    AdapterError {
        message,
        response: Box::new(UpstreamError::from_reqwest(error).into_response()),
        failure: Some(crate::adapters::AdapterFailure::BeforeHeaders),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{HeaderMap, StatusCode},
    };

    use crate::{accounts::CooldownScope, config::ApiKeyHeader};

    use super::{
        hold_admission_on_success, normalize_upstream_model, outbound_headers,
        quota_cooldown_scope, rate_limit_kind, rewrite_account_uuid, Credential,
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
            service_tier: None,
        }
    }

    fn claude_account() -> crate::config::AccountConfig {
        crate::config::AccountConfig {
            name: "acct".to_string(),
            ..Default::default()
        }
    }

    fn quota_headers(values: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in values {
            headers.insert(name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn fable_only_rejection_scopes_fable_429_cooldown() {
        let headers = quota_headers(&[("anthropic-ratelimit-unified-7d_oi-status", "rejected")]);

        assert_eq!(
            quota_cooldown_scope(StatusCode::TOO_MANY_REQUESTS, &headers, true),
            CooldownScope::Fable
        );
    }

    #[test]
    fn fable_only_rejection_on_non_fable_request_is_account_wide() {
        let headers = quota_headers(&[("anthropic-ratelimit-unified-7d_oi-status", "rejected")]);

        assert_eq!(
            quota_cooldown_scope(StatusCode::TOO_MANY_REQUESTS, &headers, false),
            CooldownScope::Account
        );
    }

    #[test]
    fn shared_rejection_keeps_fable_429_cooldown_account_wide() {
        for shared_status in [
            "anthropic-ratelimit-unified-5h-status",
            "anthropic-ratelimit-unified-7d-status",
        ] {
            let headers = quota_headers(&[
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                (shared_status, "rejected"),
            ]);

            assert_eq!(
                quota_cooldown_scope(StatusCode::TOO_MANY_REQUESTS, &headers, true),
                CooldownScope::Account,
                "{shared_status} must make the cooldown account-wide"
            );
        }
    }

    #[test]
    fn fable_rejection_does_not_scope_non_429_cooldown() {
        let headers = quota_headers(&[("anthropic-ratelimit-unified-7d_oi-status", "rejected")]);

        assert_eq!(
            quota_cooldown_scope(StatusCode::INTERNAL_SERVER_ERROR, &headers, true),
            CooldownScope::Account
        );
    }

    #[test]
    fn plain_fable_429_cooldown_is_account_wide() {
        assert_eq!(
            quota_cooldown_scope(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), true),
            CooldownScope::Account
        );
    }

    #[tokio::test]
    async fn relayed_non_success_releases_admission_immediately() {
        let pool = std::sync::Arc::new(crate::accounts::AccountPool::new());
        let account = claude_account();
        let admission = pool
            .clone()
            .try_admit("claude", &account, 1, false)
            .expect("first admission");
        let response = axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap();

        let response = hold_admission_on_success(StatusCode::NOT_FOUND, response, Some(admission));

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            pool.try_admit("claude", &account, 1, false).is_some(),
            "a relayed error must release the storm-control slot before its body is consumed"
        );
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
        .expect("a 200 send should not fail")
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
        .await
        .expect("a transport failure should be handled as failover");
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
    fn kimi_oauth_sets_bearer_strips_client_auth_and_adds_msh_headers() {
        let mut headers = client_headers();
        headers.insert("x-api-key", "client-key".parse().unwrap());
        let out = outbound_headers(
            &headers,
            &Credential::KimiOauth {
                access_token: "kimi-token".to_string(),
                device_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            },
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer kimi-token");
        assert!(out.get("x-api-key").is_none());
        assert_eq!(out.get("x-msh-platform").unwrap(), "shunt");
        assert_eq!(out.get("x-msh-version").unwrap(), env!("CARGO_PKG_VERSION"));
        assert_eq!(
            out.get("x-msh-device-id").unwrap(),
            "11111111-1111-1111-1111-111111111111"
        );
        assert!(!out.get("x-msh-device-name").unwrap().is_empty());
        assert!(!out.get("x-msh-device-model").unwrap().is_empty());
    }

    #[test]
    fn kimi_oauth_without_a_stored_device_id_reuses_one_id_across_requests() {
        // A `token_env`-backed credential has no account file to persist a
        // device id in. outbound_headers must still send every X-Msh-* header
        // Kimi requires, and the fallback id must be stable across requests:
        // minting a fresh one per call would present a single account to Kimi
        // as a different device on every request.
        let credential = Credential::KimiOauth {
            access_token: "kimi-token".to_string(),
            device_id: None,
        };
        let first = outbound_headers(&client_headers(), &credential);
        let second = outbound_headers(&client_headers(), &credential);

        assert_eq!(first.get("authorization").unwrap(), "Bearer kimi-token");
        let first_id = first.get("x-msh-device-id").unwrap();
        assert!(!first_id.is_empty());
        assert_eq!(first_id, second.get("x-msh-device-id").unwrap());
        // ...and it must not silently collide with a stored account's id.
        let stored = outbound_headers(
            &client_headers(),
            &Credential::KimiOauth {
                access_token: "kimi-token".to_string(),
                device_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            },
        );
        assert_ne!(first_id, stored.get("x-msh-device-id").unwrap());
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

    /// Every shape the rewrite must decline. The bodies carry non-canonical spacing
    /// on purpose: `RequestBody::mutate` re-serializes the whole request, so if any
    /// of these wrongly entered it the output would come back canonicalized even
    /// though nothing changed.
    #[test]
    fn rewrite_account_uuid_leaves_unusable_bodies_untouched() {
        for (body, uuid) in [
            (br#"{"model":"claude-sonnet-4-6"}"#.to_vec(), Some("new")),
            (b"not json".to_vec(), Some("new")),
            (
                br#"{"metadata":{"user_id":"{\"account_uuid\":\"old\"}"}}"#.to_vec(),
                None,
            ),
            // `metadata` is not an object.
            (br#"{ "metadata": "not-an-object" }"#.to_vec(), Some("new")),
            (br#"{ "metadata": [1, 2] }"#.to_vec(), Some("new")),
            (br#"{ "metadata": null }"#.to_vec(), Some("new")),
            // `metadata.user_id` is not a string.
            (
                br#"{ "metadata": { "user_id": 42 } }"#.to_vec(),
                Some("new"),
            ),
            (
                br#"{ "metadata": { "user_id": {"account_uuid": "old"} } }"#.to_vec(),
                Some("new"),
            ),
            // `metadata.user_id` is a string but not parseable JSON.
            (
                br#"{ "metadata": { "user_id": "not json" } }"#.to_vec(),
                Some("new"),
            ),
            // The inner blob parses but is not an object.
            (
                br#"{ "metadata": { "user_id": "[1, 2]" } }"#.to_vec(),
                Some("new"),
            ),
            // The inner object has no `account_uuid` to replace — one is never added.
            (
                br#"{ "metadata": { "user_id": "{\"device\":\"cli\"}" } }"#.to_vec(),
                Some("new"),
            ),
        ] {
            let original = body.clone();
            assert_eq!(
                rewrite_account_uuid(body, uuid),
                original,
                "body was modified: {}",
                String::from_utf8_lossy(&original)
            );
        }
    }

    /// A present `account_uuid` is replaced whatever its current JSON type, so the
    /// early return must not mistake a null or numeric one for "nothing to rewrite".
    #[test]
    fn rewrite_account_uuid_replaces_a_non_string_inner_value() {
        for existing in ["null", "42", r#"{"nested":true}"#] {
            let inner = format!(r#"{{"account_uuid":{existing},"device":"cli"}}"#);
            let body = serde_json::to_vec(&serde_json::json!({
                "metadata": {"user_id": inner}
            }))
            .unwrap();

            let out = rewrite_account_uuid(body, Some("selected"));

            let outer: serde_json::Value = serde_json::from_slice(&out).unwrap();
            let inner: serde_json::Value =
                serde_json::from_str(outer["metadata"]["user_id"].as_str().unwrap()).unwrap();
            assert_eq!(inner["account_uuid"], "selected", "existing was {existing}");
            assert_eq!(inner["device"], "cli");
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
