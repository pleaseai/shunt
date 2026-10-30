//! The one inbound-credential gate every gated route calls.
//!
//! `[server.auth]` can now accept two kinds of credential — a static
//! `name:token` and a JWT minted by an external issuer (`[[server.auth.jwt]]`,
//! issue #344) — and a deployment may configure either or both. Resolving that
//! in each handler would mean repeating the precedence rules six times, so it
//! lives here: static token first (a constant-time compare, no network), then
//! JWT entries by `iss`.
//!
//! The gateway session token (`[server.gateway]`) is checked separately by the
//! handlers that accept it, because it resolves to gateway claims rather than a
//! client name and several routes act on those claims.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::inbound::{bearer_token, InboundAuth};
use crate::auth::inbound_jwt::{self, JwksCache, JwtOutcome};
use crate::auth::jwt_slots::{
    cookie_credential, cookie_slot_allowed, header_credential, Presented,
};
use crate::error::ShuntError;

/// Which credential slots a route accepts for the static-token check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slots {
    /// The configured header, `Authorization: Bearer`, and `x-api-key` — every
    /// slot the Anthropic client protocol can carry a gate token in.
    Client,
    /// The inbound Codex endpoint's posture: the configured header and
    /// `Authorization: Bearer` for a static token (the Codex CLI never sends
    /// `x-api-key`), then a JWT from `Authorization: Bearer`, the
    /// `[server.auth] jwt_header` slot, and the `jwt_cookie` slot. `Client`
    /// never reads the `jwt_header` / `jwt_cookie` slots (only the bearer JWT).
    Codex,
}

/// What the presented credentials resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Authenticated {
        /// The caller identity: the static token's configured name, or the
        /// verified JWT's email.
        client: String,
        /// Whether a static `[server.auth]` token matched. Routes that treat
        /// the two differently (the inbound Codex endpoint namespaces its
        /// account-pool sticky key by client) can still tell them apart.
        static_token: bool,
    },
    /// No credential matched. Every failure reason collapses here so a `401`
    /// never discloses which check failed.
    Rejected,
    /// A configured issuer's JWKS could not be fetched, so no verdict was
    /// possible. Distinct from [`Self::Rejected`]: answering `401` would tell
    /// an operator their credential is wrong when their IdP is unreachable.
    Unavailable,
}

/// Resolve the inbound credential across both `[server.auth]` mechanisms.
///
/// The static check runs first and short-circuits, so a deployment with no JWT
/// entries never touches the async path and behaves exactly as before.
pub async fn authenticate(
    auth: &InboundAuth,
    jwks: &JwksCache,
    headers: &HeaderMap,
    slots: Slots,
) -> Outcome {
    let static_client = match slots {
        Slots::Client => auth.authenticate_client(headers),
        Slots::Codex => auth.authenticate_bearer(headers),
    };
    if let Some(client) = static_client {
        return Outcome::Authenticated {
            client: client.to_string(),
            static_token: true,
        };
    }
    if auth.jwt().is_empty() {
        return Outcome::Rejected;
    }
    let mut presented = vec![bearer_credential(headers)];
    if slots == Slots::Codex {
        presented.extend(
            auth.jwt_header()
                .map(|name| header_credential(headers, name)),
        );
        // A cookie rides along on cross-origin browser requests, so the slot
        // is consulted only when `Origin` does not rule it out (FR-12).
        if cookie_slot_allowed(headers) {
            presented.extend(
                auth.jwt_cookie()
                    .map(|name| cookie_credential(headers, name)),
            );
        }
    }
    verify_slots(auth, jwks, presented).await
}

/// `Authorization: Bearer` is where Claude Code sends `ANTHROPIC_AUTH_TOKEN`,
/// so a JWT arrives there with no shunt-specific client configuration. Only the
/// first `Authorization` value is read, as on the static path.
fn bearer_credential(headers: &HeaderMap) -> Presented<'_> {
    match bearer_token(headers) {
        None => Presented::Absent,
        Some(value) => std::str::from_utf8(value).map_or(Presented::Unreadable, Presented::One),
    }
}

/// Verify the JWT slots lazily, in order. The first `Verified` wins; a slot is
/// examined only when the scan reaches it, so an earlier credential that
/// authenticates wins even when a later slot is repeated. A repeated slot ends
/// the scan, and no slot verifying is `Unavailable` rather than `Rejected`
/// when any consulted issuer was unreachable, so a `401` never blames the
/// credential for an IdP outage.
async fn verify_slots(
    auth: &InboundAuth,
    jwks: &JwksCache,
    presented: Vec<Presented<'_>>,
) -> Outcome {
    let mut unavailable = false;
    for slot in presented {
        let token = match slot {
            Presented::Absent | Presented::Unreadable => continue,
            Presented::Repeated => break,
            Presented::One(token) => token,
        };
        match inbound_jwt::verify(auth.jwt(), jwks, token).await {
            JwtOutcome::Verified { identity, issuer } => {
                tracing::debug!(%issuer, "inbound JWT verified");
                return Outcome::Authenticated {
                    client: identity,
                    static_token: false,
                };
            }
            JwtOutcome::Rejected => {}
            JwtOutcome::Unavailable => unavailable = true,
        }
    }
    if unavailable {
        Outcome::Unavailable
    } else {
        Outcome::Rejected
    }
}

/// The `503` for [`Outcome::Unavailable`]. `api_error` rather than
/// `overloaded_error`: shunt is not shedding load, a configured dependency is
/// unreachable.
pub fn unavailable_error() -> ShuntError {
    ShuntError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "api_error",
        "cannot verify the presented credential: a configured JWT issuer's key set is unreachable",
    )
}

/// [`unavailable_error`] as a response, for handlers that return one directly.
pub fn unavailable_response() -> Response {
    unavailable_error().into_response()
}
