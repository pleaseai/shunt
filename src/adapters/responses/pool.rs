//! Codex/ChatGPT OAuth account-pool failover (M10): try each account in turn,
//! websocket-first when enabled with an HTTP fallback per account, classifying
//! each raw upstream status to decide relay / rotate / refresh-and-retry.

use std::{path::PathBuf, time::Duration};

use axum::http::{HeaderValue, StatusCode};

use serde_json::Value;

use crate::{
    accounts::{self, FailoverAction},
    adapters::AdapterError,
    auth::{self, codex::auth::CodexAuthStore, resolve_chatgpt_account, Credential},
    config::{AccountConfig, AuthMode},
    routing::Route,
    server::AppState,
};

use super::error::{mapped_upstream_error, own_error};
use super::http::{http_send, json_response, stream_response};
use super::websocket::forward_websocket;

/// Drive a Responses turn over the Codex/ChatGPT OAuth account pool (M10),
/// mirroring the Anthropic adapter's `forward_claude_oauth` as closely as this
/// adapter's structure allows. Each account in `order` is tried in turn:
/// websocket first when enabled (with the pool key prefixed per-account so
/// accounts never share a pooled connection — this is the key correctness
/// requirement of the WS integration), falling back to HTTP for that same
/// account on a pre-stream websocket failure, then classifying the raw HTTP
/// status with [`accounts::classify_codex`] to decide whether to relay,
/// rotate to the next account, or force-refresh and retry the same one.
/// `note_quota` is never called here — unlike Anthropic, the ChatGPT backend
/// carries no per-account quota-rejection headers, so failover in this
/// adapter is cooldown-based only.
#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_chatgpt_oauth(
    state: AppState,
    route: Route,
    pool_key: Option<String>,
    session_id: Option<String>,
    upstream_body: Value,
    accounts_config: Vec<AccountConfig>,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let order = state.accounts.select_order(
        &route.provider,
        &accounts_config,
        session_id.as_deref(),
        // Codex carries no per-model quota signal to order by, unlike the
        // Anthropic pool's rate-limit headers.
        None,
    );
    let ws_enabled = state.config.codex_websocket_enabled(&route.provider);
    let auth = AuthMode::ChatgptOauth;
    let mut last_response: Option<reqwest::Response> = None;

    for index in order {
        let account = &accounts_config[index];
        // The per-account refresh_lock serializes only credential refreshes for
        // one account (see the matching note in
        // anthropic::forward_claude_oauth) — never held across an upstream send.
        let refresh_lock = state.accounts.refresh_lock(&route.provider, &account.name);

        let credential = {
            let _guard = refresh_lock.lock().await;
            match resolve_chatgpt_account(account, &state.http_client).await {
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
                        "failed to resolve ChatGPT OAuth account"
                    );
                    continue;
                }
            }
        };

        // Prefixing the pool key with the account name is the key point of
        // this integration: without it, two accounts serving the same client
        // session could reuse (and leak turn state across) one another's
        // pooled websocket connection.
        let account_pool_key = pool_key
            .as_deref()
            .map(|key| format!("{}::{key}", account.name));

        if ws_enabled {
            match forward_websocket(
                &state,
                &route,
                account_pool_key.as_deref(),
                upstream_body.clone(),
                credential.clone(),
                auth,
                client_wants_stream,
                thinking_enabled,
                tool_search_native,
                // Pool path does not pre-compute a message_start input estimate
                // yet (see relay_success) — follow-up to thread it through here.
                None,
            )
            .await
            {
                Ok((status, response)) => {
                    state.accounts.mark_healthy(&route.provider, &account.name);
                    return Ok((status, with_account_header(response, &account.name)));
                }
                Err(error) => {
                    // A pre-stream websocket failure (connect/handshake/send) falls
                    // back to HTTP on the SAME account, exactly like the
                    // single-account path in `forward` — only an HTTP failure
                    // triggers account-pool failover below. (A mid-stream failure
                    // is instead surfaced as an SSE error event and never reaches
                    // here — the response has already begun by then.)
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "codex websocket failed before streaming; falling back to HTTP for this account"
                    );
                }
            }
        }

        let upstream = match http_send(
            &state,
            &route,
            credential.clone(),
            session_id.as_deref(),
            bytes::Bytes::from(upstream_body.to_string()),
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
                    "ChatGPT OAuth upstream request failed"
                );
                continue;
            }
        };

        let status = upstream.status();
        match accounts::classify_codex(status, upstream.headers()) {
            FailoverAction::Relay => {
                // A non-401/429/5xx response means the account itself is fine,
                // whether or not this particular request succeeded (mirrors
                // the Anthropic adapter's top-level Relay arm).
                state.accounts.mark_healthy(&route.provider, &account.name);
                if status.is_success() {
                    let response = relay_success(
                        &state,
                        &route,
                        upstream,
                        client_wants_stream,
                        thinking_enabled,
                        tool_search_native,
                    )
                    .await?;
                    let response = with_account_header(response, &account.name);
                    // Surface the real status (a `502` when a backend error event
                    // fired on the non-streaming path, issue #113) to the access
                    // log and metrics rather than a hardcoded `200`.
                    return Ok((response.status(), response));
                }
                // A non-failover 4xx (e.g. 400) is a client error, not the
                // account's fault: relay it (re-shaped into the Anthropic error
                // envelope by mapped_upstream_error, as everywhere on this path)
                // rather than rotating to another account.
                return Err(mapped_upstream_error(status, upstream, auth).await);
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
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    status = %status,
                    "ChatGPT OAuth account failed over; cooling down and rotating to the next account"
                );
                last_response = Some(upstream);
            }
            FailoverAction::RefreshRetry => {
                // Unlike Claude, Codex's store never encodes a non-refreshable
                // "long-lived setup token" shape (see auth/codex/store.rs) — the
                // only static credential source is an explicit `token_env`.
                if account.token_env.is_some() {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "ChatGPT OAuth account returned 401 but its credential is not refreshable (token_env); cooling down"
                    );
                    last_response = Some(upstream);
                    continue;
                }

                let credentials_path = account
                    .credentials
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| auth::codex::store::account_path(&account.name));
                let store = CodexAuthStore::new(credentials_path, state.http_client.clone());
                // Serialize the refresh + credential writeback for this account
                // (see the refresh_lock note at the top of the loop); release
                // the lock again before the retry send below.
                let refreshed = {
                    let _guard = refresh_lock.lock().await;
                    match store.force_refresh().await {
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
                                "failed to force-refresh ChatGPT OAuth account"
                            );
                            last_response = Some(upstream);
                            continue;
                        }
                    }
                };
                let retry_credential = Credential::ChatGptOAuth {
                    access_token: refreshed.access_token,
                    account_id: refreshed.account_id,
                };
                let retry = match http_send(
                    &state,
                    &route,
                    retry_credential,
                    session_id.as_deref(),
                    bytes::Bytes::from(upstream_body.to_string()),
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        state.accounts.cooldown(
                            &route.provider,
                            &account.name,
                            Duration::from_secs(30),
                        );
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            error = %error,
                            "ChatGPT OAuth refresh retry failed"
                        );
                        last_response = Some(upstream);
                        continue;
                    }
                };
                let retry_status = retry.status();
                if retry_status == StatusCode::UNAUTHORIZED {
                    // Refresh succeeded but the credential is still rejected —
                    // the account is genuinely broken. Cool it down longer and
                    // rotate rather than relaying the 401 to the client.
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "ChatGPT OAuth account refreshed successfully but upstream still rejected the new credential; cooling down and rotating"
                    );
                    last_response = Some(retry);
                    continue;
                }
                // Classify the refreshed retry the same way the initial
                // response is classified, so a non-success outcome fails over
                // to the remaining accounts instead of short-circuiting the
                // pool.
                match accounts::classify_codex(retry_status, retry.headers()) {
                    FailoverAction::Relay => {
                        if retry_status.is_success() {
                            state.accounts.mark_healthy(&route.provider, &account.name);
                            let response = relay_success(
                                &state,
                                &route,
                                retry,
                                client_wants_stream,
                                thinking_enabled,
                                tool_search_native,
                            )
                            .await?;
                            let response = with_account_header(response, &account.name);
                            // Surface the real status (issue #113) rather than a
                            // hardcoded `200` — see the relay arm above.
                            return Ok((response.status(), response));
                        }
                        return Err(mapped_upstream_error(retry_status, retry, auth).await);
                    }
                    // Exhaustive rather than `_` so a new FailoverAction variant
                    // forces a decision here. Two of these arms are unreachable
                    // for the retry status and are matched only to document the
                    // invariants at the call site: classify_codex returns
                    // RefreshRetry only for 401, but a 401 retry already `continue`d
                    // at the `retry_status == UNAUTHORIZED` check above, so it never
                    // reaches this match; and it never returns PauseSame at all (its
                    // 429 arm always maps to Rotate). Only Relay and Rotate are live
                    // here — RefreshRetry rides Rotate's arm as a defensive no-op.
                    FailoverAction::Rotate | FailoverAction::RefreshRetry => {
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
                            "ChatGPT OAuth refresh retry did not succeed; rotating to the next account"
                        );
                        last_response = Some(retry);
                        continue;
                    }
                    FailoverAction::PauseSame => {
                        unreachable!("classify_codex never returns PauseSame")
                    }
                }
            }
            FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
        }
    }

    match last_response {
        Some(upstream) => {
            let status = upstream.status();
            Err(mapped_upstream_error(status, upstream, auth).await)
        }
        None => Err(own_error(
            "all Codex OAuth accounts failed before receiving an upstream response".to_string(),
        )),
    }
}

/// Relay a successful upstream Responses answer to the client, choosing SSE
/// or a single JSON body per `client_wants_stream`. Thin wrapper shared by
/// every success arm in [`forward_chatgpt_oauth`] so each only differs in
/// which upstream response and account produced it (mirrors how the
/// single-account [`forward_http`] picks between [`stream_response`] and
/// [`json_response`]).
async fn relay_success(
    state: &AppState,
    route: &Route,
    upstream: reqwest::Response,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> Result<axum::response::Response, AdapterError> {
    if client_wants_stream {
        let keepalive = Duration::from_secs(state.config.server.sse_keepalive_seconds);
        // The pool path does not (yet) pre-compute a tiktoken input estimate to
        // seed message_start (#112 threads it only through the single-account
        // forward_http / forward_websocket paths), so pass 0 = "no estimate"
        // here — identical to those paths when the provider opts out of local
        // counting. Extending the estimate to pooled codex turns is a follow-up.
        Ok(stream_response(
            upstream,
            route.model.clone(),
            thinking_enabled,
            tool_search_native,
            0,
            keepalive,
        ))
    } else {
        json_response(
            upstream,
            route.model.clone(),
            thinking_enabled,
            tool_search_native,
        )
        .await
    }
}

/// Inject `x-shunt-account` naming which pool account produced the response,
/// mirroring the Anthropic adapter's `relay_response`. Silently skipped if the
/// account name is not a valid header value — should never happen, since
/// account names are validated against `[a-z0-9-]+` at import time (see
/// `auth::codex::store::validate_account_name`).
fn with_account_header(
    mut response: axum::response::Response,
    account_name: &str,
) -> axum::response::Response {
    if let Ok(value) = HeaderValue::from_str(account_name) {
        response.headers_mut().insert("x-shunt-account", value);
    }
    response
}
