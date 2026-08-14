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
use crate::error::ShuntError;

/// Which credential slots a route accepts for the static-token check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slots {
    /// The configured header, `Authorization: Bearer`, and `x-api-key` — every
    /// slot the Anthropic client protocol can carry a gate token in.
    Client,
    /// The configured header and `Authorization: Bearer` only. The inbound
    /// Codex endpoint's posture: the Codex CLI never sends `x-api-key`.
    Bearer,
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
        Slots::Bearer => auth.authenticate_bearer(headers),
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
    // JWT credentials arrive in the `Authorization: Bearer` slot, which is
    // where Claude Code sends `ANTHROPIC_AUTH_TOKEN` — the whole point of the
    // design is that the client needs no shunt-specific configuration.
    let Some(token) = bearer_token(headers).and_then(|value| std::str::from_utf8(value).ok())
    else {
        return Outcome::Rejected;
    };
    match inbound_jwt::verify(auth.jwt(), jwks, token).await {
        JwtOutcome::Verified { identity, issuer } => {
            tracing::debug!(%issuer, "inbound JWT verified");
            Outcome::Authenticated {
                client: identity,
                static_token: false,
            }
        }
        JwtOutcome::Rejected => Outcome::Rejected,
        JwtOutcome::Unavailable => Outcome::Unavailable,
    }
}

/// The `503` for [`Outcome::Unavailable`]. `api_error` rather than
/// `overloaded_error`: shunt is not shedding load, a configured dependency is
/// unreachable.
pub fn unavailable_response() -> Response {
    ShuntError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "api_error",
        "cannot verify the presented credential: a configured JWT issuer's key set is unreachable",
    )
    .into_response()
}
