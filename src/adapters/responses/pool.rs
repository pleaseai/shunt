//! Codex/ChatGPT OAuth account-pool failover (M10): try each account in turn,
//! websocket-first when enabled with an HTTP fallback per account, classifying
//! each raw upstream status to decide relay / rotate / refresh-and-retry.

use std::{path::PathBuf, time::Duration};

use axum::http::{HeaderValue, StatusCode};

use crate::{
    accounts::{self, FailoverAction},
    adapters::AdapterError,
    auth::{self, codex::auth::CodexAuthStore, resolve_chatgpt_account, Credential},
    config::{AccountConfig, AuthMode},
    routing::Route,
    server::AppState,
};

use super::context::{ForwardOptions, PoolForward, RelayOptions};
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
/// Codex quota headers are recorded for the admin dashboard, but selection
/// remains deliberately cooldown-based rather than quota-aware.
pub(super) async fn forward_chatgpt_oauth(
    state: AppState,
    route: Route,
    forward: PoolForward,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let PoolForward {
        pool_key,
        session_id,
        upstream_body,
        accounts_config,
        turn,
    } = forward;
    // An all-`disabled` pool yields an empty order; surface it as a distinct
    // config error rather than the generic "all accounts failed" below.
    if !accounts_config.is_empty() && accounts_config.iter().all(|account| account.disabled) {
        tracing::warn!(
            provider = %route.provider,
            accounts = accounts_config.len(),
            "all accounts for provider are disabled; none are selectable"
        );
        return Err(own_error(format!(
            "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
            route.provider,
            accounts_config.len()
        )));
    }
    // Codex usage is recorded for the admin dashboard via note_codex_quota,
    // but it is deliberately not fed into selection: failover stays
    // cooldown-based. Per-account priority/disabled still apply.
    let order = state.accounts.select_order_cooldown(
        &route.provider,
        &accounts_config,
        session_id.as_deref(),
    );
    let ws_enabled = state.config.codex_websocket_enabled(&route.provider);
    let auth = AuthMode::ChatgptOauth;
    let mut last_response: Option<reqwest::Response> = None;

    for index in order {
        let account = &accounts_config[index];

        // Resolve the account's credential under its per-account refresh lock
        // (see resolve_or_cooldown); a resolution failure cools it down and
        // rotates to the next account.
        let credential = match resolve_or_cooldown(&state, &route, account).await {
            Some(credential) => credential,
            None => continue,
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
                ForwardOptions {
                    upstream_body: upstream_body.clone(),
                    credential: credential.clone(),
                    auth,
                    turn,
                    codex_quota_account: Some(account.name.clone()),
                    // Pool path does not pre-compute a message_start input estimate
                    // yet (see relay_success) — follow-up to thread it through here.
                    estimate_input: None,
                },
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

        state
            .accounts
            .note_codex_quota(&route.provider, &account.name, upstream.headers());
        match classify_first(&state, &route, account, upstream) {
            FirstOutcome::Relay(upstream) => {
                // A non-401/429/5xx response means the account itself is fine,
                // whether or not this particular request succeeded (mirrors the
                // Anthropic adapter's top-level Relay arm).
                let status = upstream.status();
                state.accounts.mark_healthy(&route.provider, &account.name);
                if status.is_success() {
                    let response = relay_success(
                        &state,
                        upstream,
                        turn.client_wants_stream,
                        turn.relay(&route),
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
            FirstOutcome::Rotate(upstream) => {
                last_response = Some(upstream);
            }
            FirstOutcome::NeedRefresh(upstream) => {
                // Force-refresh the account's stored credential under its refresh
                // lock (see force_refresh_or_cooldown); a `token_env` account or a
                // refresh failure cools it down and rotates instead.
                let retry_credential =
                    match force_refresh_or_cooldown(&state, &route, account, &credential).await {
                        Some(credential) => credential,
                        None => {
                            last_response = Some(upstream);
                            continue;
                        }
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
                state
                    .accounts
                    .note_codex_quota(&route.provider, &account.name, retry.headers());
                match classify_retry(&state, &route, account, retry) {
                    RetryOutcome::Relay(retry) => {
                        let retry_status = retry.status();
                        if retry_status.is_success() {
                            state.accounts.mark_healthy(&route.provider, &account.name);
                            let response = relay_success(
                                &state,
                                retry,
                                turn.client_wants_stream,
                                turn.relay(&route),
                            )
                            .await?;
                            let response = with_account_header(response, &account.name);
                            // Surface the real status (issue #113) rather than a
                            // hardcoded `200` — see the relay arm above.
                            return Ok((response.status(), response));
                        }
                        return Err(mapped_upstream_error(retry_status, retry, auth).await);
                    }
                    RetryOutcome::Rotate(retry) => {
                        last_response = Some(retry);
                    }
                }
            }
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
    upstream: reqwest::Response,
    client_wants_stream: bool,
    relay: RelayOptions,
) -> Result<axum::response::Response, AdapterError> {
    if client_wants_stream {
        let keepalive = Duration::from_secs(state.config.server.sse_keepalive_seconds);
        // The pool path does not (yet) pre-compute a tiktoken input estimate to
        // seed message_start (#112 threads it only through the single-account
        // forward_http / forward_websocket paths), so pass 0 = "no estimate"
        // here — identical to those paths when the provider opts out of local
        // counting. Extending the estimate to pooled codex turns is a follow-up.
        Ok(stream_response(upstream, relay, 0, keepalive))
    } else {
        json_response(upstream, relay).await
    }
}

/// Inject `x-shunt-account` naming which pool account produced the response,
/// mirroring the Anthropic adapter's `relay_response`. Silently skipped if the
/// account name is not a valid header value — should never happen, since
/// account names are validated against `[a-z0-9-]+` at import time (see
/// `auth::codex::store::validate_account_name`).
pub(super) fn with_account_header(
    mut response: axum::response::Response,
    account_name: &str,
) -> axum::response::Response {
    if let Ok(value) = HeaderValue::from_str(account_name) {
        response.headers_mut().insert("x-shunt-account", value);
    }
    response
}

// --- Shared Codex/ChatGPT pool failover primitives ---------------------------
//
// These are the parts of the per-account failover machine that are identical
// between the translating outbound path ([`forward_chatgpt_oauth`], above) and
// the verbatim inbound passthrough (`responses::inbound::forward_codex_inbound`):
// cooldown timing, credential resolution, and force-refresh. Sharing them keeps
// the two paths from drifting and avoids duplicating the cooldown/refresh rules.

/// Cooldown for a rotate-worthy upstream status on the Codex pool: honor a 429's
/// `retry-after` (clamped to 1s..=1h), otherwise a flat 30s. Shared so the
/// translating and passthrough paths back off identically.
pub(super) fn rotate_cooldown(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Duration {
    if status == StatusCode::TOO_MANY_REQUESTS {
        accounts::retry_after(headers)
            .unwrap_or(Duration::from_secs(60))
            .clamp(Duration::from_secs(1), Duration::from_secs(3600))
    } else {
        Duration::from_secs(30)
    }
}

/// Resolve one Codex/ChatGPT OAuth account's credential under its per-account
/// refresh lock. On failure the account is cooled down for 5 minutes and logged,
/// and `None` signals the caller to rotate to the next account. The lock is
/// released before the caller sends upstream (never held across a send).
pub(super) async fn resolve_or_cooldown(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
) -> Option<Credential> {
    let refresh_lock = state.accounts.refresh_lock(&route.provider, &account.name);
    let _guard = refresh_lock.lock().await;
    match resolve_chatgpt_account(account, &state.http_client).await {
        Ok(credential) => Some(credential),
        Err(error) => {
            state
                .accounts
                .cooldown(&route.provider, &account.name, Duration::from_secs(5 * 60));
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                error = %error.message,
                "failed to resolve ChatGPT OAuth account"
            );
            None
        }
    }
}

pub(super) fn chatgpt_access_token(credential: &Credential) -> Option<&str> {
    match credential {
        Credential::ChatGptOAuth { access_token, .. } => Some(access_token),
        _ => None,
    }
}

/// Force-refresh one Codex/ChatGPT OAuth account's stored credential under its
/// refresh lock, returning the refreshed credential to retry with. A `token_env`
/// (static) account has nothing to refresh — unlike Claude, Codex's store never
/// encodes a non-refreshable "long-lived setup token" shape (see
/// auth/codex/store.rs), so the only static source is an explicit `token_env` —
/// and a refresh failure likewise cools the account down. Either case returns
/// `None` to signal the caller to rotate. The lock is released before the caller
/// retries upstream (never held across a send).
pub(super) async fn force_refresh_or_cooldown(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    credential: &Credential,
) -> Option<Credential> {
    if account.token_env.is_some() {
        state
            .accounts
            .cooldown(&route.provider, &account.name, Duration::from_secs(5 * 60));
        tracing::warn!(
            provider = %route.provider,
            account = %account.name,
            "ChatGPT OAuth account returned 401 but its credential is not refreshable (token_env); cooling down"
        );
        return None;
    }

    let rejected_access_token = match chatgpt_access_token(credential) {
        Some(access_token) => access_token,
        None => {
            state
                .accounts
                .cooldown(&route.provider, &account.name, Duration::from_secs(5 * 60));
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                "Codex pool account returned 401 with a non-ChatGPT credential; cooling down"
            );
            return None;
        }
    };

    let credentials_path = account
        .credentials
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| auth::codex::store::account_path(&account.name));
    let store = CodexAuthStore::new(credentials_path, state.http_client.clone());
    let refresh_lock = state.accounts.refresh_lock(&route.provider, &account.name);
    let _guard = refresh_lock.lock().await;
    match store
        .force_refresh_if_access_token(rejected_access_token)
        .await
    {
        Ok(refreshed) => Some(Credential::ChatGptOAuth {
            access_token: refreshed.access_token,
            account_id: refreshed.account_id,
        }),
        Err(error) => {
            state
                .accounts
                .cooldown(&route.provider, &account.name, Duration::from_secs(5 * 60));
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                error = %error.message,
                "failed to force-refresh ChatGPT OAuth account"
            );
            None
        }
    }
}

/// The classification of a **first-attempt** upstream response on the Codex pool.
/// The account-specific relay rendering (translate vs verbatim, and the
/// `mark_healthy`) stays with the caller; the shared cooldown/rotate bookkeeping
/// is applied here so the translating and passthrough paths classify identically.
pub(super) enum FirstOutcome {
    /// The account is fine — the caller marks it healthy and relays this response
    /// its own way.
    Relay(reqwest::Response),
    /// The account failed over (already cooled down); the caller stashes this
    /// response as the pool's last-seen and rotates.
    Rotate(reqwest::Response),
    /// A 401 — the caller should force-refresh and retry this account.
    NeedRefresh(reqwest::Response),
}

/// Classify a first-attempt upstream response, applying the shared rotate cooldown.
/// A `Relay` account is left for the caller to mark healthy so it can render the
/// response its own way (a translating path splits success vs a non-failover 4xx;
/// the passthrough relays verbatim).
pub(super) fn classify_first(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    upstream: reqwest::Response,
) -> FirstOutcome {
    let status = upstream.status();
    match accounts::classify_codex(status, upstream.headers()) {
        FailoverAction::Relay => FirstOutcome::Relay(upstream),
        FailoverAction::Rotate => {
            let cooldown = rotate_cooldown(status, upstream.headers());
            state
                .accounts
                .cooldown(&route.provider, &account.name, cooldown);
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                status = %status,
                "codex pool account failed over; cooling down and rotating to the next account"
            );
            FirstOutcome::Rotate(upstream)
        }
        FailoverAction::RefreshRetry => FirstOutcome::NeedRefresh(upstream),
        FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
    }
}

/// The classification of a **refreshed-retry** upstream response on the Codex pool.
pub(super) enum RetryOutcome {
    /// The caller marks the account healthy and relays this refreshed response.
    Relay(reqwest::Response),
    /// Rotate (already cooled down); the caller stashes this as the pool's
    /// last-seen.
    Rotate(reqwest::Response),
}

/// Classify a refreshed-retry upstream response. A retry still rejected with 401
/// (the refresh succeeded but the credential is still bad) or otherwise
/// non-relayable cools the account down and rotates; only a relayable status is
/// handed back for the caller to render. `classify_codex` returns `RefreshRetry`
/// only for 401 (handled above) and never `PauseSame`, so only `Relay` and
/// `Rotate` are live — the others ride `Rotate`'s arm as a defensive no-op.
pub(super) fn classify_retry(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    retry: reqwest::Response,
) -> RetryOutcome {
    let retry_status = retry.status();
    if retry_status == StatusCode::UNAUTHORIZED {
        state
            .accounts
            .cooldown(&route.provider, &account.name, Duration::from_secs(5 * 60));
        tracing::warn!(
            provider = %route.provider,
            account = %account.name,
            "codex pool account refreshed but upstream still rejected the new credential; cooling down and rotating"
        );
        return RetryOutcome::Rotate(retry);
    }
    match accounts::classify_codex(retry_status, retry.headers()) {
        FailoverAction::Relay => RetryOutcome::Relay(retry),
        FailoverAction::Rotate | FailoverAction::RefreshRetry => {
            let cooldown = rotate_cooldown(retry_status, retry.headers());
            state
                .accounts
                .cooldown(&route.provider, &account.name, cooldown);
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                status = %retry_status,
                "codex pool refresh retry did not succeed; rotating to the next account"
            );
            RetryOutcome::Rotate(retry)
        }
        FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
    }
}
