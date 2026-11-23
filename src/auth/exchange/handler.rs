//! `POST /v1/oauth/token`: trade a verified external JWT for a shunt-issued
//! exchange token (RFC 7523 jwt-bearer grant, JSON body, as the AWF api-proxy
//! sidecar sends it).
//!
//! Errors are OAuth-shaped (`error`, `error_description`) rather than
//! Anthropic-shaped, because the sidecar parses them as OAuth errors. Every
//! `error_description` is a fixed string per error code: it never names the
//! check that failed (NFR-3) and never echoes the assertion (NFR-2).

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{fingerprint, mint, unix_now, ExchangeSettings};
use crate::auth::inbound::InboundAuth;
use crate::auth::inbound_jwt::{self, JwtOutcome};
use crate::gateway::oauth::no_store;
use crate::gateway::rate_limit::{PerIpRateLimiter, RetryAfter};
use crate::server::AppState;

const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// An assertion is one JWT; anything near this size is not a legitimate
/// request, so it is refused before it is parsed.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub(crate) async fn post(
    State(state): State<AppState>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // The route exists from boot, but a reload may have removed the table
    // since: answer exactly as an unregistered route would. Nothing is counted.
    let state = state.refreshed();
    let Some((auth, settings)) = state
        .inbound_auth
        .as_deref()
        .and_then(|auth| auth.exchange().map(|settings| (auth, settings)))
    else {
        return no_store(StatusCode::NOT_FOUND.into_response());
    };

    let peer = connection.map(|Extension(ConnectInfo(address))| address);
    let key = bounded_key(crate::gateway::device::client_ip(
        &headers,
        peer,
        state.config.server.access_control.trust_forwarded_for,
    ));
    let failures = &state.gateway_stores.exchange_failure_rate;
    // An address over its failure limit is refused before the body is read or
    // any key set is fetched, so a blocked caller costs no verification work.
    if let Err(retry_after) = failures.peek(&key) {
        return slow_down(retry_after);
    }

    let assertion = match read_assertion(&headers, body).await {
        Ok(assertion) => assertion,
        Err(code) => return count_failure(failures, &key, code),
    };
    match exchange(&state, auth, settings, &assertion).await {
        Ok(response) => response,
        // An unreachable key set says nothing about the caller: not counted.
        Err(ErrorCode::TemporarilyUnavailable) => oauth_error(ErrorCode::TemporarilyUnavailable),
        Err(code) => count_failure(failures, &key, code),
    }
}

/// Longest address kept verbatim as a limiter key. A trusted forwarded header is
/// caller-controlled and unbounded, so a longer value is replaced by its digest
/// (still one key per distinct value, at a fixed size). `/device` is unchanged.
const MAX_ADDRESS_KEY_BYTES: usize = 64;

fn bounded_key(address: String) -> String {
    if address.len() <= MAX_ADDRESS_KEY_BYTES {
        return address;
    }
    URL_SAFE_NO_PAD.encode(Sha256::digest(address.as_bytes()))
}

/// Record one failed exchange for `key` and answer `code`, or answer
/// `slow_down` when the limiter refuses to track the address.
fn count_failure(failures: &PerIpRateLimiter, key: &str, code: ErrorCode) -> Response {
    match failures.record_failure(key) {
        Ok(()) => oauth_error(code),
        Err(retry_after) => slow_down(retry_after),
    }
}

/// Limiter key for one identity under one rule: a digest, so the stored key has
/// a fixed size whatever the identity's length. The fingerprint is base64url
/// of fixed length, so the NUL separator cannot make two pairs collide.
fn identity_key(fingerprint: &str, identity: &str) -> String {
    let digest = Sha256::new()
        .chain_update(fingerprint.as_bytes())
        .chain_update([0])
        .chain_update(identity.as_bytes())
        .finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

/// Check the content type, read the bounded body and parse the assertion.
async fn read_assertion(headers: &HeaderMap, body: Body) -> Result<String, ErrorCode> {
    if !is_json(headers) {
        return Err(ErrorCode::InvalidRequest);
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ErrorCode::InvalidRequest)?;
    parse_request(&bytes)
}

/// Verify `assertion` against the opted-in rules and mint the token response.
///
/// One `verify` call per opted-in rule: `Verified` names only the issuer,
/// which cannot tell two entries on one issuer apart, and the minted token is
/// bound to the exact entry that accepted the assertion. Only opted-in rules
/// are tried, so static tokens, gateway sessions and earlier exchange tokens
/// fall out as ordinary rejections.
async fn exchange(
    state: &AppState,
    auth: &InboundAuth,
    settings: &ExchangeSettings,
    assertion: &str,
) -> Result<Response, ErrorCode> {
    let mut unavailable = false;
    for rule in auth.jwt().iter().filter(|rule| rule.exchange) {
        match inbound_jwt::verify(std::slice::from_ref(rule), &state.inbound_jwks, assertion).await
        {
            JwtOutcome::Verified { identity, .. } => {
                let fingerprint = fingerprint(rule);
                // Counted before minting, so a refused request issues nothing. It
                // is a throttle, not a failed assertion, so the address's failure
                // count is left alone (the `Ok` arm in `post` records nothing).
                let admitted = state
                    .gateway_stores
                    .exchange_identity_rate
                    .admit(&identity_key(&fingerprint, &identity));
                if let Err(retry_after) = admitted {
                    tracing::info!(outcome = "slow_down", identity = %identity, "workload exchange");
                    return Ok(slow_down(retry_after));
                }
                let minted = mint(settings, &identity, &fingerprint, unix_now());
                tracing::info!(outcome = "issued", identity = %identity, "workload exchange");
                return Ok(no_store(
                    Json(json!({
                        "access_token": minted.token,
                        "token_type": "Bearer",
                        "expires_in": minted.expires_in,
                    }))
                    .into_response(),
                ));
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
    Err(code)
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
    SlowDown,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedGrantType => "unsupported_grant_type",
            Self::InvalidGrant => "invalid_grant",
            Self::TemporarilyUnavailable => "temporarily_unavailable",
            Self::SlowDown => "slow_down",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest | Self::UnsupportedGrantType => StatusCode::BAD_REQUEST,
            Self::InvalidGrant => StatusCode::UNAUTHORIZED,
            Self::TemporarilyUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::SlowDown => StatusCode::TOO_MANY_REQUESTS,
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
            Self::SlowDown => "Too many requests. Wait before retrying.",
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

/// The `slow_down` answer with the address's remaining wait as `Retry-After`.
fn slow_down(retry_after: RetryAfter) -> Response {
    let mut response = oauth_error(ErrorCode::SlowDown);
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from(retry_after.seconds()),
    );
    response
}
