//! Workload token exchange (`[server.auth.exchange]`): shunt-issued,
//! short-lived tokens traded for a verified external assertion.
//!
//! A token is an HS256 JWT (see [`super::hs256`]) bound to the *entry* that
//! verified the assertion, not to its position in the config: it carries the
//! entry's [`fingerprint`], and [`verify`] accepts it only while an
//! `exchange = true` entry with that exact fingerprint is still configured.
//! Removing the entry, clearing its opt-in, or editing any rule that decides
//! who may authenticate therefore revokes every token it issued, without
//! shunt keeping any revocation state.

pub(crate) mod handler;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{hs256, inbound::InboundAuth, inbound_jwt::JwtIssuerRule};

/// The `iss` of every exchange token. A `[[server.auth.jwt]]` entry may not
/// claim it (config validation), so the exchange verifier and an external
/// issuer can never be confused for each other.
pub(crate) const EXCHANGE_ISSUER: &str = "urn:shunt:workload-exchange";

/// `aud` of every exchange token. Differs from the gateway session audience
/// (`"shunt"`), so neither family's verifier accepts the other's tokens.
const EXCHANGE_AUDIENCE: &str = "shunt-exchange";
/// The dedicated shunt marker claim, shared in name with gateway sessions.
const MARKER_CLAIM: &str = "shunt_token_use";
/// Marker value identifying an exchange token.
const MARKER_VALUE: &str = "workload-exchange";
/// Leading tag of the fingerprint encoding. Bump it when the covered fields
/// change, which revokes every outstanding token on purpose.
const FINGERPRINT_VERSION: &str = "shunt-exchange-entry-v1";

/// Resolved `[server.auth.exchange]`: the ordered signing secrets (index 0
/// signs, every entry verifies) and the issued-token lifetime.
#[derive(Clone)]
pub struct ExchangeSettings {
    secrets: Vec<Vec<u8>>,
    max_ttl_seconds: u64,
}

impl ExchangeSettings {
    pub(crate) fn new(secrets: Vec<Vec<u8>>, max_ttl_seconds: u64) -> Self {
        assert!(
            !secrets.is_empty(),
            "ExchangeSettings requires at least one signing secret"
        );
        Self {
            secrets,
            max_ttl_seconds,
        }
    }

    /// The secret new tokens are signed with.
    pub(crate) fn signing_secret(&self) -> &[u8] {
        &self.secrets[0]
    }

    /// Every secret a presented token may verify under.
    pub(crate) fn verifying_secrets(&self) -> &[Vec<u8>] {
        &self.secrets
    }

    pub fn max_ttl_seconds(&self) -> u64 {
        self.max_ttl_seconds
    }
}

impl std::fmt::Debug for ExchangeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangeSettings")
            .field(
                "secrets",
                &format_args!("[redacted; {}]", self.secrets.len()),
            )
            .field("max_ttl_seconds", &self.max_ttl_seconds)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    iat: u64,
    exp: u64,
    /// Entry fingerprint (see [`fingerprint`]).
    ent: String,
    #[serde(default)]
    shunt_token_use: String,
}

/// A freshly minted exchange token.
pub(crate) struct Minted {
    pub(crate) token: String,
    pub(crate) expires_in: u64,
}

/// Stable identity of the verification rules in `rule`: a SHA-256 over a
/// versioned, field-ordered JSON array, so no field can bleed into another.
///
/// Covers every field that decides who may authenticate: issuer, audience,
/// algorithms, authorized parties, email rules, `require`, subject prefixes
/// and the identity claim. List members are sorted because their order has no
/// meaning. It leaves out `jwks_url` (key location, not policy),
/// `clock_skew_seconds` and `max_token_age_seconds` (they bound the
/// assertion's freshness at exchange time only, so tuning them must not
/// revoke tokens) and the `exchange` flag (checked separately).
pub(crate) fn fingerprint(rule: &JwtIssuerRule) -> String {
    fn sorted<T: Ord + Clone>(values: &[T]) -> Vec<T> {
        let mut values = values.to_vec();
        values.sort();
        values
    }
    let algorithms: Vec<String> = rule.algorithms.iter().map(|a| format!("{a:?}")).collect();
    let require: Vec<(&String, Vec<String>)> = rule
        .require
        .iter()
        .map(|(claim, values)| (claim, sorted(values)))
        .collect();
    let encoded = serde_json::to_vec(&(
        FINGERPRINT_VERSION,
        &rule.issuer,
        sorted(&rule.audience),
        sorted(&algorithms),
        sorted(&rule.authorized_parties),
        sorted(&rule.allowed_domains),
        sorted(&rule.allowed_emails),
        require,
        sorted(&rule.subject_prefixes),
        &rule.identity_claim,
    ))
    .expect("fingerprint input serializes");
    URL_SAFE_NO_PAD.encode(Sha256::digest(encoded))
}

/// Seconds since the Unix epoch; `0` if the clock is before it.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Mint an exchange token for `identity`, bound to `entry_fingerprint` and
/// valid for `max_ttl_seconds` from `now`.
pub(crate) fn mint(
    settings: &ExchangeSettings,
    identity: &str,
    entry_fingerprint: &str,
    now: u64,
) -> Minted {
    let expires_in = settings.max_ttl_seconds();
    let claims = Claims {
        iss: EXCHANGE_ISSUER.to_string(),
        aud: EXCHANGE_AUDIENCE.to_string(),
        sub: identity.to_string(),
        iat: now,
        exp: now.saturating_add(expires_in),
        ent: entry_fingerprint.to_string(),
        shunt_token_use: MARKER_VALUE.to_string(),
    };
    let token = hs256::sign_token(
        &serde_json::to_vec(&claims).expect("exchange claims serialize"),
        settings.signing_secret(),
    );
    Minted { token, expires_in }
}

/// Verify `token` against the current `auth` snapshot and return the
/// identity it carries. Accepts only a token signed under a listed secret,
/// still unexpired, whose entry fingerprint matches an `exchange = true` rule
/// in this snapshot. `None` when exchange is not configured or on any
/// malformed input.
pub(crate) fn verify(auth: &InboundAuth, token: &str, now: u64) -> Option<String> {
    let settings = auth.exchange()?;
    let payload = settings
        .verifying_secrets()
        .iter()
        .find_map(|secret| hs256::verified_payload(token, secret))?;
    let claims: Claims = serde_json::from_slice(&payload).ok()?;
    if claims.shunt_token_use != MARKER_VALUE
        || claims.iss != EXCHANGE_ISSUER
        || claims.aud != EXCHANGE_AUDIENCE
        || claims.exp <= now
    {
        return None;
    }
    auth.jwt()
        .iter()
        .filter(|rule| rule.exchange)
        .any(|rule| fingerprint(rule) == claims.ent)
        .then_some(claims.sub)
}

/// Whether `value` is *shaped* like an exchange token: three segments whose
/// payload carries `shunt_token_use == "workload-exchange"`, judged without
/// verifying the signature or expiry. For do-not-forward decisions, which ask
/// "did shunt issue this?" and must hold for expired, rotated-out and orphaned
/// tokens. Never panics.
pub(crate) fn is_exchange_shaped(value: &[u8]) -> bool {
    let Ok(token) = std::str::from_utf8(value) else {
        return false;
    };
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .ok()
        .and_then(|value| {
            value
                .get(MARKER_CLAIM)
                .and_then(serde_json::Value::as_str)
                .map(|marker| marker == MARKER_VALUE)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
