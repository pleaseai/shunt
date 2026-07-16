use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Form, Json,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{admin::session::random_id, server::AppState};

use super::{
    jwt,
    store::{DevicePoll, DEVICE_CODE_TTL, INITIAL_POLL_INTERVAL},
};

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const USER_CODE_CHARSET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

#[derive(Serialize)]
struct DiscoveryResponse {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
    grant_types_supported: [&'static str; 2],
    response_types_supported: [String; 0],
    token_endpoint_auth_methods_supported: [&'static str; 1],
    scopes_supported: [&'static str; 3],
    gateway_protocol_version: u8,
}

pub async fn discovery(State(state): State<AppState>) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(DiscoveryResponse {
        issuer: auth.public_url().to_string(),
        device_authorization_endpoint: auth.url("/oauth/device_authorization"),
        token_endpoint: auth.url("/oauth/token"),
        grant_types_supported: [DEVICE_GRANT, "refresh_token"],
        response_types_supported: [],
        token_endpoint_auth_methods_supported: ["none"],
        scopes_supported: ["openid", "profile", "email"],
        gateway_protocol_version: 1,
    })
    .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub struct DeviceAuthorizationForm {
    #[serde(default)]
    _client_id: String,
    #[serde(default)]
    _scope: String,
}

#[derive(Serialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

pub async fn device_authorization(
    State(state): State<AppState>,
    Form(_form): Form<DeviceAuthorizationForm>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let device_code = random_id();
    let user_code = unique_user_code(&state.gateway_stores.device_grants);
    state
        .gateway_stores
        .device_grants
        .create(device_code.clone(), user_code.clone());
    let verification_uri = auth.url("/device");
    let response = Json(DeviceAuthorizationResponse {
        device_code,
        verification_uri_complete: format!("{verification_uri}?user_code={user_code}"),
        verification_uri,
        user_code,
        expires_in: DEVICE_CODE_TTL.as_secs(),
        interval: INITIAL_POLL_INTERVAL.as_secs(),
    })
    .into_response();
    no_store(response)
}

#[derive(Debug, Default, Deserialize)]
pub struct TokenForm {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    device_code: String,
    #[serde(default)]
    refresh_token: String,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: u64,
}

pub async fn token(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response = match form.grant_type.as_str() {
        DEVICE_GRANT => match state.gateway_stores.device_grants.poll(&form.device_code) {
            DevicePoll::Pending => oauth_error(StatusCode::BAD_REQUEST, "authorization_pending"),
            DevicePoll::SlowDown => oauth_error(StatusCode::BAD_REQUEST, "slow_down"),
            DevicePoll::Denied => oauth_error(StatusCode::BAD_REQUEST, "access_denied"),
            DevicePoll::Expired => oauth_error(StatusCode::BAD_REQUEST, "expired_token"),
            DevicePoll::Approved(identity) => {
                state
                    .gateway_stores
                    .device_grants
                    .consume(&form.device_code);
                let refresh_token = state.gateway_stores.refresh_tokens.issue(identity.clone());
                token_response(&auth, &identity, refresh_token)
            }
        },
        "refresh_token" => match state
            .gateway_stores
            .refresh_tokens
            .rotate(&form.refresh_token)
        {
            Some((identity, refresh_token)) => token_response(&auth, &identity, refresh_token),
            None => oauth_error(StatusCode::UNAUTHORIZED, "invalid_grant"),
        },
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    };
    no_store(response)
}

fn token_response(
    auth: &super::GatewayAuth,
    identity: &super::approval::Identity,
    refresh_token: String,
) -> Response {
    Json(TokenResponse {
        access_token: jwt::mint(
            identity,
            auth.public_url(),
            auth.jwt_secret(),
            auth.token_ttl_seconds(),
        ),
        refresh_token,
        token_type: "Bearer",
        expires_in: auth.token_ttl_seconds(),
    })
    .into_response()
}

fn oauth_error(status: StatusCode, error: &'static str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn unique_user_code(store: &super::store::DeviceGrantStore) -> String {
    loop {
        let code = generate_user_code(&mut rand::rng());
        if store.user_code_available(&code) {
            return code;
        }
    }
}

fn generate_user_code(rng: &mut impl Rng) -> String {
    let mut code = String::with_capacity(9);
    for index in 0..8 {
        if index == 4 {
            code.push('-');
        }
        code.push(USER_CODE_CHARSET[rng.random_range(0..USER_CODE_CHARSET.len())] as char);
    }
    code
}

#[cfg(test)]
mod tests {
    use rand::{rngs::StdRng, SeedableRng};

    use super::{generate_user_code, USER_CODE_CHARSET};

    #[test]
    fn user_code_has_rfc8628_charset_and_format() {
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..100 {
            let code = generate_user_code(&mut rng);
            assert_eq!(code.len(), 9);
            assert_eq!(code.as_bytes()[4], b'-');
            assert!(code
                .bytes()
                .enumerate()
                .all(|(index, byte)| index == 4 || USER_CODE_CHARSET.contains(&byte)));
        }
    }
}
