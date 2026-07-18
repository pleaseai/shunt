use std::net::SocketAddr;

use axum::{
    extract::{rejection::FormRejection, ConnectInfo, Form, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use serde::Deserialize;

use crate::{auth::shared::generate_pkce, server::AppState};

use super::{
    device::{auth_page, client_ip, device_page, normalize_user_code, same_origin},
    idp_client,
};

#[derive(Default, Deserialize)]
pub struct AuthorizeForm {
    #[serde(default)]
    user_code: String,
}

#[derive(Default, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: Option<String>,
}

pub async fn authorize(
    State(state): State<AppState>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    form: Result<Form<AuthorizeForm>, FormRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let user_code = form
        .as_ref()
        .ok()
        .map(|Form(form)| form.user_code.as_str())
        .unwrap_or_default();
    if !same_origin(&headers, auth.public_url()) {
        return error_page(
            &auth,
            user_code,
            StatusCode::FORBIDDEN,
            "This request came from another site and was blocked.",
        );
    }
    let Form(form) = match form {
        Ok(form) => form,
        Err(_) => {
            return error_page(
                &auth,
                "",
                StatusCode::BAD_REQUEST,
                "The submitted form could not be read. Try again.",
            );
        }
    };
    let Some(idp) = auth.oidc_arc() else {
        return error_page(
            &auth,
            &form.user_code,
            StatusCode::BAD_GATEWAY,
            "External sign-in is not configured.",
        );
    };
    if !check_rate_limit(&state, &auth, &headers, connection) {
        return error_page(
            &auth,
            &form.user_code,
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Wait a minute and try again.",
        );
    }
    let user_code = normalize_user_code(&form.user_code);
    if !state
        .gateway_stores
        .device_grants
        .pending_exists(&user_code)
    {
        return error_page(
            &auth,
            &user_code,
            StatusCode::BAD_REQUEST,
            "The device code is invalid, expired, or already used.",
        );
    }
    let endpoint = match idp_client::authorization_endpoint(&state, &idp).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, "gateway: identity-provider discovery failed");
            return error_page(
                &auth,
                &user_code,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let redirect_uri = auth.url("/device/callback");
    let pkce = generate_pkce();
    if !state.gateway_stores.oidc_states.insert(
        pkce.state.clone(),
        user_code.clone(),
        pkce.verifier,
        idp.clone(),
        redirect_uri.clone(),
    ) {
        return error_page(
            &auth,
            &user_code,
            StatusCode::BAD_GATEWAY,
            "Sign-in with the identity provider is unavailable right now.",
        );
    }
    let mut location = match reqwest::Url::parse(&endpoint) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(%error, "gateway: identity-provider authorization endpoint is invalid");
            return error_page(
                &auth,
                &user_code,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    location.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", idp.client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", idp.scopes.join(" ").as_str()),
        ("state", pkce.state.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    redirect(location)
}

pub async fn callback(
    State(state): State<AppState>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !check_rate_limit(&state, &auth, &headers, connection) {
        return error_page(
            &auth,
            "",
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Wait a minute and try again.",
        );
    }
    let Some(pending) = state.gateway_stores.oidc_states.take(&query.state) else {
        return error_page(
            &auth,
            "",
            StatusCode::BAD_REQUEST,
            "This sign-in link is invalid or has expired. Start again from the device page.",
        );
    };
    if let Some(provider_error) = query.error.as_deref() {
        let provider_error = sanitized_provider_error(provider_error);
        tracing::warn!(
            provider_error,
            "gateway: identity provider rejected authorization"
        );
        state.gateway_stores.device_grants.deny(&pending.user_code);
        return error_page(
            &auth,
            &pending.user_code,
            StatusCode::BAD_REQUEST,
            "The identity provider reported an error.",
        );
    }
    if query.code.trim().is_empty() {
        return error_page(
            &auth,
            &pending.user_code,
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
            tracing::warn!(%error, "gateway: identity-provider token exchange failed");
            return error_page(
                &auth,
                &pending.user_code,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let identity = match idp_client::fetch_identity(&state, &pending.idp, &access_token).await {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(%error, "gateway: identity-provider userinfo request failed");
            return error_page(
                &auth,
                &pending.user_code,
                StatusCode::BAD_GATEWAY,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let Some(current_idp) = auth.oidc() else {
        return error_page(
            &auth,
            &pending.user_code,
            StatusCode::FORBIDDEN,
            "This account is not authorized for this gateway.",
        );
    };
    if !current_idp.email_allowed(&identity.email) {
        return error_page(
            &auth,
            &pending.user_code,
            StatusCode::FORBIDDEN,
            "This account is not authorized for this gateway.",
        );
    }
    if !state
        .gateway_stores
        .device_grants
        .approve(&pending.user_code, identity)
    {
        return error_page(
            &auth,
            &pending.user_code,
            StatusCode::BAD_REQUEST,
            "The device code is invalid, expired, or already used.",
        );
    }
    device_page(auth_page(
        &auth,
        &pending.user_code,
        Some("Device approved. You can return to your device."),
        true,
    ))
}

fn check_rate_limit(
    state: &AppState,
    auth: &super::GatewayAuth,
    headers: &HeaderMap,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> bool {
    let peer = connection.map(|Extension(ConnectInfo(address))| address);
    let ip = client_ip(headers, peer, auth.trust_forwarded_for());
    state.gateway_stores.device_verify_rate.check(&ip)
}

fn error_page(
    auth: &super::GatewayAuth,
    user_code: &str,
    status: StatusCode,
    message: &str,
) -> Response {
    let mut response = device_page(auth_page(auth, user_code, Some(message), false));
    *response.status_mut() = status;
    response
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

fn redirect(location: reqwest::Url) -> Response {
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
