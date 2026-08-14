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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::StreamExt;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// The same 10s budget `crate::gateway::idp_client` gives discovery, token, and
/// userinfo requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Floor between two JWKS fetches for one issuer. An unknown `kid` triggers at
/// most one refetch per window, so a caller cannot use forged `kid` values to
/// make shunt hammer the issuer's JWKS endpoint.
const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(60);

/// Cap on a JWKS (or discovery) document. Both are fetched from a configured,
/// operator-chosen origin, so this is a runaway guard rather than a defence
/// against a hostile peer.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// Cap on the resolved caller identity. The identity namespaces the account
/// pool's sticky key (`codex_endpoint`) and is logged per request, so it must
/// not be an unbounded caller-controlled string — the failure mode #296 records
/// for the inbound Codex `model` label.
const MAX_IDENTITY_BYTES: usize = 256;

/// One resolved `[[server.auth.jwt]]` entry. Config only: it carries no cache
/// and no client, so a hot reload can swap the whole set without disturbing the
/// [`JwksCache`], which lives for the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtIssuerRule {
    /// Exact `iss` match, normalized at config resolution.
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

/// Process-lifetime, per-issuer JWKS state. Held on `AppState` alongside
/// `admin_stores` / `gateway_stores` rather than on the hot-reloadable
/// `InboundAuth`, so a config reload that leaves an entry unchanged does not
/// throw its keys away and refetch.
///
/// The outer `Mutex` is held only long enough to look up an issuer's entry; the
/// per-issuer `tokio::sync::Mutex` is held across the network fetch, so
/// concurrent requests for one issuer collapse into a single fetch while a
/// different issuer proceeds untouched. That is the isolation the design
/// requires: one issuer's outage must not deny the others.
pub struct JwksCache {
    client: reqwest::Client,
    issuers: Mutex<HashMap<String, Arc<tokio::sync::Mutex<IssuerState>>>>,
}

#[derive(Default)]
struct IssuerState {
    /// Resolved once per issuer, from config or discovery.
    jwks_url: Option<String>,
    keys: Option<Arc<JwkSet>>,
    /// When a fetch was last *attempted*, successful or not, so a failing
    /// issuer is rate-limited exactly like a succeeding one.
    last_fetch: Option<Instant>,
}

/// A JWKS could not be produced. Deliberately opaque: the reason is logged, not
/// returned, so a caller cannot probe an issuer's reachability through response
/// differences.
#[derive(Debug)]
pub struct JwksUnavailable;

impl Default for JwksCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JwksCache {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("JWKS HTTP client configuration is valid"),
            issuers: Mutex::new(HashMap::new()),
        }
    }

    /// The key for `kid`, fetching or refetching this issuer's JWKS as needed.
    ///
    /// `Ok(None)` means "this issuer has usable keys and none of them is `kid`"
    /// — a `401`. `Err` means "no usable keys at all" — a `503`.
    async fn key_for(
        &self,
        rule: &JwtIssuerRule,
        kid: &str,
    ) -> Result<Option<Jwk>, JwksUnavailable> {
        let entry = {
            let mut issuers = self.issuers.lock().expect("inbound JWKS lock poisoned");
            issuers.entry(rule.issuer.clone()).or_default().clone()
        };
        let mut state = entry.lock().await;

        if let Some(jwk) = state.keys.as_ref().and_then(|keys| keys.find(kid)) {
            return Ok(Some(jwk.clone()));
        }

        let due = state
            .last_fetch
            .is_none_or(|at| at.elapsed() >= MIN_REFETCH_INTERVAL);
        if !due {
            // Inside the refetch floor. With keys cached this is simply an
            // unknown `kid`; with none cached shunt still cannot verify
            // anything for this issuer, and answering `401` would misreport a
            // continuing outage as a bad credential.
            return if state.keys.is_some() {
                Ok(None)
            } else {
                Err(JwksUnavailable)
            };
        }
        state.last_fetch = Some(Instant::now());

        let url = match &state.jwks_url {
            Some(url) => url.clone(),
            None => {
                let url = self.resolve_jwks_url(rule).await?;
                state.jwks_url = Some(url.clone());
                url
            }
        };
        match self.fetch_jwks(&url).await {
            Ok(keys) => {
                let keys = Arc::new(keys);
                state.keys = Some(keys.clone());
                Ok(keys.find(kid).cloned())
            }
            Err(error) => {
                tracing::warn!(
                    issuer = %rule.issuer,
                    error = %error,
                    "inbound JWT: JWKS fetch failed"
                );
                // A previously-fetched key set is still the best available
                // answer; only a cold cache is an outage from the caller's
                // point of view.
                if state.keys.is_some() {
                    Ok(None)
                } else {
                    Err(JwksUnavailable)
                }
            }
        }
    }

    /// The configured `jwks_url`, or the `jwks_uri` from the issuer's discovery
    /// document. Both go through [`validate_endpoint`].
    async fn resolve_jwks_url(&self, rule: &JwtIssuerRule) -> Result<String, JwksUnavailable> {
        if let Some(url) = &rule.jwks_url {
            return Ok(url.clone());
        }
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            rule.issuer.trim_end_matches('/')
        );
        let document: DiscoveryDocument =
            self.fetch_json(&discovery_url).await.map_err(|error| {
                tracing::warn!(
                    issuer = %rule.issuer,
                    error = %error,
                    "inbound JWT: OIDC discovery failed"
                );
                JwksUnavailable
            })?;
        if document.issuer.trim_end_matches('/') != rule.issuer.trim_end_matches('/') {
            tracing::warn!(
                issuer = %rule.issuer,
                "inbound JWT: discovery document issuer does not match the configured issuer"
            );
            return Err(JwksUnavailable);
        }
        validate_endpoint(&document.jwks_uri).map_err(|message| {
            tracing::warn!(issuer = %rule.issuer, %message, "inbound JWT: discovered jwks_uri rejected");
            JwksUnavailable
        })?;
        Ok(document.jwks_uri)
    }

    async fn fetch_jwks(&self, url: &str) -> Result<JwkSet, String> {
        self.fetch_json(url).await
    }

    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let response = self
            .client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("returned HTTP {status}"));
        }
        let body = read_bounded(response).await?;
        serde_json::from_slice(&body).map_err(|error| format!("invalid JSON: {error}"))
    }
}

/// Only the two fields the JWKS path needs. `crate::gateway::idp_client`'s
/// `DiscoveredEndpoints` requires the authorization/token/userinfo endpoints,
/// which a verify-only deployment's issuer has no reason to serve.
#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

/// Read at most [`MAX_DOCUMENT_BYTES`], failing rather than truncating: a
/// truncated JWKS would parse as "this issuer has fewer keys than it does" and
/// silently reject tokens signed with the ones that were cut off.
async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("response stream failed: {error}"))?;
        if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(format!("response exceeds {MAX_DOCUMENT_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

/// The claims Phase 1 reads. Every field the verification depends on is
/// non-optional, so a token missing one fails to deserialize rather than
/// reaching a check that would have to invent a default: an absent `iat` would
/// otherwise skip the `max_token_age_seconds` bound entirely.
#[derive(Deserialize)]
struct VerifiedClaims {
    exp: u64,
    iat: u64,
    email: String,
    email_verified: bool,
    #[serde(default)]
    azp: Option<String>,
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
        let Some(claims) = validate_claims(rule, token, &key) else {
            continue;
        };
        return JwtOutcome::Verified {
            identity: bounded_identity(&claims.email),
            issuer: rule.issuer.clone(),
        };
    }

    // Only report an outage when no entry reached a verdict. A token that a
    // reachable entry rejected is a `401` even if some other entry for the same
    // issuer happened to be unreachable.
    if unavailable {
        JwtOutcome::Unavailable
    } else {
        JwtOutcome::Rejected
    }
}

/// Signature and claim checks for one entry. `None` on any failure — the caller
/// collapses every rejection into one `401`, so no reason is returned.
fn validate_claims(rule: &JwtIssuerRule, token: &str, key: &DecodingKey) -> Option<VerifiedClaims> {
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
    // present, per OIDC Core, but never ignored when it is.
    if let Some(azp) = &claims.azp {
        if !rule.authorized_parties.iter().any(|party| party == azp) {
            return None;
        }
    }
    // Phase 1 authorizes on email alone, so an unverified address is worthless:
    // an IdP that lets a user set an arbitrary unverified email would otherwise
    // let them claim any address in an allowed domain.
    if !claims.email_verified {
        return None;
    }
    if !crate::gateway::email_allowed(&claims.email, &rule.allowed_emails, &rule.allowed_domains) {
        return None;
    }
    Some(claims)
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

/// Truncate on a char boundary so the identity stays valid UTF-8. See
/// [`MAX_IDENTITY_BYTES`] for why it is bounded at all.
fn bounded_identity(email: &str) -> String {
    if email.len() <= MAX_IDENTITY_BYTES {
        return email.to_string();
    }
    let mut end = MAX_IDENTITY_BYTES;
    while end > 0 && !email.is_char_boundary(end) {
        end -= 1;
    }
    email[..end].to_string()
}

#[cfg(test)]
mod tests;
