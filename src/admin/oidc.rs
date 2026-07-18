use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{auth::shared::generate_pkce, gateway::idp_client, server::AppState};

use super::{login_response, not_found, same_origin, secure_cookie, set_cookie};

#[derive(Default, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: Option<String>,
}

pub async fn start(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.admin_auth.clone() else {
        return not_found();
    };
    if !same_origin(&headers) {
        return error_page(
            &auth,
            StatusCode::FORBIDDEN,
            "This request came from another site and was blocked.",
        );
    }
    if !state.admin_stores.login_rate.check() {
        return error_page(
            &auth,
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Wait a minute and try again.",
        );
    }
    let Some(idp) = auth.oidc_arc() else {
        return error_page(
            &auth,
            StatusCode::BAD_GATEWAY,
            "External sign-in is not configured.",
        );
    };
    let Some(redirect_uri) = auth.oidc_callback_url() else {
        return error_page(
            &auth,
            StatusCode::BAD_GATEWAY,
            "External sign-in is not configured.",
        );
    };
    let endpoint = match idp_client::authorization_endpoint(&state, &idp).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, "admin: identity-provider discovery failed");
            return error_page(
                &auth,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let pkce = generate_pkce();
    if !state.admin_stores.oidc_states.insert(
        pkce.state.clone(),
        pkce.verifier,
        idp.clone(),
        redirect_uri.clone(),
        auth.pending_ttl(),
    ) {
        tracing::warn!("admin: OIDC state store is full or state collision occurred");
        return error_page(
            &auth,
            StatusCode::BAD_GATEWAY,
            "Sign-in with the identity provider is unavailable right now.",
        );
    }
    let Some(location) =
        idp_client::authorization_url(&endpoint, &idp, &redirect_uri, &pkce.state, &pkce.challenge)
    else {
        tracing::warn!("admin: identity-provider authorization endpoint is invalid");
        return error_page(
            &auth,
            StatusCode::BAD_GATEWAY,
            "Sign-in with the identity provider is unavailable right now.",
        );
    };
    redirect_to_idp(location)
}

pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.admin_auth.clone() else {
        return not_found();
    };
    if !state.admin_stores.login_rate.check() {
        return error_page(
            &auth,
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Wait a minute and try again.",
        );
    }
    let Some(pending) = state.admin_stores.oidc_states.take(&query.state) else {
        return error_page(
            &auth,
            StatusCode::BAD_REQUEST,
            "This sign-in link is invalid or has expired. Start again from the login page.",
        );
    };
    if let Some(provider_error) = query.error.as_deref() {
        let provider_error = sanitized_provider_error(provider_error);
        tracing::warn!(
            provider_error,
            "admin: identity provider rejected authorization"
        );
        return error_page(
            &auth,
            StatusCode::BAD_REQUEST,
            "The identity provider reported an error.",
        );
    }
    if query.code.trim().is_empty() {
        return error_page(
            &auth,
            StatusCode::BAD_REQUEST,
            "The identity provider reported an error.",
        );
    }
    let access_token = match idp_client::exchange_code(
        &state,
        &pending.idp,
        &query.code,
        &pending.verifier,
        &pending.redirect_uri,
    )
    .await
    {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!(%error, "admin: identity-provider token exchange failed");
            return error_page(
                &auth,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let identity = match idp_client::fetch_identity(&state, &pending.idp, &access_token).await {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(%error, "admin: identity-provider userinfo request failed");
            return error_page(
                &auth,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let Some(current_idp) = auth.oidc() else {
        return error_page(
            &auth,
            StatusCode::FORBIDDEN,
            "This account is not authorized for this admin surface.",
        );
    };
    if !current_idp.email_allowed(&identity.email) {
        return error_page(
            &auth,
            StatusCode::FORBIDDEN,
            "This account is not authorized for this admin surface.",
        );
    }
    let (sid, _csrf) = state.admin_stores.sessions.create(auth.session_ttl());
    let cookie = set_cookie(&sid, secure_cookie(&headers), auth.session_ttl());
    tracing::info!("admin: OIDC browser session created");
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/admin".to_string()),
        ],
    )
        .into_response()
}

fn error_page(auth: &super::AdminAuth, status: StatusCode, message: &str) -> Response {
    let label = auth.oidc().map(crate::gateway::ResolvedIdp::button_label);
    login_response(status, Some(message), label)
}

fn sanitized_provider_error(error: &str) -> &str {
    if !error.is_empty()
        && error.len() <= 64
        && error
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        error
    } else {
        "invalid_provider_error"
    }
}

fn redirect_to_idp(location: reqwest::Url) -> Response {
    let Ok(location) = HeaderValue::from_str(location.as_str()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response()
}
