use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use serde::Deserialize;

use crate::{auth::shared::generate_pkce, server::AppState};

use super::{
    device::{auth_page, client_ip, device_page, normalize_user_code},
    idp_client,
};

#[derive(Default, Deserialize)]
pub struct AuthorizeQuery {
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
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(idp) = auth.oidc() else {
        return error_page(
            &auth,
            &query.user_code,
            "External sign-in is not configured.",
        );
    };
    if !check_rate_limit(&state, &auth, &headers, connection) {
        return error_page(
            &auth,
            &query.user_code,
            "Too many attempts. Wait a minute and try again.",
        );
    }
    let user_code = normalize_user_code(&query.user_code);
    if !state
        .gateway_stores
        .device_grants
        .pending_exists(&user_code)
    {
        return error_page(
            &auth,
            &user_code,
            "The device code is invalid, expired, or already used.",
        );
    }
    let endpoint = match idp_client::authorization_endpoint(&state, idp).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, "gateway: identity-provider discovery failed");
            return error_page(
                &auth,
                &user_code,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    let pkce = generate_pkce();
    if !state.gateway_stores.oidc_states.insert(
        pkce.state.clone(),
        user_code.clone(),
        pkce.verifier,
    ) {
        return error_page(
            &auth,
            &user_code,
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
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    location.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", idp.client_id.as_str()),
        ("redirect_uri", auth.url("/device/callback").as_str()),
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
    let Some(idp) = auth.oidc() else {
        return error_page(&auth, "", "External sign-in is not configured.");
    };
    if !check_rate_limit(&state, &auth, &headers, connection) {
        return error_page(&auth, "", "Too many attempts. Wait a minute and try again.");
    }
    if query.error.is_some() {
        return error_page(&auth, "", "The identity provider reported an error.");
    }
    let Some(pending) = state.gateway_stores.oidc_states.take(&query.state) else {
        return error_page(
            &auth,
            "",
            "This sign-in link is invalid or has expired. Start again from the device page.",
        );
    };
    if query.code.trim().is_empty() {
        return error_page(
            &auth,
            &pending.user_code,
            "The identity provider reported an error.",
        );
    }
    let redirect_uri = auth.url("/device/callback");
    let access_token =
        match idp_client::exchange_code(&state, idp, &query.code, &pending.verifier, &redirect_uri)
            .await
        {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "gateway: identity-provider token exchange failed");
                return error_page(
                    &auth,
                    &pending.user_code,
                    "Sign-in with the identity provider is unavailable right now.",
                );
            }
        };
    let identity = match idp_client::fetch_identity(&state, idp, &access_token).await {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(%error, "gateway: identity-provider userinfo request failed");
            return error_page(
                &auth,
                &pending.user_code,
                "Sign-in with the identity provider is unavailable right now.",
            );
        }
    };
    if !idp.email_allowed(&identity.email) {
        return error_page(
            &auth,
            &pending.user_code,
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

fn error_page(auth: &super::GatewayAuth, user_code: &str, message: &str) -> Response {
    device_page(auth_page(auth, user_code, Some(message), false))
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
