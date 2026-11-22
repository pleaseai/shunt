//! `POST /v1/oauth/token`: trade a verified external JWT for a shunt-issued
//! exchange token (RFC 7523 jwt-bearer grant, JSON body, as the AWF api-proxy
//! sidecar sends it).
//!
//! Errors are OAuth-shaped (`error`, `error_description`) rather than
//! Anthropic-shaped, because the sidecar parses them as OAuth errors. Every
//! `error_description` is a fixed string per error code: it never names the
//! check that failed (NFR-3) and never echoes the assertion (NFR-2).

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::{fingerprint, mint, unix_now};
use crate::auth::inbound_jwt::{self, JwtOutcome};
use crate::gateway::oauth::no_store;
use crate::server::AppState;

const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// An assertion is one JWT; anything near this size is not a legitimate
/// request, so it is refused before it is parsed.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub(crate) async fn post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // The route exists from boot, but a reload may have removed the table
    // since: answer exactly as an unregistered route would.
    let state = state.refreshed();
    let Some((auth, settings)) = state
        .inbound_auth
        .as_deref()
        .and_then(|auth| auth.exchange().map(|settings| (auth, settings)))
    else {
        return no_store(StatusCode::NOT_FOUND.into_response());
    };

    if !is_json(&headers) {
        return oauth_error(ErrorCode::InvalidRequest);
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return oauth_error(ErrorCode::InvalidRequest);
    };
    let assertion = match parse_request(&bytes) {
        Ok(assertion) => assertion,
        Err(code) => return oauth_error(code),
    };

    // One `verify` call per opted-in rule: `Verified` names only the issuer,
    // which cannot tell two entries on one issuer apart, and the minted token
    // is bound to the exact entry that accepted the assertion. Only opted-in
    // rules are tried, so static tokens, gateway sessions and earlier exchange
    // tokens fall out as ordinary rejections.
    let mut unavailable = false;
    for rule in auth.jwt().iter().filter(|rule| rule.exchange) {
        match inbound_jwt::verify(std::slice::from_ref(rule), &state.inbound_jwks, &assertion).await
        {
            JwtOutcome::Verified { identity, .. } => {
                let minted = mint(settings, &identity, &fingerprint(rule), unix_now());
                tracing::info!(outcome = "issued", identity = %identity, "workload exchange");
                return no_store(
                    Json(json!({
                        "access_token": minted.token,
                        "token_type": "Bearer",
                        "expires_in": minted.expires_in,
                    }))
                    .into_response(),
                );
            }
            JwtOutcome::Unavailable => unavailable = true,
            JwtOutcome::Rejected => {}
        }
    }
    let code = if unavailable {
        ErrorCode::TemporarilyUnavailable
    } else {
        ErrorCode::InvalidGrant
    };
    tracing::info!(outcome = code.as_str(), "workload exchange");
    oauth_error(code)
}

/// Whether `Content-Type` is `application/json` or a `+json` type.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let essence = value
        .split_once(';')
        .map_or(value, |(essence, _)| essence)
        .trim();
    essence.eq_ignore_ascii_case("application/json")
        || essence
            .len()
            .checked_sub("+json".len())
            .and_then(|start| essence.get(start..))
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
}

/// Extract the assertion from a request body. The federation ids and any other
/// field are ignored, whatever their type.
///
/// Precedence when several things are wrong: a body that is not a JSON object,
/// or whose `grant_type` / `assertion` has the wrong JSON type, is
/// `invalid_request`; then a missing or different `grant_type` is
/// `unsupported_grant_type`; then a missing or blank `assertion` is
/// `invalid_request`.
fn parse_request(bytes: &[u8]) -> Result<String, ErrorCode> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(bytes) else {
        return Err(ErrorCode::InvalidRequest);
    };
    let string_field = |name: &str| match fields.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(ErrorCode::InvalidRequest),
    };
    let grant_type = string_field("grant_type")?;
    let assertion = string_field("assertion")?;
    if grant_type != Some(JWT_BEARER_GRANT) {
        return Err(ErrorCode::UnsupportedGrantType);
    }
    match assertion.map(str::trim) {
        Some(assertion) if !assertion.is_empty() => Ok(assertion.to_string()),
        _ => Err(ErrorCode::InvalidRequest),
    }
}

#[derive(Clone, Copy)]
enum ErrorCode {
    InvalidRequest,
    UnsupportedGrantType,
    InvalidGrant,
    TemporarilyUnavailable,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedGrantType => "unsupported_grant_type",
            Self::InvalidGrant => "invalid_grant",
            Self::TemporarilyUnavailable => "temporarily_unavailable",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest | Self::UnsupportedGrantType => StatusCode::BAD_REQUEST,
            Self::InvalidGrant => StatusCode::UNAUTHORIZED,
            Self::TemporarilyUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Fixed per code. `invalid_grant` is shared by every verification
    /// failure, so it must not say which check rejected the assertion.
    fn description(self) -> &'static str {
        match self {
            Self::InvalidRequest => "The request body is malformed or incomplete.",
            Self::UnsupportedGrantType => "Only the jwt-bearer grant type is supported.",
            Self::InvalidGrant => "The assertion is not valid.",
            Self::TemporarilyUnavailable => "The assertion cannot be verified right now.",
        }
    }
}

fn oauth_error(code: ErrorCode) -> Response {
    no_store(
        (
            code.status(),
            Json(json!({
                "error": code.as_str(),
                "error_description": code.description(),
            })),
        )
            .into_response(),
    )
}
