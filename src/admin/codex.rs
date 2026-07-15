//! Codex/ChatGPT account provisioning handlers for the admin web surface.

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
        codex::{auth as codex_auth, login as codex_login, store as codex_store},
        inbound::constant_time_eq,
        shared::generate_pkce,
    },
    config::AuthMode,
    server::AppState,
};

use super::{
    authenticate, bad_gateway, bad_request, check_csrf, forget_pool_health, internal, json_secure,
    session::{PendingAttempt, PendingKind},
    too_many_requests, unauthorized,
};

fn codex_pending_key(name: &str) -> String {
    format!("codex/{name}")
}

#[derive(Deserialize)]
pub(super) struct AddCodexBody {
    name: String,
}

pub(super) async fn add_codex_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<AddCodexBody>, JsonRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return bad_request("invalid JSON body");
    };
    if codex_store::validate_account_name(&body.name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let pkce = generate_pkce();
    let authorize_url = match codex_login::build_authorize_url(
        &pkce.challenge,
        &pkce.state,
        codex_login::REDIRECT_URI,
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(account = %body.name, %error, "admin: failed to build Codex authorize URL");
            return internal("failed to build authorize URL");
        }
    };
    state.admin_stores.pending.start(
        &codex_pending_key(&body.name),
        PendingKind::CodexOauth,
        pkce.verifier,
        pkce.state,
        authok.auth.pending_ttl(),
    );
    tracing::info!(account = %body.name, "admin: Codex account provisioning started");
    json_secure(json!({ "name": body.name, "authorize_url": authorize_url.to_string() }))
}

#[derive(Deserialize)]
pub(super) struct CompleteCodexBody {
    code: String,
}

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

pub(super) async fn complete_codex_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CompleteCodexBody>, JsonRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if !state.admin_stores.complete_rate.check() {
        return too_many_requests("too many completion attempts; slow down");
    }
    let Ok(Json(body)) = body else {
        return bad_request("invalid JSON body");
    };
    if codex_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let key = codex_pending_key(&name);
    let pending = match state.admin_stores.pending.attempt(&key) {
        PendingAttempt::Ready(pending) => pending,
        PendingAttempt::NotFound => {
            return bad_request("no pending login for this account; start again")
        }
        PendingAttempt::TooManyAttempts => return bad_request("too many attempts; start again"),
    };
    if pending.kind != PendingKind::CodexOauth {
        return internal("unexpected claude pending on the codex route");
    }

    let Some((code, returned_state)) = parse_callback_value(body.code.trim()) else {
        return bad_request("authorization value must be a redirect URL or <code>#<state>");
    };
    if code.is_empty() || !constant_time_eq(returned_state.as_bytes(), pending.state.as_bytes()) {
        return bad_request("invalid authorization code or OAuth state mismatch");
    }

    // Mirror the Claude completion flow's `admin_token_url()`: warn on an invalid or
    // unsafe `SHUNT_CODEX_TOKEN_URL` override rather than the silent fallback the
    // background refresh path's `resolve_oauth_token_url` gives — this handler
    // consumes the single-use authorization code, so a typo'd override must not
    // quietly burn the real code against production with no trace in the logs.
    let token_url = crate::auth::shared::admin_token_url_override(
        "SHUNT_CODEX_TOKEN_URL",
        codex_auth::TOKEN_URL,
    );
    let tokens = match codex_login::exchange_code(
        &state.http_client,
        &code,
        &pending.verifier,
        codex_login::REDIRECT_URI,
        &token_url,
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: Codex token exchange failed");
            return bad_gateway("Codex token exchange failed");
        }
    };
    let Some(refresh_token) = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
    else {
        tracing::warn!(account = %name, "admin: Codex token exchange did not return a refresh token");
        return bad_gateway("Codex token exchange did not return a refresh token");
    };
    let Some(account_id) = codex_auth::jwt_account_id(&tokens.access_token) else {
        tracing::warn!(account = %name, "admin: Codex token exchange did not return an account id");
        return bad_gateway("Codex token exchange did not return an account id");
    };

    let account_name = name.clone();
    let access_token = tokens.access_token;
    let id_token = tokens.id_token;
    let stored = tokio::task::spawn_blocking(move || {
        codex_store::store_chatgpt_tokens(
            &account_name,
            &access_token,
            &refresh_token,
            id_token.as_deref(),
            &account_id,
        )
    })
    .await;
    match stored {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to persist Codex account after successful token exchange");
            return internal("failed to store account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: Codex account persistence task panicked");
            return internal("failed to store account");
        }
    }
    state.admin_stores.pending.remove(&key);
    // Re-provisioning reuses the account name; clear any process-lifetime Codex
    // pool health from a prior token without touching same-named Claude health.
    forget_pool_health(&state, AuthMode::ChatgptOauth, &name);
    tracing::info!(account = %name, account_id_present = true, "admin: Codex account stored");

    let live =
        state.config.providers.values().any(|provider| {
            provider.auth == AuthMode::ChatgptOauth && provider.accounts.is_empty()
        });
    let message = if live {
        "Refreshable ChatGPT OAuth login stored and live now (an empty-accounts provider scans the store each request)."
    } else {
        "Refreshable ChatGPT OAuth login stored. Add a name-only [[providers.<name>.accounts]] entry and reload to activate it."
    };
    json_secure(json!({ "name": name, "stored": true, "live": live, "message": message }))
}

pub(super) async fn remove_codex_account_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if codex_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let target = name.clone();
    let removed = match tokio::task::spawn_blocking(move || codex_store::remove_account(&target))
        .await
    {
        Ok(Ok(removed)) => removed,
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to remove Codex account");
            return internal("failed to remove account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: Codex remove_account task panicked");
            return internal("failed to remove account");
        }
    };
    tracing::info!(account = %name, removed, "admin: Codex account removed");
    // Drop process-lifetime Codex pool health for the removed name so a later
    // re-add does not inherit stale state, without touching same-named Claude health.
    forget_pool_health(&state, AuthMode::ChatgptOauth, &name);
    json_secure(json!({ "name": name, "removed": removed }))
}

pub(super) async fn list_codex_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }
    match tokio::task::spawn_blocking(codex_store::list_account_meta).await {
        Ok(Ok(accounts)) => json_secure(json!({ "accounts": accounts })),
        Ok(Err(error)) => {
            tracing::error!(%error, "admin: failed to list Codex account metadata");
            internal("failed to list accounts")
        }
        Err(join_error) => {
            tracing::error!(%join_error, "admin: Codex list_account_meta task panicked");
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
            parse_callback_value("http://localhost:1455/auth/callback?code=a%2Bb&state=s%2F1"),
            Some(("a+b".to_string(), "s/1".to_string()))
        );
        assert_eq!(
            parse_callback_value("code#state"),
            Some(("code".to_string(), "state".to_string()))
        );
        assert_eq!(parse_callback_value("missing-state"), None);
    }
}
