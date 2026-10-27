//! Verify-only inbound JWT credentials (`[[server.auth.jwt]]`, issue #344).
//!
//! shunt accepts a JWT minted by an external identity provider, validates it
//! against that issuer's JWKS, and maps the verified claims to a caller
//! identity. There is no issuance here: no login flow, no session store, no
//! signing secret. That is the whole point — a client keeps talking to shunt
//! with `ANTHROPIC_BASE_URL` plus a bearer token, so it never enters Claude
//! Code's gateway provider mode and never pays the feature loss a gateway
//! login costs.
//!
//! Distinct from [`crate::gateway::jwt`], which mints *and* verifies shunt's
//! own symmetric HS256 session token. Here shunt only ever verifies, and the
//! signature is asymmetric, so the checks a symmetric token gets for free
//! (there is one key, and shunt owns it) all have to be made explicit:
//! algorithms come from config and the token header's `alg` is never honored,
//! `kid` is required, and an unknown `kid` refetches at most once per window.

use std::collections::HashSet;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;

mod jwks;
pub use jwks::{JwksCache, JwksUnavailable};

/// Cap on the resolved caller identity. The identity namespaces the account
/// pool's sticky key (`codex_endpoint`) and is logged per request, so it must
/// not be an unbounded caller-controlled string — the failure mode #296 records
/// for the inbound Codex `model` label. An address over the cap is rejected,
/// never truncated: truncation would fold two distinct addresses that share a
/// prefix into one identity, and so into one sticky key.
const MAX_IDENTITY_BYTES: usize = 256;

/// One resolved `[[server.auth.jwt]]` entry. Config only: it carries no cache
/// and no client, so a hot reload can swap the whole set without disturbing the
/// [`JwksCache`], which lives for the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtIssuerRule {
    /// Exact `iss` match, exactly as configured (trimmed of whitespace only): an
    /// IdP whose `iss` carries a trailing slash must keep it to be selected.
    pub issuer: String,
    /// Explicit JWKS endpoint. `None` ⇒ derive it from the issuer's discovery
    /// document on first use.
    pub jwks_url: Option<String>,
    /// Accepted `aud` values. Non-empty (config validation).
    pub audience: Vec<String>,
    /// Accepted signing algorithms, pinned from config. Asymmetric only.
    pub algorithms: Vec<Algorithm>,
    /// Accepted `azp` values when the claim is present. Defaults to
    /// [`Self::audience`] at config resolution, so it is never empty.
    pub authorized_parties: Vec<String>,
    /// Lowercase domain parts, matched after the final `@`.
    pub allowed_domains: Vec<String>,
    /// Lowercase full addresses.
    pub allowed_emails: Vec<String>,
    /// Claim name → accepted values, from `require`. Each claim must be a
    /// top-level JSON string equal to one of its values.
    pub require: Vec<(String, Vec<String>)>,
    /// Accepted `sub` prefixes; empty ⇒ `sub` is not checked.
    pub subject_prefixes: Vec<String>,
    /// The claim whose string value becomes the caller identity. `"email"` when
    /// the entry has an email rule and names nothing else.
    pub identity_claim: String,
    /// Tolerance applied to `exp` and `nbf`.
    pub clock_skew_seconds: u64,
    /// Reject when `exp - iat` exceeds this. shunt keeps no revocation state,
    /// so this is what bounds how long a revoked identity keeps working.
    pub max_token_age_seconds: u64,
}

/// What a JWT credential resolved to. The three arms are distinct on purpose:
/// a token that fails every check is the caller's problem (`401`), while an
/// issuer whose JWKS cannot be reached is shunt's (`503`). Collapsing the
/// second into the first would report an IdP outage as a bad credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtOutcome {
    /// Verified against one entry; carries the resolved caller identity.
    Verified { identity: String, issuer: String },
    /// No JWT credential was presented, or it verified against no entry.
    Rejected,
    /// A matching entry's JWKS could not be fetched, so no verdict is possible.
    Unavailable,
}

/// Reject an endpoint shunt should not fetch from. Mirrors
/// `crate::gateway::idp_client::validate_endpoint`: HTTPS, except on loopback.
pub(crate) fn validate_endpoint(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|error| format!("not a valid URL: {error}"))?;
    let safe_transport = url.scheme() == "https"
        || url.scheme() == "http"
            && crate::config::host_is_loopback(url.host_str().unwrap_or_default());
    if !safe_transport
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "must be an https URL (http is allowed only on loopback) with no userinfo or fragment"
                .to_string(),
        );
    }
    Ok(url)
}

#[derive(Deserialize)]
struct VerifiedClaims {
    exp: u64,
    iat: u64,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

impl VerifiedClaims {
    fn string(&self, name: &str) -> Option<&str> {
        self.rest.get(name)?.as_str()
    }

    fn verified_email(&self) -> Option<&str> {
        if self.rest.get("email_verified") != Some(&Value::Bool(true)) {
            return None;
        }
        self.string("email")
    }
}

/// Verify a presented bearer token against the configured entries.
///
/// Entry selection routes on the token's *unverified* `iss`, which is safe
/// because the selected entry's JWKS is then authoritative: claiming another
/// issuer only picks a key set the token cannot satisfy, and
/// [`Validation::set_issuer`] re-checks `iss` against the verified payload
/// before anything is accepted. Selection collects *every* matching entry
/// rather than the first, because one issuer with several audiences is a normal
/// configuration.
pub async fn verify(rules: &[JwtIssuerRule], cache: &JwksCache, token: &str) -> JwtOutcome {
    if rules.is_empty() {
        return JwtOutcome::Rejected;
    }
    let Ok(header) = decode_header(token) else {
        return JwtOutcome::Rejected;
    };
    // Required, never guessed: trying every key in the set would let a caller
    // fish for a key that happens to validate a crafted token.
    let Some(kid) = header.kid else {
        return JwtOutcome::Rejected;
    };
    let Some(issuer) = unverified_issuer(token) else {
        return JwtOutcome::Rejected;
    };

    let mut unavailable = false;
    for rule in rules.iter().filter(|rule| rule.issuer == issuer) {
        let jwk = match cache.key_for(rule, &kid).await {
            Ok(Some(jwk)) => jwk,
            Ok(None) => continue,
            Err(JwksUnavailable) => {
                unavailable = true;
                continue;
            }
        };
        let Ok(key) = DecodingKey::from_jwk(&jwk) else {
            continue;
        };
        let Some(identity) = validate_claims(rule, token, &key) else {
            continue;
        };
        return JwtOutcome::Verified {
            identity,
            issuer: rule.issuer.clone(),
        };
    }

    // An outage wins over a rejection whenever any matching entry was
    // unreachable and none verified: the unreachable entry might have accepted
    // the token (duplicate issuer entries with different audiences, say), so
    // `401` would claim a verdict nobody reached.
    if unavailable {
        JwtOutcome::Unavailable
    } else {
        JwtOutcome::Rejected
    }
}

/// Signature and claim checks for one entry, returning the validated caller
/// identity. `None` on any failure — the caller collapses every rejection into
/// one `401`, so no reason is returned.
fn validate_claims(rule: &JwtIssuerRule, token: &str, key: &DecodingKey) -> Option<String> {
    // `Validation::algorithms` is the pin: jsonwebtoken rejects a token whose
    // header `alg` is not in this list, so the header can never select the
    // algorithm. Config validation additionally refuses HMAC entries, which is
    // what stops a published JWKS key from being replayed as an HMAC secret.
    let mut validation = Validation::new(*rule.algorithms.first()?);
    validation.algorithms = rule.algorithms.clone();
    validation.set_issuer(&[rule.issuer.as_str()]);
    validation.set_audience(&rule.audience);
    validation.leeway = rule.clock_skew_seconds;
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.required_spec_claims =
        HashSet::from(["exp".to_string(), "iss".to_string(), "aud".to_string()]);

    let claims = decode::<VerifiedClaims>(token, key, &validation)
        .ok()?
        .claims;

    // shunt holds no revocation state, so a long-lived token is a long-lived
    // grant. `exp < iat` is nonsense rather than a zero-age token.
    if claims.exp < claims.iat || claims.exp - claims.iat > rule.max_token_age_seconds {
        return None;
    }
    // `azp` names the party the token was issued *to*. Checked only when
    // present, per OIDC Core, but never ignored when it is. A JSON `null` counts
    // as absent, as it did when `azp` deserialized into `Option<String>`.
    if let Some(azp) = claims.rest.get("azp").filter(|azp| !azp.is_null()) {
        let authorized = azp
            .as_str()
            .is_some_and(|azp| rule.authorized_parties.iter().any(|party| party == azp));
        if !authorized {
            return None;
        }
    }
    let identity = claims.string(&rule.identity_claim)?;
    if identity.is_empty() || identity.len() > MAX_IDENTITY_BYTES {
        return None;
    }
    if !email_rules_pass(rule, &claims) {
        return None;
    }
    let required = rule.require.iter().all(|(name, values)| {
        claims
            .string(name)
            .is_some_and(|claim| values.iter().any(|value| value == claim))
    });
    if !required {
        return None;
    }
    if !rule.subject_prefixes.is_empty() {
        let sub = claims.string("sub")?;
        if !rule
            .subject_prefixes
            .iter()
            .any(|prefix| sub.starts_with(prefix.as_str()))
        {
            return None;
        }
    }
    Some(identity.to_string())
}

/// Whether the entry's email-dependent checks pass. Any entry that reads
/// `email` — an email rule, `identity_claim = "email"`, or `require` naming it —
/// trusts only a verified address.
fn email_rules_pass(rule: &JwtIssuerRule, claims: &VerifiedClaims) -> bool {
    let has_email_rule = !rule.allowed_domains.is_empty() || !rule.allowed_emails.is_empty();
    let reads_email = has_email_rule
        || rule.identity_claim == "email"
        || rule.require.iter().any(|(name, _)| name == "email");
    if !reads_email {
        return true;
    }
    claims.verified_email().is_some_and(|email| {
        !has_email_rule
            || crate::gateway::email_allowed(email, &rule.allowed_emails, &rule.allowed_domains)
    })
}

/// Whether `value` — one raw header-slot value — is a JWT naming one of
/// `rules`' issuers, judged by its unverified `iss` alone.
///
/// This is the strip-side mirror of [`verify`], which selects a rule by the
/// same claim before it checks anything else: every token `verify` could
/// accept passes this, so a forward site that strips on it never relays a
/// credential the gate would have authenticated. It deliberately does not
/// verify. An expired token, or one whose key rotated out of the issuer's set,
/// is still an identity token from the operator's IdP, and relaying it to a
/// third-party upstream leaks the caller's identity just the same — the
/// reasoning `GatewayAuth::is_shunt_shaped_token` applies to shunt's own JWT.
/// A JWT naming an issuer no entry configures is left alone: it may be the
/// caller's own upstream credential.
pub(crate) fn names_configured_issuer(rules: &[JwtIssuerRule], value: &[u8]) -> bool {
    if rules.is_empty() {
        return false;
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|token| unverified_issuer(token.trim()))
        .is_some_and(|issuer| rules.iter().any(|rule| rule.issuer == issuer))
}

/// The `iss` claim read without verifying the signature — for entry selection
/// only. See [`verify`] for why that is safe.
fn unverified_issuer(token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Unverified {
        iss: String,
    }
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Unverified>(&bytes)
        .ok()
        .map(|claims| claims.iss)
}

#[cfg(test)]
mod tests;
