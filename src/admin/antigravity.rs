//! Antigravity account provisioning + refresh handlers for the admin web surface.
//!
//! Antigravity's registered OAuth redirect is a fixed loopback port
//! (`http://localhost:51121/oauth-callback`) that the admin server never
//! listens on — like Codex, not Claude's manual out-of-band redirect. The
//! browser's address bar still shows `?code=...&state=...` after the failed
//! connection, so completion reuses the same "paste the redirect URL" shape
//! as `codex::parse_callback_value`.

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::{
        antigravity::{
            auth as antigravity_auth, login as antigravity_login, login_base_url,
            store as antigravity_store,
        },
        inbound::constant_time_eq,
        shared::generate_pkce,
    },
    config::AuthMode,
    server::AppState,
};

use super::{
    authenticate, bad_gateway, bad_request, check_csrf, cleanup_reprovisioned_pool_health,
    forget_pool_health_if_absent, internal, json_secure, not_found, remaining_account_identities,
    require_write,
    session::{PendingAttempt, PendingKind, COMPLETION_EXCHANGE_TIMEOUT},
    too_many_requests, unauthorized,
};

const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";

fn antigravity_pending_key(name: &str) -> String {
    format!("antigravity/{name}")
}

/// Parse a pasted OAuth callback value: either the full redirect URL (its
/// `code`/`state` query params) or a bare `<code>#<state>` pair. Identical
/// shape to `codex::parse_callback_value` — both providers register a
/// fixed loopback redirect the admin server does not listen on, so the
/// browser shows the failed-connection URL for the operator to copy.
fn parse_callback_value(pasted: &str) -> Option<(String, String)> {
    if let Ok(url) = reqwest::Url::parse(pasted) {
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                _ => {}
            }
        }
        if let (Some(code), Some(state)) = (code, state) {
            return Some((code, state));
        }
    }
    let (code, state) = pasted.split_once('#')?;
    Some((code.to_string(), state.to_string()))
}

#[derive(Deserialize)]
pub(super) struct AddAntigravityBody {
    name: String,
}

pub(super) async fn add_antigravity_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<AddAntigravityBody>, JsonRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = require_write(&authok) {
        return response;
    }
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return bad_request("invalid JSON body");
    };
    if antigravity_store::validate_account_name(&body.name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let pkce = generate_pkce();
    let authorize_url =
        antigravity_login::build_auth_url(&pkce.challenge, &pkce.state, REDIRECT_URI);
    state.admin_stores.pending.start(
        &antigravity_pending_key(&body.name),
        PendingKind::AntigravityOauth,
        pkce.verifier,
        pkce.state,
        authok.auth.pending_ttl(),
    );
    tracing::info!(account = %body.name, "admin: Antigravity account provisioning started");
    json_secure(json!({ "name": body.name, "authorize_url": authorize_url }))
}

#[derive(Deserialize)]
pub(super) struct CompleteAntigravityBody {
    code: String,
}

pub(super) async fn complete_antigravity_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CompleteAntigravityBody>, JsonRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = require_write(&authok) {
        return response;
    }
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if !state.admin_stores.complete_rate.check() {
        return too_many_requests("too many completion attempts; slow down");
    }
    let Ok(Json(body)) = body else {
        return bad_request("invalid JSON body");
    };
    if antigravity_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let key = antigravity_pending_key(&name);
    // Held for the rest of the handler; see `PendingStore::lock_completion` for
    // the interleaving this closes (#440).
    let _completion = state.admin_stores.pending.lock_completion(&key).await;
    let pending = match state.admin_stores.pending.attempt(&key) {
        PendingAttempt::Ready(pending) => pending,
        PendingAttempt::NotFound => {
            tracing::info!(
                account = %name,
                "admin: completion found no pending login (no start, expired, or consumed by a concurrent completion)"
            );
            return bad_request("no pending login for this account; start again");
        }
        PendingAttempt::TooManyAttempts => return bad_request("too many attempts; start again"),
    };
    if pending.kind != PendingKind::AntigravityOauth {
        return internal("unexpected pending kind on the antigravity route");
    }

    let Some((code, returned_state)) = parse_callback_value(body.code.trim()) else {
        return bad_request("authorization value must be a redirect URL or <code>#<state>");
    };
    if code.is_empty() || !constant_time_eq(returned_state.as_bytes(), pending.state.as_bytes()) {
        return bad_request("invalid authorization code or OAuth state mismatch");
    }

    // Mirror the Codex completion flow's token URL override: warn on an invalid
    // or unsafe `SHUNT_ANTIGRAVITY_TOKEN_URL` rather than the silent fallback
    // the background refresh path gives — this handler consumes the
    // single-use authorization code, so a typo'd override must not quietly
    // burn the real code against production with no trace in the logs.
    let token_url = crate::auth::shared::admin_token_url_override(
        "SHUNT_ANTIGRAVITY_TOKEN_URL",
        antigravity_auth::TOKEN_URL,
    );
    let exchange = antigravity_login::exchange_code(
        &state.http_client,
        &token_url,
        &code,
        REDIRECT_URI,
        &pending.verifier,
        COMPLETION_EXCHANGE_TIMEOUT,
    );
    // Bounded because the completion lock is held across it; see
    // `COMPLETION_EXCHANGE_TIMEOUT`.
    let tokens = match tokio::time::timeout(COMPLETION_EXCHANGE_TIMEOUT, exchange).await {
        Ok(Ok(tokens)) => tokens,
        Ok(Err(error)) => {
            tracing::warn!(account = %name, %error, "admin: Antigravity token exchange failed");
            return bad_gateway("Antigravity token exchange failed");
        }
        Err(_elapsed) => {
            tracing::warn!(account = %name, "admin: Antigravity token exchange timed out");
            return bad_gateway("Antigravity token exchange timed out");
        }
    };
    let Some(refresh_token) = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
    else {
        tracing::warn!(account = %name, "admin: Antigravity token exchange did not return a refresh token");
        return bad_gateway(
            "Antigravity token exchange did not return a refresh token; retry the login (the \
             authorization URL requests offline access, so this should not happen)",
        );
    };

    let base_url = login_base_url(Some(state.config.as_ref()));
    let discovery_store = antigravity_auth::AntigravityAuthStore::new(
        antigravity_store::account_path(&name),
        state.http_client.clone(),
        base_url,
    );
    let project_id = match discovery_store.discover_project(&tokens.access_token).await {
        Ok(project_id) => Some(project_id),
        Err(error) => {
            // Not fatal: the credential is still stored and discovery is
            // retried on the account's first request, mirroring the CLI
            // login's same non-fatal treatment.
            tracing::warn!(account = %name, error = %error.message, "admin: Antigravity project discovery failed; will retry on first request");
            None
        }
    };
    // Mirrors the token URL override above: lets tests point the admin
    // completion flow's userinfo lookup at a mock server instead of the real
    // Google endpoint, without adding an unbounded live network dependency
    // to `/admin/api/accounts/antigravity/{name}/complete`.
    let userinfo_url = crate::auth::shared::admin_token_url_override(
        "SHUNT_ANTIGRAVITY_USERINFO_URL",
        antigravity_auth::USERINFO_URL,
    );
    let email = match antigravity_login::fetch_email(
        &state.http_client,
        &userinfo_url,
        &tokens.access_token,
        antigravity_login::USERINFO_REQUEST_TIMEOUT,
    )
    .await
    {
        Ok(email) => email,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: Antigravity email lookup failed; continuing without a label");
            None
        }
    };

    let account_name = name.clone();
    let access_token = tokens.access_token;
    let expiry_date = antigravity_login::expiry_millis(tokens.expires_in.unwrap_or(3600));
    let stored = tokio::task::spawn_blocking(move || {
        antigravity_store::store_oauth_tokens(
            &account_name,
            &access_token,
            &refresh_token,
            expiry_date,
            email.as_deref(),
            project_id.as_deref(),
        )
    })
    .await;
    match stored {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to persist Antigravity account after successful token exchange");
            return internal("failed to store account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: Antigravity account persistence task panicked");
            return internal("failed to store account");
        }
    }
    state.admin_stores.pending.remove(&key);
    // Antigravity accounts never carry a `uuid` — every account's identity is
    // its own file name (see `antigravity_store::account_identity`) — so a
    // reprovision can never change the identity, unlike Claude/Codex. Only the
    // new (== only) identity needs a pool-health clear.
    let other_identities =
        remaining_account_identities(&name, antigravity_store::scan_accounts_strict).await;
    if other_identities.is_none() {
        tracing::warn!(account = %name, "admin: failed to scan Antigravity account store during reprovision cleanup; preserving dynamic-discovery-provider pool health");
    }
    cleanup_reprovisioned_pool_health(
        &state,
        AuthMode::AntigravityOauth,
        None,
        &name,
        Some(name.as_str()),
        other_identities.as_ref(),
    );
    state.accounts.set_needs_relogin_for_store_account(
        crate::accounts::StoreFamily::Antigravity,
        &name,
        None,
        false,
    );
    tracing::info!(account = %name, "admin: Antigravity account stored");

    let live = state.config.providers.values().any(|provider| {
        provider.auth == AuthMode::AntigravityOauth && provider.accounts.is_empty()
    });
    let message = if live {
        "Refreshable Antigravity login stored and live now (an empty-accounts provider scans the store each request)."
    } else {
        "Refreshable Antigravity login stored. Add a name-only [[providers.<name>.accounts]] entry and reload to activate it."
    };
    json_secure(json!({ "name": name, "stored": true, "live": live, "message": message }))
}

/// Probe one imported Antigravity account's refresh grant on demand, mirroring
/// `refresh_account` (Claude). `AntigravityAuthStore::force_refresh_if_access_token`
/// takes the process-global `REFRESH_LOCK` across read → POST → atomic
/// writeback, so this never races the proxy's own refresh or the background
/// refresher. Unlike Claude's unconditional `force_refresh`, Antigravity's
/// store only exposes the "if still the rejected token" form — but reading
/// the current on-disk token first and passing it back in has the same
/// effect, since `read()` inside it will see that same token as current.
pub(super) async fn refresh_antigravity_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = require_write(&authok) {
        return response;
    }
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if !state.admin_stores.complete_rate.check() {
        return too_many_requests("too many refresh attempts; slow down");
    }
    if antigravity_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let token_name = name.clone();
    let access_token = match tokio::task::spawn_blocking(move || {
        antigravity_store::stored_access_token(&token_name)
    })
    .await
    {
        Ok(token) => token,
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: account metadata task panicked");
            return internal("failed to read the account");
        }
    };
    let Some(access_token) = access_token else {
        return not_found();
    };

    let base_url = login_base_url(Some(state.config.as_ref()));
    let store = antigravity_auth::AntigravityAuthStore::new(
        antigravity_store::account_path(&name),
        state.http_client.clone(),
        base_url,
    );
    let outcome = store.force_refresh_if_access_token(&access_token).await;

    match outcome {
        Ok(_) => {
            state.accounts.clear_grant_relogin_for_store_account(
                crate::accounts::StoreFamily::Antigravity,
                &name,
                None,
            );
            let still_marked = state.accounts.store_account_needs_relogin(
                crate::accounts::StoreFamily::Antigravity,
                &name,
                None,
            );
            let expiry_name = name.clone();
            let expires_at = tokio::task::spawn_blocking(move || {
                antigravity_store::account_meta(&expiry_name).and_then(|meta| meta.expires_at)
            })
            .await
            .unwrap_or(None);
            tracing::info!(account = %name, "admin: Antigravity account refresh probe succeeded");
            json_secure(json!({
                "name": name,
                "refreshed": true,
                "needs_relogin": still_marked,
                "expires_at": expires_at,
                "message": if still_marked {
                    "Refresh succeeded, but this account still needs a re-login: the provider \
                     rejected a token it had already issued to this account. Remove and re-add it."
                } else {
                    "Refresh succeeded; this login is alive."
                }
            }))
        }
        Err(error) => {
            let terminal = error.terminal;
            if terminal {
                state.accounts.set_needs_relogin_for_store_account(
                    crate::accounts::StoreFamily::Antigravity,
                    &name,
                    None,
                    true,
                );
            }
            tracing::warn!(
                account = %name,
                error = %error.error.message,
                terminal,
                "admin: Antigravity account refresh probe failed"
            );
            if terminal {
                bad_request(
                    "this account's stored credential can no longer produce an access token \
                     — the provider rejected the refresh token, the file carries none, or a \
                     rotated pair was lost. It needs a re-login — remove and re-add the account",
                )
            } else {
                bad_gateway(
                    "the refresh attempt failed without a terminal verdict; the provider may \
                     be unavailable. See the server logs and try again",
                )
            }
        }
    }
}

pub(super) async fn remove_antigravity_account_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = require_write(&authok) {
        return response;
    }
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if antigravity_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let target = name.clone();
    let removed = match tokio::task::spawn_blocking(move || {
        antigravity_store::remove_account(&target)
    })
    .await
    {
        Ok(Ok(removed)) => removed,
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to remove Antigravity account");
            return internal("failed to remove account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: Antigravity remove_account task panicked");
            return internal("failed to remove account");
        }
    };
    tracing::info!(account = %name, removed, "admin: Antigravity account removed");
    // Antigravity's identity is always its own name (no `uuid`), so there is
    // exactly one identity to forget — no separate old/new identity dance.
    let store_scan_others = match tokio::task::spawn_blocking(
        antigravity_store::scan_accounts_strict,
    )
    .await
    {
        Ok(Ok(remaining)) => Some(
            remaining
                .into_iter()
                .map(|account| crate::accounts::account_identity(&account).to_string())
                .collect::<std::collections::HashSet<String>>(),
        ),
        Ok(Err(error)) => {
            tracing::warn!(account = %name, %error, "admin: failed to scan Antigravity account store after removal; preserving dynamic-discovery-provider pool health for the removed identity");
            None
        }
        Err(join_error) => {
            tracing::warn!(account = %name, %join_error, "admin: Antigravity account store scan task panicked after removal; preserving dynamic-discovery-provider pool health for the removed identity");
            None
        }
    };
    forget_pool_health_if_absent(
        &state,
        AuthMode::AntigravityOauth,
        &name,
        Some(name.as_str()),
        store_scan_others.as_ref(),
    );
    json_secure(json!({ "name": name, "removed": removed }))
}

pub(super) async fn list_antigravity_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }
    match tokio::task::spawn_blocking(antigravity_store::list_account_meta).await {
        Ok(Ok(accounts)) => json_secure(json!({ "accounts": accounts })),
        Ok(Err(error)) => {
            tracing::error!(%error, "admin: failed to list Antigravity account metadata");
            internal("failed to list accounts")
        }
        Err(join_error) => {
            tracing::error!(%join_error, "admin: Antigravity list_account_meta task panicked");
            internal("failed to list accounts")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_callback_value;

    #[test]
    fn parses_full_redirect_and_code_state_values() {
        assert_eq!(
            parse_callback_value("http://localhost:51121/oauth-callback?code=a%2Bb&state=s%2F1"),
            Some(("a+b".to_string(), "s/1".to_string()))
        );
        assert_eq!(
            parse_callback_value("code#state"),
            Some(("code".to_string(), "state".to_string()))
        );
        assert_eq!(parse_callback_value("missing-state"), None);
    }
}
