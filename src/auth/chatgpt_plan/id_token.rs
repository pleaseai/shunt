//! Verification of the ID token returned by the Sign in with ChatGPT code
//! exchange.
//!
//! The check is [`inbound_jwt::verify`] with a rule built per sign-in attempt:
//! the issuer's JWKS signs the token, `aud` must contain the client id the
//! authorization issued, and the attempt's `nonce` must come back unchanged.
//! Nothing here parses a signature or a claim by hand; the only claim read
//! directly is `email`, and only after `verify` accepted the token.

use jsonwebtoken::Algorithm;
use thiserror::Error;

use crate::auth::inbound_jwt::{self, JwksCache, JwtIssuerRule, JwtOutcome};
use crate::auth::shared::jwt_claims;

/// The issuer of the SIWC ID token. Tests pass a loopback mock instead; the
/// issuer is never read from the environment.
pub const OPENAI_ISSUER: &str = "https://auth.openai.com";

/// Tolerance on `exp` and `nbf`. `jsonwebtoken` does not check `iat`, so a
/// clock that lags the issuer by a few seconds does not reject a fresh token.
const CLOCK_SKEW_SECONDS: u64 = 60;

/// OpenAI issues ID tokens that live 3600 s; the cap sits well above that so it
/// bounds a pathological token without ever rejecting a real one.
const MAX_TOKEN_AGE_SECONDS: u64 = 86_400;

/// The identity an accepted ID token carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    pub sub: String,
    pub email: Option<String>,
}

/// Why an ID token was not accepted. Neither variant carries the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IdTokenError {
    #[error("the ChatGPT ID token failed validation")]
    Rejected,
    #[error("could not reach OpenAI's signing keys; try again")]
    KeysUnavailable,
}

/// The per-attempt rule: one issuer, one audience, one nonce, RS256 only.
fn rule(issuer: &str, client_id: &str, nonce: &str) -> JwtIssuerRule {
    JwtIssuerRule {
        issuer: issuer.to_string(),
        jwks_url: None,
        audience: vec![client_id.to_string()],
        algorithms: vec![Algorithm::RS256],
        // OpenAI's token has no `azp`; `verify` checks it only when present.
        authorized_parties: Vec::new(),
        allowed_domains: Vec::new(),
        allowed_emails: Vec::new(),
        require: vec![("nonce".to_string(), vec![nonce.to_string()])],
        subject_prefixes: Vec::new(),
        identity_claim: "sub".to_string(),
        clock_skew_seconds: CLOCK_SKEW_SECONDS,
        max_token_age_seconds: MAX_TOKEN_AGE_SECONDS,
    }
}

/// Verify `token` for the sign-in attempt that issued `client_id` and `nonce`.
pub async fn verify_id_token(
    cache: &JwksCache,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    token: &str,
) -> Result<VerifiedIdentity, IdTokenError> {
    let rules = [rule(issuer, client_id, nonce)];
    match inbound_jwt::verify(&rules, cache, token).await {
        JwtOutcome::Verified { identity, .. } => {
            let email = jwt_claims(token)
                .and_then(|claims| claims.get("email")?.as_str().map(str::to_string))
                .filter(|email| !email.is_empty());
            Ok(VerifiedIdentity {
                sub: identity,
                email,
            })
        }
        JwtOutcome::Rejected => Err(IdTokenError::Rejected),
        JwtOutcome::Unavailable => Err(IdTokenError::KeysUnavailable),
    }
}

#[cfg(test)]
pub(crate) mod tests;
