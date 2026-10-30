//! The chatgpt-plan family (`StoreFamily::ChatgptPlan`, ADR-0007) inside the
//! Responses account pool.
//!
//! The pool loop in `pool.rs` is shared with codex and xAI; it forks here only
//! where a plan account differs:
//!
//! - credentials come from the plan refresh store
//!   ([`ChatGptPlanAuthStore`]), keyed by account name, not from the codex
//!   store, which needs a `chatgpt_account_id` claim plan tokens lack;
//! - the forced refresh after an upstream 401 goes through the same store;
//! - the translated body passes one kill switch
//!   ([`PLAN_FORWARDS_MAX_OUTPUT_TOKENS`]);
//! - gateway-owned error wording points at ChatGPT usage settings (429) and
//!   at `shunt login chatgpt-plan` (401).
//!
//! HTTP classification is `accounts::classify_codex` unchanged: 429 and 5xx
//! rotate with a cooldown (credentials kept), 401 refreshes once and retries,
//! and every other 4xx — the SIWC 403 permission codes included — relays
//! without a retry or a refresh.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::Value;

use crate::{
    accounts,
    auth::{chatgpt_plan::auth::ChatGptPlanAuthStore, codex::auth::ChatGptAuthError, Credential},
    config::{AccountConfig, AuthMode},
    routing::Route,
    server::AppState,
};

/// Whether the plan provider forwards the client's `max_tokens` as
/// `max_output_tokens`. OpenAI's preview-limitations page lists the parameter
/// as unsupported, but the 2026-10-30 probe showed it accepted and enforced.
/// Flip this to `false` if the preview starts rejecting it; it is a code
/// constant, not a config key (plan Non-Goals).
pub(super) const PLAN_FORWARDS_MAX_OUTPUT_TOKENS: bool = true;

/// Where a ChatGPT subscriber reads their plan usage.
pub(super) const USAGE_SETTINGS_URL: &str = "https://chatgpt.com/settings/usage";

/// How long a plan account whose credential could not be produced sits out.
const AUTH_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// The plan refresh store. Built per use, as codex builds its
/// `CodexAuthStore`: the store holds only its token URL, and its single-flight
/// lock is the process-wide one keyed by account file
/// (`refresh_lock_for`), so separate instances still serialize one account's
/// refreshes.
fn plan_store() -> ChatGptPlanAuthStore {
    ChatGptPlanAuthStore::from_env()
}

/// Whether `account` belongs to the chatgpt-plan pool.
pub(super) fn is_plan_account(account: &AccountConfig) -> bool {
    account.store_family == Some(accounts::StoreFamily::ChatgptPlan)
}

/// Apply the plan-only body switches to a translated upstream body.
pub(super) fn shape_upstream_body(body: &mut Value, auth: AuthMode) {
    shape_with(body, auth, PLAN_FORWARDS_MAX_OUTPUT_TOKENS);
}

fn shape_with(body: &mut Value, auth: AuthMode, forward_max_output_tokens: bool) {
    if auth != AuthMode::ChatgptPlanOauth || forward_max_output_tokens {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
    }
}

/// [`super::pool::resolve_or_cooldown`] for a plan account: a usable access
/// token, renewed first when due. `None` rotates; the account is then cooled
/// down, and a terminal failure (rejected grant, signed out, corrupt file)
/// also marks it `needs_relogin`.
pub(super) async fn resolve_plan_or_cooldown(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
) -> Option<Credential> {
    let resolved = match account.token_env.as_deref() {
        Some(token_env) => static_token(token_env),
        None if account.credentials.is_some() => Err(ChatGptAuthError::new(format!(
            "ChatGPT plan account {} sets `credentials`, which chatgpt-plan accounts do not \
             read; sign it in with `shunt login chatgpt-plan --name {}`",
            account.name, account.name
        ))),
        None => plan_store().get_valid(&account.name).await,
    };
    match resolved {
        Ok(access_token) => Some(Credential::ChatgptPlanOauth { access_token }),
        Err(error) => {
            cool_auth_failure(state, route, account, &error, "resolve");
            None
        }
    }
}

/// [`super::pool::force_refresh_or_cooldown`] for a plan account: upstream
/// rejected `credential` with a 401, so renew it if it is still the stored
/// token and return the replacement for one retry. A `token_env` account has
/// nothing to refresh, so it is cooled down and marked like codex's.
pub(super) async fn force_refresh_plan_or_cooldown(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    credential: &Credential,
) -> Option<Credential> {
    let Credential::ChatgptPlanOauth { access_token } = credential else {
        state
            .accounts
            .cooldown(&route.provider, account, AUTH_COOLDOWN, "auth");
        return None;
    };
    if account.token_env.is_some() {
        state
            .accounts
            .cooldown(&route.provider, account, AUTH_COOLDOWN, "auth");
        state.accounts.mark_needs_relogin(
            &route.provider,
            account,
            accounts::ReloginCause::ServedRequest,
        );
        tracing::warn!(
            provider = %route.provider,
            account = %account.name,
            "ChatGPT plan account returned 401 but its credential is not refreshable (token_env); cooling down"
        );
        return None;
    }
    match plan_store()
        .force_refresh_if_access_token(&account.name, access_token)
        .await
    {
        Ok(access_token) => Some(Credential::ChatgptPlanOauth { access_token }),
        Err(error) => {
            cool_auth_failure(state, route, account, &error, "force-refresh");
            None
        }
    }
}

fn static_token(token_env: &str) -> Result<String, ChatGptAuthError> {
    std::env::var(token_env)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ChatGptAuthError::new(format!("{token_env} is not set")))
}

/// The pool's shared auth-failure tail: a 5-minute cooldown, plus the
/// `needs_relogin` mark when no retry can recover the credential (AC-012).
fn cool_auth_failure(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    error: &ChatGptAuthError,
    stage: &'static str,
) {
    state
        .accounts
        .cooldown(&route.provider, account, AUTH_COOLDOWN, "auth");
    if error.is_terminal() {
        state.accounts.mark_needs_relogin(
            &route.provider,
            account,
            accounts::ReloginCause::RefreshGrant,
        );
    }
    tracing::warn!(
        provider = %route.provider,
        account = %account.name,
        stage,
        error = %error.detail,
        terminal = error.is_terminal(),
        "failed to {stage} ChatGPT plan account; cooling down"
    );
}

/// The gateway's addition to a plan provider's mapped upstream error message,
/// by status: where to look when the plan's usage is spent, and how to sign an
/// account back in.
pub(super) fn error_hint(status: StatusCode, auth: AuthMode) -> Option<String> {
    if auth != AuthMode::ChatgptPlanOauth {
        return None;
    }
    match status {
        StatusCode::TOO_MANY_REQUESTS => Some(format!(
            "ChatGPT plan usage limit reached; check your usage at {USAGE_SETTINGS_URL}"
        )),
        StatusCode::UNAUTHORIZED => {
            Some("run `shunt login chatgpt-plan --name <account>` to sign in again".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kill_switch_strips_max_output_tokens_only_for_the_plan() {
        let body = || json!({"model": "m", "max_output_tokens": 64, "stream": true});

        let mut kept = body();
        shape_with(&mut kept, AuthMode::ChatgptPlanOauth, true);
        assert_eq!(kept, body());

        let mut stripped = body();
        shape_with(&mut stripped, AuthMode::ChatgptPlanOauth, false);
        assert_eq!(stripped, json!({"model": "m", "stream": true}));

        for other in [AuthMode::ApiKey, AuthMode::ChatgptOauth, AuthMode::XaiOauth] {
            let mut untouched = body();
            shape_with(&mut untouched, other, false);
            assert_eq!(untouched, body(), "{other:?}");
        }
    }

    #[test]
    fn production_switch_forwards_max_output_tokens() {
        let mut body = json!({"max_output_tokens": 64});
        shape_upstream_body(&mut body, AuthMode::ChatgptPlanOauth);
        assert_eq!(body["max_output_tokens"], 64);
    }

    #[test]
    fn hints_name_usage_settings_and_the_login_command() {
        let usage = error_hint(StatusCode::TOO_MANY_REQUESTS, AuthMode::ChatgptPlanOauth).unwrap();
        assert!(usage.contains(USAGE_SETTINGS_URL), "{usage}");
        let login = error_hint(StatusCode::UNAUTHORIZED, AuthMode::ChatgptPlanOauth).unwrap();
        assert!(login.contains("shunt login chatgpt-plan --name"), "{login}");
        assert!(error_hint(StatusCode::FORBIDDEN, AuthMode::ChatgptPlanOauth).is_none());
        assert!(error_hint(StatusCode::TOO_MANY_REQUESTS, AuthMode::ChatgptOauth).is_none());
    }
}
